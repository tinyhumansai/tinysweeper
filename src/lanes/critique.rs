//! The `critique` lane: correctness of the diff.
//!
//! The first lane, and the template for the rest. Its shape is the point:
//!
//! 1. Decide whether there is anything to review at all, before spending a
//!    token.
//! 2. Build the prompt in cache-friendly layers.
//! 3. Ask for structured output; refuse to parse prose.
//! 4. Place every finding by the code it quoted, not by a number it guessed.
//! 5. Drop the findings the diff disproves.
//!
//! Steps 4 and 5 are the noise control, and they pull in opposite directions on
//! purpose. Step 4 (`src/position`) exists because the old rule — the model
//! emits a line number, and anything outside the diff is dropped — threw away
//! good findings for bad arithmetic. Step 5 (`src/falsify`) exists because
//! keeping more findings is only an improvement if the wrong ones still go, and
//! it removes only what the diff *disproves*, never what it merely cannot
//! confirm.
//!
//! A finding that cannot be placed is no longer dropped. It loses its line and
//! is rendered into the check-run summary instead of posted inline, which is
//! the honest outcome: the review found something and could not say exactly
//! where.

use std::sync::Arc;

use async_trait::async_trait;

use crate::config::types::LaneId;
use crate::error::Result;
use crate::evidence::diff::FileDiff;
use crate::evidence::replay;
use crate::falsify::Falsifier;
use crate::harness::prompt::{self, PromptInputs};
use crate::harness::schema::{self, RawFinding};
use crate::lanes::{Lane, LaneInput, LaneOutcome};
use crate::ports::model::{Message, Model, ModelRequest, Spend};
use crate::position::{PositionRequest, Positioner, Resolution, Unanchored};

/// The correctness lane.
pub struct Critique {
    model: Arc<dyn Model>,
}

impl Critique {
    /// Build the lane over `model`.
    pub fn new(model: Arc<dyn Model>) -> Self {
        Self { model }
    }
}

#[async_trait]
impl Lane for Critique {
    fn id(&self) -> LaneId {
        LaneId::Critique
    }

