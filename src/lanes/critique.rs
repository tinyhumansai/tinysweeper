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
//!
//! The lane fans out **one conversation per changed file**, like `security`.
//! It reviewed the whole pull request in a single call until a 31-file change
//! landed with two real correctness bugs in it and this lane reported one
//! hallucination: the failure `fanout`'s own module doc predicts, where the
//! first few files are read closely and the rest are an afterthought. The
//! subject here is one file's correctness, so nothing is lost by the split —
//! unlike `tests`, whose subject is the relationship *between* files and which
//! is deliberately still one conversation.

use std::sync::Arc;

use async_trait::async_trait;

use crate::config::types::{Config, LaneId};
use crate::council;
use crate::error::Result;
use crate::evidence::diff::FileDiff;
use crate::evidence::replay;
use crate::falsify::{Falsifier, Rejection};
use crate::findings::types::Finding;
use crate::flows::panel::Call;
use crate::flows::runner;
use crate::harness::prompt::{self, PromptInputs};
use crate::harness::schema::{self, RawFinding};
use crate::lanes::fanout::{FileReview, per_file};
use crate::lanes::{Lane, LaneInput, LaneOutcome};
use crate::ports::model::{Model, Spend};
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

        // Files this lane has already reviewed, unchanged since, are skipped
        // outright rather than replayed into a cacheable prefix. Both are
        // sound, but a per-file lane can take the stronger one: skipping pays
        // always, where a cache prefix only pays when the provider honours it.
        // This is what `security` does, and for the same reason.
        let fresh: Vec<&FileDiff> = replay::unreviewed(input.reviewed_evidence, input.diffs)
            .into_iter()
            .filter(|diff| !diff.changed_lines.is_empty())
            .collect();
        let paths: Vec<String> = fresh.iter().map(|diff| diff.path.clone()).collect();

        let changed_paths = input.changed_paths();
        // One capability for the whole lane, so the pull-request budget is
        // enforced across every file and every reviewer at once. That is what
        // lets the files run concurrently: this lane reviewed them one at a
        // time only because spend is known after a call returns, and there was
        // nowhere else to check it.
        let llm = runner::lane_llm(
            self.model.clone(),
            input.config,
            input.config.models.budget_usd_per_pr,
        );

        let outcome = per_file(&paths, |path| {
            let llm = llm.clone();
            let input = &input;
            let changed_paths = &changed_paths;
            async move {
                let diff = input
                    .diffs
                    .iter()
                    .find(|d| d.path == path)
                    .expect("the path came from the diff list");
                review_file(llm, input, changed_paths, diff).await
            }
        })
        .await;

        // The graph's own calls are tallied inside the capability, which is the
        // only object every one of them passes through. Folded in once here
        // rather than per file: `llm` is shared across the fan-out, so adding
        // it per file would multiply the bill by the file count.
        let mut outcome = outcome.into_outcome();
        outcome.spend.merge(llm.spend());
        Ok(outcome)
    }
}