    async fn run(&self, input: LaneInput<'_>) -> Result<LaneOutcome> {
        // Cheapest possible check first. A pull request that only deletes
        // files, or only touches ignored paths, has nothing for this lane, and
        // asking a model to confirm that is money for a foregone conclusion.
        if !input.has_reviewable_content() {
            return Ok(LaneOutcome::skipped(
                "No added or modified lines to review.",
            ));
        }

        if input.pull_request.draft && !input.config.review.draft_prs {
            return Ok(LaneOutcome::skipped(
                "Draft pull request; set `review.draft_prs = true` to review drafts.",
            ));
        }

        // Put the initial complete diff in the same serialized prefix where a
        // later run replays it. Providers cache byte-identical prefixes; using
        // the user-message position on the first run would make that first
        // evidence miss the cache again on the next run.
        let evidence = replay::render(input.diffs);
        let (reviewed_evidence, new_evidence) = if input.reviewed_evidence.is_empty() {
            (evidence.clone(), String::new())
        } else {
            replay::split(input.reviewed_evidence, &evidence)
        };
        let changed_paths = input.changed_paths();
        let built = prompt::build(&PromptInputs {
            repo_policy: input.repo_policy,
            reviewed_evidence: &reviewed_evidence,
            prior_findings: input.prior_findings,
            new_evidence: &new_evidence,
            changed_paths: &changed_paths,
            ..PromptInputs::new(LaneId::Critique, input.config)
        });

        // The prefix goes in the system message and the suffix in the user
        // message. Providers cache on the serialised prefix, and a system
        // message is the one part guaranteed to be sent first.
        let response = self
            .model
            .complete(ModelRequest {
                model: input.config.model_for(LaneId::Critique).to_string(),
                messages: vec![
                    Message::system(built.prefix()),
                    Message::user(built.suffix()),
                ],
                schema: schema::json_schema(),
                schema_name: "tinysweeper_critique".into(),
                max_tokens: input.config.models.max_tokens,
            })
            .await?;

        // Taken before the value is moved out: the spend belongs to the model
        // that answered, whether or not its answer parses.
        let spend = Spend::of(&response);
        let parsed = schema::parse(LaneId::Critique, response.value)?;

        let mut spend = spend;
        let positioner = Positioner::new(self.model.as_ref(), input.config);

        let mut findings = Vec::new();
        let mut unanchored = 0usize;
        let mut discarded = 0usize;

        for raw in parsed.findings {
            // A file this pull request never touched is still dropped outright.
            // There is nothing to anchor against and nothing this author did.
            let Some(diff) = input.diffs.iter().find(|d| d.path == raw.path) else {
                discarded += 1;
                continue;
            };

            // Budget check: relocation can make one model call per unresolvable
            // finding, so enforce the limit inside the loop before escalating to
            // stage 3. Do not wait until the lane finishes.
            if spend.cost_usd() > input.config.models.budget_usd_per_pr {
                return Err(crate::error::Error::Budget {
                    spent: spend.cost_usd(),
                    limit: input.config.models.budget_usd_per_pr,
                });
            }

            let comment = format!("{}\n\n{}", raw.title, raw.body);
            let snippet = raw.existing_code.clone().unwrap_or_default();
            let resolution = positioner
                .resolve(
                    PositionRequest {
                        snippet: &snippet,
                        diff: Some(diff),
                        file: input.file_contents.get(&raw.path).map(String::as_str),
                        comment: &comment,
                        // Relocation and falsification need the complete
                        // current evidence even when that evidence was placed
                        // in the cacheable prompt prefix on an initial run.
                        rendered_diff: &evidence,
                    },
                    &mut spend,
                )
                .await;

            let range = postable_range(&raw, diff, resolution);
            if range.is_none() {
                unanchored += 1;
            }

            let mut finding = raw.into_finding(LaneId::Critique);
            finding.line = range.map(|(start, _)| start);
            finding.end_line = range.and_then(|(start, end)| (end > start).then_some(end));

            // Postability is wider than the changed-line set, so a finding can
            // now land on a context line the pull request never touched. That
            // is a deliberate widening, but it must not be a silent one: the
            // noise rule is "introduced by this pull request", and a reader has
            // to be able to tell when a finding is not. Marking it `late` is
            // what puts the pre-existing badge on it in the summary.
            if let Some((start, end)) = range
                && !diff.touches_range(start, end)
            {
                finding.late = true;
            }

            findings.push(finding);
        }

        // Step 5, on the findings that survived positioning. It sees only the
        // diff, and it can only remove.
        let filtered = Falsifier::new(self.model.as_ref(), input.config)
            .filter(LaneId::Critique, findings, &evidence)
            .await;
        spend.merge(filtered.spend);

        Ok(LaneOutcome {
            summary: summarise(
                parsed.summary.trim(),
                unanchored,
                discarded,
                filtered.rejected.len(),
            ),
            findings: filtered.findings,
            resolved: parsed.resolved,
            spend,
            skipped: None,
        })
    }
}

/// The head-revision range a finding may be posted against, if any.
///
/// One rule: the range has to be inside a hunk, because that is exactly what
/// GitHub will accept an inline comment on. That is deliberately wider than
/// *the lines this pull request changed* — a finding that quotes a context
/// line inside the hunk is about the change too, and the quotation is evidence
/// the model really did read that line rather than guess at it. It is also
/// deliberately narrower than *anywhere in the file*: a finding that resolved
/// through the whole-file fallback to code the diff never showed cannot be
/// posted inline, so it goes in the summary rather than being thrown away.
fn postable_range(raw: &RawFinding, diff: &FileDiff, resolution: Resolution) -> Option<(u64, u64)> {
    let (start, end) = match resolution {
        Resolution::Anchored(anchor) => (anchor.start, anchor.end),
        // The migration path: a model still answering with the old schema gets
        // its line number honoured, because there is no quotation to place and
        // its number is better than nothing.
        Resolution::Unanchored(Unanchored::NoSnippet) => {
            let line = raw.line?;
            (line, raw.end_line.unwrap_or(line))
        }
        Resolution::Unanchored(Unanchored::NoMatch) => return None,
    };

    diff.within_hunk(start, end).then_some((start, end))
}