/// Review one file, in a conversation that knows about no other file.
///
/// Positioning (step 4) and falsification (step 5) both run here rather than
/// once over the folded result, because both want *the evidence this
/// conversation was shown* and that is now one file's diff. Falsification is
/// also free for the common file: `Falsifier::filter` makes no call when there
/// is nothing to filter, so the number of falsify calls is the number of files
/// that actually produced a finding.
async fn review_file(
    llm: std::sync::Arc<crate::flows::caps::ModelCapability>,
    input: &LaneInput<'_>,
    changed_paths: &[String],
    diff: &FileDiff,
) -> Result<FileReview> {
    let config: &Config = input.config;
    let evidence = replay::render(std::slice::from_ref(diff));
    let reviewers = council::reviewers(config, LaneId::Critique);

    // Every reviewer at once, as one graph. `ask_all` returns one answer per
    // reviewer in the order asked, and reports a reviewer it could not reach
    // rather than failing the council for it.
    let calls: Vec<Call> = reviewers
        .iter()
        .map(|reviewer| {
            let built = build_prompt(input, changed_paths, diff, &evidence, reviewer);
            Call {
                id: reviewer.id.to_string(),
                model: reviewer.model.to_string(),
                system: built.prefix().to_string(),
                prompt: built.suffix().to_string(),
                schema_name: "tinysweeper_critique".into(),
            }
        })
        .collect();

    let answers = runner::ask_all(
        llm.clone(),
        LaneId::Critique,
        &calls,
        &schema::json_schema(),
        config
            .council
            .subagents
            .then_some(config.models.flash.as_str()),
        input.corpus,
    )
    .await?;

    let mut spend = Spend::default();
    let mut per_reviewer: Vec<Vec<Finding>> = Vec::with_capacity(reviewers.len());
    let mut summary = String::new();
    // Whether `summary` currently comes from a reviewer whose findings are
    // capped, and may therefore still be replaced by an uncapped reviewer's.
    let mut summary_capped = false;
    let mut resolved: Vec<String> = Vec::new();
    let mut unanchored = 0usize;
    let mut discarded = 0usize;

    for (reviewer, answer) in reviewers.iter().zip(&answers) {
        let Some(value) = answer.value.clone() else {
            // One reviewer's failure is not the lane's. With a council
            // configured, losing one angle should cost that angle and nothing
            // else — the same rule `lanes::fanout` applies to a file.
            tracing::warn!(
                agent = reviewer.id,
                err = answer.error.as_deref().unwrap_or("no answer"),
                "a council reviewer failed"
            );
            continue;
        };

        spend.note(&answer.model);

        // One reviewer answering off-schema is that reviewer lost, not the
        // file. With a solo council there is nothing else, so it is fatal —
        // which is what `per_reviewer.is_empty()` below turns it into.
        let asked = match place(llm.clone(), input, diff, &evidence, value).await {
            Ok(asked) => asked,
            Err(err) if reviewers.len() > 1 => {
                tracing::warn!(agent = reviewer.id, %err, "a council reviewer failed");
                continue;
            }
            Err(err) => return Err(err),
        };

        spend.merge(asked.spend);
        unanchored += asked.unanchored;
        discarded += asked.discarded;
        // The first reviewer's prose, taken whole. Blending N summaries would
        // author text no reviewer wrote, which is the objection `src/falsify`
        // raises to a filter that can return findings of its own.
        //
        // A capped reviewer yields the headline to any uncapped one, whatever
        // the configured order: `style` is capped precisely because its subject
        // is not what a check run should lead with, and the summary is the one
        // line a human actually reads.
        let capped = reviewer.ceiling.is_some();
        if summary.is_empty() || (summary_capped && !capped) {
            summary = asked.summary;
            resolved = asked.resolved;
            summary_capped = capped;
        }
        let mut findings = asked.findings;
        reviewer.clamp(&mut findings);
        per_reviewer.push(findings);
    }

    if per_reviewer.is_empty() {
        return Err(crate::error::Error::lane(
            "critique",
            format!("every reviewer failed on {}", diff.path),
        ));
    }

    // Corroboration is a separate switch from the council itself, so the merge
    // can be measured before a second agent is what is being judged. Off, the
    // findings are concatenated exactly as the reviewers produced them.
    let findings: Vec<Finding> = if config.council.corroboration {
        council::merge(per_reviewer)
    } else {
        per_reviewer.into_iter().flatten().collect()
    };

    // Step 5, once over the merged set rather than once per reviewer. The
    // filter can only reject, so more inputs in one pass is identical semantics
    // at a fraction of the calls.
    let filtered = Falsifier::new(llm.model().as_ref(), config)
        .filter(LaneId::Critique, findings, &evidence)
        .await;
    spend.merge(filtered.spend);

    Ok(FileReview {
        summary: summarise(
            summary.trim(),
            unanchored,
            discarded,
            &filtered.rejected,
            filtered.findings.len(),
        ),
        findings: filtered.findings,
        resolved,
        spend,
    })
}

/// What one reviewer said about one file.
struct Asked {
    summary: String,
    resolved: Vec<String>,
    findings: Vec<Finding>,
    spend: Spend,
    unanchored: usize,
    discarded: usize,
}

/// Build one reviewer's prompt for one file.
///
/// Split from [`place`] so every reviewer's prompt is assembled before any call
/// is made: the graph asks them all at once, and a builder that ran inside the
/// call would serialise them again.
fn build_prompt<'a>(
    input: &'a LaneInput<'_>,
    changed_paths: &'a [String],
    diff: &'a FileDiff,
    evidence: &'a str,
    reviewer: &council::Reviewer<'_>,
) -> prompt::Prompt {
    let config: &Config = input.config;

    prompt::build(&PromptInputs {
        repo_policy: input.repo_policy,
        extracted_rules: input.extracted_rules,
        prior_findings: input.prior_findings,
        new_evidence: evidence,
        // Every path the pull request touched, not just this one. This selects
        // which `path_instructions` are injected, and narrowing it to the focus
        // file would silently drop the rules for every other changed path from
        // a prefix all N conversations otherwise share — losing the cache as
        // well as the rules.
        changed_paths,
        focus_path: Some(&diff.path),
        persona: reviewer.persona,
        retrieved_context: input.retrieved_context,
        ..PromptInputs::new(LaneId::Critique, config)
    })
}