/// Fold the bookkeeping into the model's own summary.
///
/// Every count here is a finding that did not become an inline comment. They
/// are stated rather than hidden: a filter nobody can see the effect of is a
/// filter nobody can tell is broken.
fn summarise(summary: &str, unanchored: usize, discarded: usize, rejected: usize) -> String {
    let mut notes = Vec::new();
    if unanchored > 0 {
        notes.push(format!(
            "{unanchored} finding{} could not be anchored to a line",
            plural(unanchored)
        ));
    }
    if discarded > 0 {
        notes.push(format!(
            "{discarded} finding{} discarded for naming a file this pull request did not change",
            plural(discarded)
        ));
    }
    if rejected > 0 {
        notes.push(format!(
            "{rejected} finding{} dropped as disproved by the diff",
            plural(rejected)
        ));
    }

    if notes.is_empty() {
        return summary.to_string();
    }
    format!("{summary} ({})", notes.join("; "))
}

fn plural(count: usize) -> &'static str {
    if count == 1 { "" } else { "s" }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::types::{Config, Severity};
    use crate::evidence::diff::parse_file_patch;
    use crate::forge::types::PullRequest;
    use crate::harness::mock::MockModel;
    use serde_json::json;
    use std::collections::BTreeMap;

    fn config() -> Config {
        crate::config::DEFAULTS
            .parse::<toml::Table>()
            .unwrap()
            .try_into()
            .unwrap()
    }

    const PATCH: &str =
        "@@ -1,3 +1,5 @@\n fn main() {\n+    let x = items[i];\n+    println!(\"{x}\");\n }\n";

    /// The head revision of the same file. Lines 6–8 are outside every hunk,
    /// so only the whole-file fallback can reach them.
    const FILE: &str = "\
fn main() {
    let x = items[i];
    println!(\"{x}\");
}

fn helper() {
    let cfg = load();
}
";

    fn diffs() -> Vec<FileDiff> {
        vec![parse_file_patch("src/main.rs", PATCH)]
    }

    fn pull_request() -> PullRequest {
        PullRequest {
            number: 7,
            title: "feat: index items".into(),
            head_sha: "abc123".into(),
            ..PullRequest::default()
        }
    }

    async fn run_with(model: MockModel, config: &Config, diffs: &[FileDiff]) -> LaneOutcome {
        run_with_files(model, config, diffs, &BTreeMap::new()).await
    }

    async fn run_with_files(
        model: MockModel,
        config: &Config,
        diffs: &[FileDiff],
        file_contents: &BTreeMap<String, String>,
    ) -> LaneOutcome {
        let pr = pull_request();
        Critique::new(Arc::new(model))
            .run(LaneInput {
                config,
                pull_request: &pr,
                diffs,
                file_contents,
                scan_findings: &[],
                commits: &[],
                repo_policy: None,
                reviewed_evidence: "",
                prior_findings: &[],
            })
            .await
            .expect("lane runs")
    }

    /// A finding anchored the way the schema now asks for: by quotation.
    fn finding_quoting(snippet: &str) -> serde_json::Value {
        json!({
            "path": "src/main.rs",
            "existing_code": snippet,
            "rule": "unchecked-index",
            "title": "Guard the index before dereferencing",
            "body": "`i` is never bounds-checked.",
            "severity": "high",
            "confidence": 0.9
        })
    }

    /// A finding in the pre-positioning shape, still accepted so a proposal
    /// written by an older version keeps working.
    fn finding_at(line: u64) -> serde_json::Value {
        json!({
            "path": "src/main.rs",
            "line": line,
            "rule": "unchecked-index",
            "title": "Guard the index before dereferencing",
            "body": "`i` is never bounds-checked.",
            "severity": "high",
            "confidence": 0.9
        })
    }

    #[tokio::test]
    async fn a_finding_on_a_changed_line_survives() {
        let model = MockModel::new().then(json!({
            "summary": "Adds an unchecked index.",
            "findings": [finding_at(2)]
        }));
        let outcome = run_with(model, &config(), &diffs()).await;

        assert_eq!(outcome.findings.len(), 1);
        assert_eq!(outcome.findings[0].severity, Severity::High);
        assert_eq!(outcome.findings[0].lane, LaneId::Critique);
    }

    #[tokio::test]
    async fn a_quoted_snippet_is_what_places_the_finding() {
        // The model quotes the code with the indentation it felt like using and
        // never names a line. Line 2 is where that code actually is.
        let model = MockModel::new().then(json!({
            "summary": "Adds an unchecked index.",
            "findings": [finding_quoting("let x = items[i];")]
        }));
        let outcome = run_with(model, &config(), &diffs()).await;

        assert_eq!(outcome.findings.len(), 1);
        assert_eq!(outcome.findings[0].line, Some(2));
    }

    #[tokio::test]
    async fn a_leaked_diff_marker_in_the_quote_does_not_lose_the_finding() {
        let model = MockModel::new().then(json!({
            "summary": "…",
            "findings": [finding_quoting("+    let x = items[i];")]
        }));
        let outcome = run_with(model, &config(), &diffs()).await;

        assert_eq!(outcome.findings[0].line, Some(2));
    }

    #[tokio::test]
    async fn a_multi_line_quote_becomes_a_range() {
        let model = MockModel::new().then(json!({
            "summary": "…",
            "findings": [finding_quoting("let x = items[i];\n\nprintln!(\"{x}\");")]
        }));
        let outcome = run_with(model, &config(), &diffs()).await;

        assert_eq!(outcome.findings[0].line, Some(2));
        assert_eq!(outcome.findings[0].end_line, Some(3));
    }

    #[tokio::test]
    async fn a_finding_that_resolves_outside_every_hunk_survives_without_a_line() {
        // Real finding, quoted from real code, but the diff never showed that
        // code — GitHub would reject the inline comment. It goes in the summary
        // rather than being deleted, which is what the old line-number filter
        // did to it.
        let mut files = BTreeMap::new();
        files.insert("src/main.rs".to_string(), FILE.to_string());
        let model = MockModel::new().then(json!({
            "summary": "…",
            "findings": [finding_quoting("    let cfg = load();")]
        }));

        let outcome = run_with_files(model, &config(), &diffs(), &files).await;

        assert_eq!(outcome.findings.len(), 1, "not deleted");
        assert_eq!(outcome.findings[0].line, None, "not postable inline");
        assert!(
            outcome.summary.contains("1 finding could not be anchored"),
            "{}",
            outcome.summary
        );
    }

    #[tokio::test]
    async fn a_quote_that_matches_nothing_leaves_the_finding_unanchored() {
        let model = MockModel::new().then(json!({
            "summary": "…",
            "findings": [finding_quoting("let y = somewhere_else();")]
        }));
        let outcome = run_with(model, &config(), &diffs()).await;

        assert_eq!(outcome.findings.len(), 1);
        assert_eq!(outcome.findings[0].line, None);
    }

    #[tokio::test]
    async fn a_hopeless_quote_is_recovered_by_the_relocation_call() {
        let model = MockModel::new()
            .then(json!({
                "summary": "…",
                "findings": [finding_quoting("the loop that indexes without checking")]
            }))
            .then(json!({"existing_code": "    let x = items[i];"}))
            .then(json!({"incorrect": []}));

        let outcome = run_with(model, &config(), &diffs()).await;

        assert_eq!(outcome.findings[0].line, Some(2));
    }

    #[tokio::test]
    async fn the_falsification_pass_drops_what_the_diff_disproves() {
        let model = MockModel::new()
            .then(json!({
                "summary": "…",
                "findings": [finding_quoting("let x = items[i];")]
            }))
            .then(json!({
                "incorrect": [{"index": 1, "reason": "the diff bounds-checks `i` above"}]
            }));

        let outcome = run_with(model, &config(), &diffs()).await;

        assert!(outcome.findings.is_empty());
        assert!(
            outcome.summary.contains("1 finding dropped as disproved"),
            "{}",
            outcome.summary
        );
    }

    #[tokio::test]
    async fn a_broken_falsification_pass_never_deletes_a_review() {
        let model = MockModel::new()
            .then(json!({
                "summary": "…",
                "findings": [finding_quoting("let x = items[i];")]
            }))
            .then_error("upstream exploded");

        let outcome = run_with(model, &config(), &diffs()).await;

        assert_eq!(outcome.findings.len(), 1, "failed open");
        assert!(
            !outcome.summary.contains("disproved"),
            "{}",
            outcome.summary
        );
    }

    #[tokio::test]
    async fn a_finding_quoting_a_line_the_model_did_not_change_is_still_postable() {
        // Line 1 is context inside the hunk. The model quoted it, so it read
        // it, and GitHub will take a comment there.
        let model = MockModel::new().then(json!({
            "summary": "…",
            "findings": [finding_quoting("fn main() {")]
        }));
        let outcome = run_with(model, &config(), &diffs()).await;

        assert_eq!(outcome.findings[0].line, Some(1));
    }

    #[tokio::test]
    async fn a_legacy_response_with_a_line_and_no_quote_still_anchors() {
        // Migration: a proposal or a fine-tune still answering with the old
        // schema keeps working, because its number is better than nothing.
        let model = MockModel::new().then(json!({
            "summary": "…",
            "findings": [finding_at(2)]
        }));
        let outcome = run_with(model, &config(), &diffs()).await;

        assert_eq!(outcome.findings[0].line, Some(2));
    }

    #[tokio::test]
    async fn a_legacy_line_outside_every_hunk_is_not_trusted() {
        let model = MockModel::new().then(json!({
            "summary": "…",
            "findings": [finding_at(99)]
        }));
        let outcome = run_with(model, &config(), &diffs()).await;

        assert_eq!(outcome.findings[0].line, None);
    }

    #[tokio::test]
    async fn a_finding_in_a_file_the_pull_request_never_touched_is_discarded() {
        let model = MockModel::new().then(json!({
            "summary": "…",
            "findings": [{
                "path": "src/elsewhere.rs",
                "existing_code": "let x = items[i];",
                "rule": "r", "title": "t", "body": "b",
                "severity": "high", "confidence": 0.9
            }]
        }));
        let outcome = run_with(model, &config(), &diffs()).await;
        assert!(outcome.findings.is_empty());
        assert!(
            outcome.summary.contains("did not change"),
            "{}",
            outcome.summary
        );
    }

    #[tokio::test]
    async fn a_late_finding_may_sit_on_unchanged_lines_of_a_touched_file() {
        let mut late = finding_at(1);
        late["late"] = json!(true);
        let model = MockModel::new().then(json!({"summary": "…", "findings": [late]}));

        let outcome = run_with(model, &config(), &diffs()).await;
        assert_eq!(outcome.findings.len(), 1);
        assert!(outcome.findings[0].late);
    }

    #[tokio::test]
    async fn a_finding_quoting_a_context_line_is_marked_pre_existing() {
        // Postability is the hunk, which is wider than the lines this pull
        // request changed. A finding that lands on a context line is therefore
        // about code the author did not touch, and the reader has to be able to
        // tell — the model did not say `late`, the diff did.
        let model = MockModel::new().then(json!({
            "summary": "…",
            "findings": [finding_quoting("fn main() {")]
        }));

        let outcome = run_with(model, &config(), &diffs()).await;

        assert_eq!(outcome.findings.len(), 1);
        assert_eq!(outcome.findings[0].line, Some(1), "the context line");
        assert!(
            outcome.findings[0].late,
            "an untouched line must carry the pre-existing badge"
        );
    }

    #[tokio::test]
    async fn a_finding_quoting_an_added_line_is_not_marked_pre_existing() {
        let model = MockModel::new().then(json!({
            "summary": "…",
            "findings": [finding_quoting("    let x = items[i];")]
        }));

        let outcome = run_with(model, &config(), &diffs()).await;

        assert_eq!(outcome.findings.len(), 1);
        assert!(
            !outcome.findings[0].late,
            "this pull request introduced the line"
        );
    }

    #[tokio::test]
    async fn an_empty_review_is_reported_as_such() {
        let outcome = run_with(MockModel::silent(), &config(), &diffs()).await;

        assert!(outcome.findings.is_empty());
        assert_eq!(outcome.summary, "Nothing to report.");
        assert!(
            outcome.skipped.is_none(),
            "silence is not the same as skipping"
        );
    }

    #[tokio::test]
    async fn a_pull_request_with_nothing_to_review_never_calls_the_model() {
        let model = MockModel::new();
        let outcome = run_with(model.clone(), &config(), &[]).await;

        assert_eq!(model.calls(), 0, "spent money on a foregone conclusion");
        assert!(outcome.skipped.is_some());
    }

    #[tokio::test]
    async fn a_draft_is_skipped_unless_the_repository_opts_in() {
        let config = config();
        let model = MockModel::silent();
        let pr = PullRequest {
            draft: true,
            ..pull_request()
        };
        let diffs = diffs();

        let outcome = Critique::new(Arc::new(model.clone()))
            .run(LaneInput {
                config: &config,
                pull_request: &pr,
                diffs: &diffs,
                file_contents: &BTreeMap::new(),
                scan_findings: &[],
                commits: &[],
                repo_policy: None,
                reviewed_evidence: "",
                prior_findings: &[],
            })
            .await
            .expect("runs");

        assert!(outcome.skipped.is_some());
        assert_eq!(model.calls(), 0);
    }

    #[tokio::test]
    async fn the_prompt_carries_line_numbers_so_anchors_are_read_not_counted() {
        let model = MockModel::silent();
        run_with(model.clone(), &config(), &diffs()).await;

        let prompt = model.last_prompt().expect("recorded");
        assert!(prompt.contains("2 +    let x = items[i];"), "{prompt}");
    }

    #[tokio::test]
    async fn the_configured_tier_is_the_model_actually_called() {
        let mut config = config();
        config.models.deep = "some/deep-model".into();
        let model = MockModel::silent();
        run_with(model.clone(), &config, &diffs()).await;

        assert_eq!(model.requests()[0].model, "some/deep-model");
    }

    #[tokio::test]
    async fn a_re_review_replays_the_earlier_diff_into_the_cacheable_system_message() {
        let config = config();
        let model = MockModel::silent();
        let pr = pull_request();

        // The earlier cycle reviewed `src/earlier.rs`; this push adds
        // `src/main.rs`. The replay has to be the earlier file, byte for byte.
        let earlier = parse_file_patch("src/earlier.rs", "@@ -1,1 +1,2 @@\n a\n+earlier\n");
        let reviewed = replay::render(std::slice::from_ref(&earlier));
        let diffs = vec![earlier, parse_file_patch("src/main.rs", PATCH)];

        Critique::new(Arc::new(model.clone()))
            .run(LaneInput {
                config: &config,
                pull_request: &pr,
                diffs: &diffs,
                file_contents: &BTreeMap::new(),
                scan_findings: &[],
                commits: &[],
                repo_policy: None,
                reviewed_evidence: &reviewed,
                prior_findings: &["Close the socket on the error path".to_string()],
            })
            .await
            .expect("runs");

        let request = &model.requests()[0];
        let system = &request.messages[0].content;
        let user = &request.messages[1].content;

        assert!(
            system.contains("+earlier"),
            "replay must be in the cached half"
        );
        assert!(!user.contains("+earlier"));
        assert!(user.contains("src/main.rs"), "the delta is the new work");
        assert!(
            user.contains("Close the socket"),
            "prior findings are volatile"
        );
        assert!(
            !system.contains("Close the socket"),
            "prior findings must not enter the cached prefix"
        );
    }

    #[tokio::test]
    async fn malformed_model_output_fails_the_lane_rather_than_posting_nonsense() {
        let model = MockModel::new().then(json!({"summary": "…", "findings": [{"path": "x"}]}));
        let pr = pull_request();
        let config = config();
        let diffs = diffs();

        let err = Critique::new(Arc::new(model))
            .run(LaneInput {
                config: &config,
                pull_request: &pr,
                diffs: &diffs,
                file_contents: &BTreeMap::new(),
                scan_findings: &[],
                commits: &[],
                repo_policy: None,
                reviewed_evidence: "",
                prior_findings: &[],
            })
            .await
            .unwrap_err();

        assert!(
            err.to_string().contains("did not match the schema"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn resolved_findings_are_carried_through() {
        let model = MockModel::new().then(json!({
            "summary": "Earlier issue is fixed.",
            "findings": [],
            "resolved": ["Guard the index before dereferencing"]
        }));
        let outcome = run_with(model, &config(), &diffs()).await;

        assert_eq!(
            outcome.resolved,
            vec!["Guard the index before dereferencing"]
        );
    }
}