/// Place what one reviewer said against the file it reviewed.
async fn place(
    llm: std::sync::Arc<crate::flows::caps::ModelCapability>,
    input: &LaneInput<'_>,
    diff: &FileDiff,
    evidence: &str,
    value: serde_json::Value,
) -> Result<Asked> {
    let config: &Config = input.config;

    // The call's own cost is already tallied inside the capability; what is
    // counted here is only what *placement* adds, which is a relocation call
    // per finding the quote could not anchor.
    let mut spend = Spend::default();
    let parsed = schema::parse(LaneId::Critique, value)?;

    let model = llm.model().as_ref();
    let positioner = Positioner::new(model, config);
    let mut findings = Vec::new();
    let mut unanchored = 0usize;
    let mut discarded = 0usize;

    for raw in parsed.findings {
        // Any file but this conversation's own is dropped. That is stricter
        // than the whole-diff lane's rule — which only required the pull
        // request to have touched the file — and it has to be: N reviewers
        // each reporting the same cross-file problem is what `focus_path`
        // exists to prevent, and honouring an off-file finding here would
        // undo it.
        if raw.path != diff.path {
            discarded += 1;
            continue;
        }

        // Budget check: relocation can make one model call per unresolvable
        // finding, so enforce the limit inside the loop before escalating to
        // stage 3. Do not wait until the lane finishes.
        if spend.cost_usd() > config.models.budget_usd_per_pr {
            return Err(crate::error::Error::Budget {
                spent: spend.cost_usd(),
                limit: config.models.budget_usd_per_pr,
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
                    rendered_diff: evidence,
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

    // Falsification is deliberately *not* here: it runs once over the merged
    // set in `review_file`, because a reject-only filter given more inputs in
    // one pass has identical semantics at a fraction of the calls.
    Ok(Asked {
        summary: parsed.summary,
        resolved: parsed.resolved,
        findings,
        spend,
        unanchored,
        discarded,
    })
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
///
/// The model's own prose is **dropped entirely** when falsification left
/// nothing standing, and that is the important case. The prose is written
/// before the filter runs, so it describes findings that no longer exist:
/// a review once opened "One real bug: the coverage edge is never stored",
/// reported no findings, concluded success, and approved the pull request in
/// the same breath — the bug was a hallucination the falsifier correctly
/// removed, and only the summary still claimed it. A lane that reports nothing
/// must not narrate something. What replaces it is the rejection reasons,
/// which say more than the discarded prose did.
fn summarise(
    summary: &str,
    unanchored: usize,
    discarded: usize,
    rejected: &[Rejection],
    kept: usize,
) -> String {
    if kept == 0 && !rejected.is_empty() {
        let reasons: Vec<String> = rejected
            .iter()
            .map(|item| format!("{} — {}", item.title.trim(), item.reason.trim()))
            .collect();
        return format!(
            "Nothing to report. {} finding{} raised and dropped as disproved by the diff: {}.",
            rejected.len(),
            plural(rejected.len()),
            reasons.join("; ")
        );
    }

    let rejected = rejected.len();
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
    use crate::forge::types::CheckConclusion;
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
                extracted_rules: &[],
                reviewed_evidence: "",
                prior_findings: &[],
                retrieved_context: "",
                corpus: None,
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
            outcome
                .summary
                .contains("1 finding raised and dropped as disproved"),
            "{}",
            outcome.summary
        );
        assert!(
            outcome.summary.contains("the diff bounds-checks `i` above"),
            "the rejection reason replaces the prose it disproved: {}",
            outcome.summary
        );
    }

    /// The bug this lane shipped: a check run whose summary asserted a bug,
    /// reported no findings, concluded success and approved the pull request.
    /// The model's prose is written before falsification runs, so once the
    /// filter empties the finding list the prose is describing nothing.
    #[tokio::test]
    async fn a_summary_never_asserts_a_bug_the_falsifier_removed() {
        let model = MockModel::new()
            .then(json!({
                "summary": "One real bug: the coverage edge is never stored.",
                "findings": [finding_quoting("let x = items[i];")]
            }))
            .then(json!({
                "incorrect": [{"index": 1, "reason": "the diff stores it two lines above"}]
            }));

        let outcome = run_with(model, &config(), &diffs()).await;

        assert!(outcome.findings.is_empty());
        assert!(
            !outcome.summary.contains("One real bug"),
            "a lane reporting nothing must not narrate something: {}",
            outcome.summary
        );
        assert_eq!(
            outcome.conclusion(Severity::High),
            CheckConclusion::Success,
            "the verdict was already clean; it is the summary that had to agree with it"
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
                extracted_rules: &[],
                reviewed_evidence: "",
                prior_findings: &[],
                retrieved_context: "",
                corpus: None,
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

    /// A per-file lane can do better than replaying an already-reviewed file
    /// into a cacheable prefix: it can not send it at all. The cheapest call is
    /// the one not made, and a cache prefix only pays when the provider honours
    /// it.
    #[tokio::test]
    async fn a_file_reviewed_before_and_unchanged_since_is_not_reviewed_again() {
        let config = config();
        let model = MockModel::silent();
        let pr = pull_request();

        // The earlier cycle reviewed `src/earlier.rs`; this push adds
        // `src/main.rs`. Only the new file is worth a call.
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
                extracted_rules: &[],
                reviewed_evidence: &reviewed,
                prior_findings: &["Close the socket on the error path".to_string()],
                retrieved_context: "",
                corpus: None,
            })
            .await
            .expect("runs");

        let requests = model.requests();
        assert_eq!(requests.len(), 1, "one call, for the one unreviewed file");

        let system = &requests[0].messages[0].content;
        let user = &requests[0].messages[1].content;

        assert!(
            !system.contains("+earlier") && !user.contains("+earlier"),
            "the unchanged file is not sent at all"
        );
        assert!(user.contains("src/main.rs"), "the delta is the new work");
        assert!(
            system.contains("The file is `src/main.rs`"),
            "each conversation owns exactly one file: {system}"
        );
        assert!(
            user.contains("Close the socket"),
            "prior findings are volatile"
        );
        assert!(
            !system.contains("Close the socket"),
            "prior findings must not enter the cached prefix"
        );
    }

    /// One file per conversation, so a forty-file pull request is forty close
    /// readings rather than one that fades after the first few files.
    #[tokio::test]
    async fn every_changed_file_gets_its_own_conversation() {
        let config = config();
        let model = MockModel::silent();
        let pr = pull_request();
        let diffs = vec![
            parse_file_patch("src/main.rs", PATCH),
            parse_file_patch("src/other.rs", PATCH),
            parse_file_patch("src/third.rs", PATCH),
        ];

        Critique::new(Arc::new(model.clone()))
            .run(LaneInput {
                config: &config,
                pull_request: &pr,
                diffs: &diffs,
                file_contents: &BTreeMap::new(),
                scan_findings: &[],
                commits: &[],
                repo_policy: None,
                extracted_rules: &[],
                reviewed_evidence: "",
                prior_findings: &[],
                retrieved_context: "",
                corpus: None,
            })
            .await
            .expect("runs");

        let requests = model.requests();
        assert_eq!(requests.len(), 3, "one conversation per changed file");

        for (request, path) in requests
            .iter()
            .zip(["src/main.rs", "src/other.rs", "src/third.rs"])
        {
            let system = &request.messages[0].content;
            assert!(
                system.contains(&format!("The file is `{path}`")),
                "each conversation is scoped to its own file: {system}"
            );
        }
    }

    /// Malformed output still never becomes a comment. What changed with the
    /// fan-out is where the failure lands: it is isolated to its own file
    /// rather than failing the lane, so one bad response cannot delete the
    /// review of every other file. A lane where *nothing* could be reviewed
    /// must still not report success — that is the part branch protection
    /// depends on.
    #[tokio::test]
    async fn malformed_model_output_is_reported_rather_than_posted_as_nonsense() {
        let model = MockModel::new().then(json!({"summary": "…", "findings": [{"path": "x"}]}));
        let pr = pull_request();
        let config = config();
        let diffs = diffs();

        let outcome = Critique::new(Arc::new(model))
            .run(LaneInput {
                config: &config,
                pull_request: &pr,
                diffs: &diffs,
                file_contents: &BTreeMap::new(),
                scan_findings: &[],
                commits: &[],
                repo_policy: None,
                extracted_rules: &[],
                reviewed_evidence: "",
                prior_findings: &[],
                retrieved_context: "",
                corpus: None,
            })
            .await
            .expect("the failure is isolated, not propagated");

        assert!(outcome.findings.is_empty(), "nothing nonsensical is posted");
        assert!(
            outcome.summary.contains("src/main.rs"),
            "the file that could not be reviewed is named: {}",
            outcome.summary
        );
        assert_eq!(
            outcome.conclusion(Severity::High),
            CheckConclusion::Neutral,
            "a lane that reviewed nothing must not claim success"
        );
    }

    /// The other half of the isolation rule: one file's failure must leave the
    /// rest of the review standing.
    #[tokio::test]
    async fn one_files_failure_does_not_delete_the_other_files_review() {
        let config = config();
        let diffs = vec![
            parse_file_patch("src/main.rs", PATCH),
            parse_file_patch("src/other.rs", PATCH),
        ];
        let pr = pull_request();

        let model = MockModel::new()
            // src/main.rs: unparseable.
            .then(json!({"summary": "…", "findings": [{"path": "x"}]}))
            // src/other.rs: a real finding, and a falsifier that keeps it.
            .then(json!({
                "summary": "…",
                "findings": [{
                    "path": "src/other.rs",
                    "severity": "high",
                    "confidence": 0.9,
                    "rule": "bounds",
                    "title": "Unchecked index",
                    "body": "…",
                    "existing_code": "    let x = items[i];"
                }]
            }))
            .then(json!({"incorrect": []}));

        let outcome = Critique::new(Arc::new(model))
            .run(LaneInput {
                config: &config,
                pull_request: &pr,
                diffs: &diffs,
                file_contents: &BTreeMap::new(),
                scan_findings: &[],
                commits: &[],
                repo_policy: None,
                extracted_rules: &[],
                reviewed_evidence: "",
                prior_findings: &[],
                retrieved_context: "",
                corpus: None,
            })
            .await
            .expect("runs");

        assert_eq!(
            outcome.findings.len(),
            1,
            "the good file was still reviewed"
        );
        assert_eq!(outcome.findings[0].path, "src/other.rs");
        assert!(
            outcome.summary.contains("src/main.rs"),
            "and the failure is not hidden: {}",
            outcome.summary
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
