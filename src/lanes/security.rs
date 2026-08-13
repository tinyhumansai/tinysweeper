//! The `security` lane: what this pull request makes newly attackable.
//!
//! Two halves, and the order between them is the design:
//!
//! 1. The deterministic scanners have **already run**, before any token was
//!    spent. Their findings are facts. This lane republishes them unchanged and
//!    hands them to the model as evidence to *adjudicate* — say whether each is
//!    real here and why — rather than asking a model to re-derive them. A
//!    regular expression that finds a widened workflow permission is cheaper,
//!    faster and more certain than a paragraph asking a model to look for one,
//!    and duplicating the scanner in the prompt would produce two opinions
//!    about one fact.
//! 2. The model then looks for what a scanner cannot see: untrusted input
//!    reaching a dangerous sink, an authorisation check that moved, a new
//!    subprocess or deserialization site.
//!
//! A model verdict never *removes* a scanner finding. Adjudication adds
//! context; it does not get to overrule a deterministic match, because the
//! failure mode of a model talking itself out of a real finding is exactly the
//! one this lane exists to prevent.
//!
//! It fans out one conversation per changed file — see `lanes::fanout` for why.

use std::fmt::Write as _;
use std::sync::Arc;

use async_trait::async_trait;

use crate::config::types::LaneId;
use crate::error::Result;
use crate::evidence::diff::{FileDiff, render as render_diffs};
use crate::findings::types::Finding;
use crate::harness::prompt::{self, PromptInputs};
use crate::harness::schema;
use crate::flows::runner::{self, PanelRequest};
use crate::lanes::fanout::{FileReview, per_file};
use crate::lanes::triage::triage;
use crate::lanes::{Anchoring, Lane, LaneInput, LaneOutcome};
use crate::ports::model::Model;
use crate::scan::types::{Finding as ScanFinding, ScanKind};

/// The scanner findings this lane owns.
///
/// A partition, not an overlap: `commits` adjudicates secrets, blobs and junk,
/// and two lanes discussing one scanner match would double-report it.
pub const ADJUDICATES: [ScanKind; 2] = [ScanKind::Workflow, ScanKind::Dependency];

/// The security lane.
pub struct Security {
    model: Arc<dyn Model>,
}

impl Security {
    /// Build the lane over `model`.
    pub fn new(model: Arc<dyn Model>) -> Self {
        Self { model }
    }
}

#[async_trait]
impl Lane for Security {
    fn id(&self) -> LaneId {
        LaneId::Security
    }

    async fn run(&self, input: LaneInput<'_>) -> Result<LaneOutcome> {
        let scanner = input.scanner_findings_of(&ADJUDICATES);

        // A scanner finding is reason enough to run even when nothing else is
        // reviewable: a workflow whose only change is a widened permission
        // still has to be reported.
        if !input.has_reviewable_content() && scanner.is_empty() {
            return Ok(LaneOutcome::skipped(
                "No added or modified lines to review.",
            ));
        }

        if let Some(skipped) = input.skip_as_draft() {
            return Ok(skipped);
        }

        // Files this lane has already seen, unchanged since. Dropped before
        // triage, not after: the cheapest call is the one not made, and this
        // lane spends one call per file, so re-reviewing an untouched file on
        // every push is the single largest avoidable cost in a review.
        //
        // Distinct from replaying evidence into the cacheable prefix, which is
        // what `critique` does. That only pays when the provider honours the
        // cache; this pays always.
        let fresh: Vec<crate::evidence::diff::FileDiff> =
            crate::evidence::replay::unreviewed(input.reviewed_evidence, input.diffs)
                .into_iter()
                .cloned()
                .collect();

        // Deterministic triage before any token is spent: drop the files that
        // cannot carry a vulnerability, and review what is left riskiest first
        // so an exhausted budget was spent on the files that mattered.
        //
        // A scanner match forces its file back in even when unchanged: a
        // committed key is reported every time until it is gone, and "we
        // mentioned it last push" is not a reason to stop.
        let forced: Vec<&str> = scanner.iter().map(|f| f.path.as_str()).collect();
        let forced_back: Vec<crate::evidence::diff::FileDiff> = input
            .diffs
            .iter()
            .filter(|d| forced.contains(&d.path.as_str()))
            .filter(|d| !fresh.iter().any(|f| f.path == d.path))
            .cloned()
            .collect();
        let mut considered = fresh;
        considered.extend(forced_back);

        let triaged = triage(&considered, &forced);

        if triaged.review.is_empty() && scanner.is_empty() {
            return Ok(LaneOutcome::skipped(format!(
                "No changed file has any attack surface.{}",
                skip_note(&triaged.skipped)
            )));
        }

        // One capability for the whole lane, so the pull-request budget is
        // enforced across every file and every panel round at once — which is
        // what lets the files run concurrently rather than one at a time.
        let llm = runner::lane_llm(
            self.model.clone(),
            input.config,
            input.config.models.budget_usd_per_pr,
        );

        let outcome = per_file(
            &triaged.review,
            |path| {
                let llm = llm.clone();
                let config = input.config;
                let repo_policy = input.repo_policy;
                let extracted_rules = input.extracted_rules;
                let prior_findings = input.prior_findings;
                let retrieved_context = input.retrieved_context;
                let diffs = input.diffs;
                let scanner = &scanner;
                async move {
                    let diff = diffs
                        .iter()
                        .find(|d| d.path == path)
                        .expect("the path came from the diff list");
                    review_file(
                        llm,
                        config,
                        repo_policy,
                        extracted_rules,
                        prior_findings,
                        retrieved_context,
                        diff,
                        scanner,
                    )
                    .await
                }
            },
        )
        .await;

        let mut outcome = outcome.into_outcome();
        outcome.summary.push_str(&skip_note(&triaged.skipped));
        merge_scanner_findings(&mut outcome, &scanner);
        Ok(outcome)
    }
}

/// Review one file, in a conversation that knows about no other file.
///
// Every argument is one prompt layer, and they are passed individually rather
// than as a context struct because each has a different trust level — see
// `harness::prompt`. Bundling them would make it easy to route the untrusted
// ones to the wrong half of the prompt.
#[allow(clippy::too_many_arguments)]
async fn review_file(
    llm: std::sync::Arc<crate::flows::caps::ModelCapability>,
    config: &crate::config::types::Config,
    repo_policy: Option<&str>,
    extracted_rules: &[String],
    prior_findings: &[String],
    retrieved_context: &str,
    diff: &FileDiff,
    scanner: &[&ScanFinding],
) -> Result<FileReview> {
    let evidence = render_diffs(std::slice::from_ref(diff));
    let scanner_evidence = render_scanner(scanner, Some(&diff.path));

    let built = prompt::build(&PromptInputs {
        repo_policy,
        extracted_rules,
        prior_findings,
        new_evidence: &evidence,
        focus_path: Some(&diff.path),
        scanner_evidence: &scanner_evidence,
        retrieved_context,
        ..PromptInputs::new(LaneId::Security, config)
    });

    // A panel rather than one call. The lenses partition this lane's subject:
    // where untrusted input can reach, who is allowed to do what, and the
    // adjudication of what the deterministic scanners already found. The last
    // is a lens rather than a separate call because a scanner match is context
    // the other two readings want anyway.
    let panel = runner::run_with_llm(
        llm,
        PanelRequest {
            lane: LaneId::Security,
            schema: runner::schema_with_questions(schema::json_schema()),
            suffix: built.suffix(),
            system_of: &|lens| runner::system_with_charter(built.prefix(), lens),
        },
    )
    .await;

    // A file whose every panellist failed is a file nobody read. It must reach
    // the fan-out's failure list rather than come back as a clean review — an
    // unreviewed file that reads as clean is what branch protection approves.
    if panel.nothing_was_read() {
        return Err(crate::error::Error::lane(
            LaneId::Security.as_str(),
            format!(
                "no panellist could review {}: {}",
                diff.path,
                panel
                    .failures
                    .iter()
                    .map(|(who, why)| format!("{who}: {why}"))
                    .collect::<Vec<_>>()
                    .join("; ")
            ),
        ));
    }

    let spend = panel.spend.clone();
    let parsed = runner::into_response(&panel);
    let outcome = LaneOutcome::from_response(
        LaneId::Security,
        parsed,
        std::slice::from_ref(diff),
        Anchoring::Strict,
        spend,
    );

    Ok(FileReview {
        summary: outcome.summary,
        findings: outcome.findings,
        resolved: outcome.resolved,
        spend: outcome.spend,
    })
}

/// Say, in the summary, which files were never sent to a model and why.
///
/// A review that quietly skipped half the pull request reads exactly like one
/// that found nothing wrong with it, so the skip is always stated.
fn skip_note(skipped: &[(String, &'static str)]) -> String {
    if skipped.is_empty() {
        return String::new();
    }
    const LISTED: usize = 5;
    let mut names: Vec<String> = skipped
        .iter()
        .take(LISTED)
        .map(|(path, reason)| format!("{path} ({reason})"))
        .collect();
    if skipped.len() > LISTED {
        names.push(format!("and {} more", skipped.len() - LISTED));
    }
    format!(
        " {} file{} not security-reviewed: {}.",
        skipped.len(),
        if skipped.len() == 1 { " was" } else { "s were" },
        names.join(", ")
    )
}

/// Render scanner findings for adjudication, by type and location only.
///
/// `redacted_hint` is the only thing from the match itself that is ever shown,
/// and the scanner already guaranteed it carries no entropy. The value has no
/// route into this string because [`ScanFinding`] has nowhere to keep it.
pub(crate) fn render_scanner(findings: &[&ScanFinding], path: Option<&str>) -> String {
    let mut out = String::new();
    for finding in findings {
        if path.is_some_and(|p| p != finding.path) {
            continue;
        }
        let location = match finding.line {
            Some(line) => format!("{}:{line}", finding.path),
            None => finding.path.clone(),
        };
        let _ = writeln!(
            out,
            "- [{}] {location} — {} ({}): {}",
            finding.kind.as_str(),
            finding.rule,
            finding.severity,
            finding.title
        );
        if let Some(hint) = &finding.redacted_hint {
            let _ = writeln!(out, "  matched: {hint}");
        }
    }
    out
}

/// Republish the scanner findings this lane owns, and drop any model finding
/// that merely restates one.
///
/// The scanner half is unconditional. A model that decides a deterministic
/// match is a false positive is welcome to say so in its summary; it does not
/// get to delete the finding, because "the model was talked out of it" is not a
/// failure mode anyone can audit.
pub(crate) fn merge_scanner_findings(outcome: &mut LaneOutcome, scanner: &[&ScanFinding]) {
    let mut deterministic: Vec<Finding> = scanner
        .iter()
        .map(|scan| {
            let mut finding = Finding::from((*scan).clone());
            finding.lane = LaneId::Security;
            finding
        })
        .collect();

    // Same file, same rule, already reported deterministically.
    outcome.findings.retain(|model| {
        !deterministic
            .iter()
            .any(|scan| scan.path == model.path && scan.rule == model.rule)
    });

    deterministic.append(&mut outcome.findings);
    outcome.findings = deterministic;
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

    const PATCH: &str = "@@ -1,3 +1,5 @@\n fn handler(req: Request) {\n+    let cmd = req.query(\"cmd\");\n+    Command::new(\"sh\").arg(\"-c\").arg(cmd).spawn();\n }\n";

    fn diffs() -> Vec<FileDiff> {
        vec![parse_file_patch("src/handler.rs", PATCH)]
    }

    fn pull_request() -> PullRequest {
        PullRequest {
            number: 7,
            title: "feat: add a handler".into(),
            head_sha: "abc123".into(),
            ..PullRequest::default()
        }
    }

    async fn run_with(
        model: MockModel,
        config: &Config,
        diffs: &[FileDiff],
        scan_findings: &[ScanFinding],
    ) -> LaneOutcome {
        run_with_reviewed(model, config, diffs, scan_findings, "").await
    }

    async fn run_with_reviewed(
        model: MockModel,
        config: &Config,
        diffs: &[FileDiff],
        scan_findings: &[ScanFinding],
        reviewed_evidence: &str,
    ) -> LaneOutcome {
        let pr = pull_request();
        Security::new(Arc::new(model))
            .run(LaneInput {
                config,
                pull_request: &pr,
                diffs,
                file_contents: &BTreeMap::new(),
                scan_findings,
                commits: &[],
                repo_policy: None,
                extracted_rules: &[],
                reviewed_evidence,
                prior_findings: &[],
                retrieved_context: "",
            })
            .await
            .expect("lane runs")
    }

    fn workflow_finding() -> ScanFinding {
        ScanFinding::new(
            ScanKind::Workflow,
            Severity::High,
            ".github/workflows/ci.yml",
            "workflow-write-all",
            "Narrow the workflow's permissions",
            "`permissions: write-all` grants far more than the job needs.",
        )
        .at_line(3)
    }

    // --- golden test -------------------------------------------------------

    #[tokio::test]
    async fn golden_a_command_injection_on_a_changed_line_survives() {
        let model = MockModel::panel(json!({
            "summary": "Adds a shell command built from request input.",
            "findings": [
                {
                    "path": "src/handler.rs", "line": 3,
                    "rule": "command-injection",
                    "title": "Do not build a shell command from request input",
                    "body": "`cmd` comes straight from the query string.",
                    "severity": "critical", "confidence": 0.95
                },
                {
                    "path": "src/handler.rs", "line": 1,
                    "rule": "unrelated",
                    "title": "Something about an unchanged line",
                    "body": "…", "severity": "high", "confidence": 0.9
                }
            ]
        }));
        let outcome = run_with(model, &config(), &diffs(), &[]).await;

        assert_eq!(outcome.findings.len(), 1, "{:#?}", outcome.findings);
        assert_eq!(outcome.findings[0].rule, "command-injection");
        assert_eq!(outcome.findings[0].severity, Severity::Critical);
        assert_eq!(outcome.findings[0].lane, LaneId::Security);
        assert!(outcome.summary.contains("1 finding discarded"));
    }

    #[tokio::test]
    async fn a_scanner_finding_is_republished_even_when_the_model_says_nothing() {
        let outcome = run_with(
            MockModel::silent(),
            &config(),
            &diffs(),
            &[workflow_finding()],
        )
        .await;

        assert_eq!(outcome.findings.len(), 1);
        assert_eq!(outcome.findings[0].rule, "workflow-write-all");
        assert_eq!(outcome.findings[0].lane, LaneId::Security);
    }

    #[tokio::test]
    async fn a_model_finding_restating_a_scanner_match_is_dropped() {
        // Otherwise the author gets the same permission problem twice: once
        // from the regular expression that is certain about it, and once from
        // the model that was shown the regular expression's output.
        let model = MockModel::panel(json!({
            "summary": "The scanner is right.",
            "findings": [{
                "path": ".github/workflows/ci.yml", "line": 3,
                "rule": "workflow-write-all",
                "title": "Narrow the workflow permissions",
                "body": "…", "severity": "high", "confidence": 0.9
            }]
        }));
        let workflow = parse_file_patch(
            ".github/workflows/ci.yml",
            "@@ -1,2 +1,3 @@\n name: ci\n on: push\n+permissions: write-all\n",
        );
        let outcome = run_with(model, &config(), &[workflow], &[workflow_finding()]).await;

        assert_eq!(outcome.findings.len(), 1, "{:#?}", outcome.findings);
        assert_eq!(
            outcome.findings[0].confidence, 0.8,
            "the scanner's, not the model's"
        );
    }

    #[tokio::test]
    async fn the_model_cannot_talk_the_lane_out_of_a_scanner_finding() {
        let model = MockModel::panel(json!({
            "summary": "That permission setting is fine, actually.",
            "findings": []
        }));
        let workflow = parse_file_patch(
            ".github/workflows/ci.yml",
            "@@ -1,2 +1,3 @@\n name: ci\n on: push\n+permissions: write-all\n",
        );
        let outcome = run_with(model, &config(), &[workflow], &[workflow_finding()]).await;

        assert_eq!(outcome.findings.len(), 1);
    }

    #[tokio::test]
    async fn the_scanner_findings_reach_the_prompt_as_evidence_not_instructions() {
        let model = MockModel::silent();
        let workflow = parse_file_patch(
            ".github/workflows/ci.yml",
            "@@ -1,2 +1,3 @@\n name: ci\n on: push\n+permissions: write-all\n",
        );
        run_with(model.clone(), &config(), &[workflow], &[workflow_finding()]).await;

        let prompt = model.last_prompt().expect("recorded");
        assert!(prompt.contains("scanner-findings"), "{prompt}");
        assert!(prompt.contains("workflow-write-all"));
        assert!(prompt.contains("Do not repeat them and do not re-scan"));
    }

    #[tokio::test]
    async fn each_file_gets_its_own_conversation() {
        let model = MockModel::silent();
        let diffs = vec![
            parse_file_patch("src/a.rs", "@@ -1 +1,2 @@\n a\n+b\n"),
            parse_file_patch("src/b.rs", "@@ -1 +1,2 @@\n a\n+b\n"),
        ];
        run_with(model.clone(), &config(), &diffs, &[]).await;

        assert_eq!(model.calls(), 2);
        let prompts: Vec<String> = model
            .requests()
            .iter()
            .map(|r| r.messages[0].content.clone())
            .collect();
        assert!(prompts.iter().any(|p| p.contains("`src/a.rs`")));
        assert!(prompts.iter().any(|p| p.contains("`src/b.rs`")));
        assert!(
            prompts.iter().all(|p| p.contains("One file only")),
            "each conversation must be told it owns one file"
        );
    }

    #[tokio::test]
    async fn one_files_failure_leaves_the_other_files_reviewed() {
        let model = MockModel::panel(json!({
                "summary": "Fine.",
                "findings": [{
                    "path": "src/a.rs", "line": 2, "rule": "r",
                    "title": "t", "body": "b",
                    "severity": "high", "confidence": 0.9
                }]
            }))
            .then_error("upstream exploded");
        let diffs = vec![
            parse_file_patch("src/a.rs", "@@ -1 +1,2 @@\n a\n+b\n"),
            parse_file_patch("src/b.rs", "@@ -1 +1,2 @@\n a\n+b\n"),
        ];
        let outcome = run_with(model, &config(), &diffs, &[]).await;

        assert_eq!(outcome.findings.len(), 1);
        assert!(
            outcome.summary.contains("could not be reviewed"),
            "{}",
            outcome.summary
        );
    }

    // --- triage -------------------------------------------------------------

    #[tokio::test]
    async fn a_lockfile_change_costs_no_model_call() {
        let model = MockModel::silent();
        let diffs = vec![
            parse_file_patch("Cargo.lock", "@@ -1 +1,2 @@\n a\n+serde = \"1\"\n"),
            parse_file_patch("src/handler.rs", "@@ -1 +1,2 @@\n a\n+let x = 1;\n"),
        ];
        let outcome = run_with(model.clone(), &config(), &diffs, &[]).await;

        assert_eq!(model.calls(), 1, "only the source file is worth a call");
        let prompt = model.last_prompt().expect("recorded");
        assert!(prompt.contains("`src/handler.rs`"), "{prompt}");
        assert!(
            outcome.summary.contains("Cargo.lock"),
            "the skip has to be visible: {}",
            outcome.summary
        );
    }

    #[tokio::test]
    async fn the_riskiest_file_is_reviewed_first() {
        let model = MockModel::silent();
        let diffs = vec![
            parse_file_patch("src/render.rs", "@@ -1 +1,2 @@\n a\n+let x = 1;\n"),
            parse_file_patch(
                "src/auth/session.rs",
                "@@ -1 +1,2 @@\n a\n+Command::new(\"sh\").arg(cmd).spawn();\n",
            ),
        ];
        run_with(model.clone(), &config(), &diffs, &[]).await;

        let first = &model.requests()[0].messages[0].content;
        assert!(
            first.contains("`src/auth/session.rs`"),
            "the budget must buy the riskiest file first: {first}"
        );
    }

    #[tokio::test]
    async fn a_scanner_finding_pulls_a_skippable_file_back_into_the_review() {
        let model = MockModel::silent();
        let diffs = vec![parse_file_patch(
            "Cargo.lock",
            "@@ -1 +1,2 @@\n a\n+left-pad = \"1\"\n",
        )];
        let dependency = ScanFinding::new(
            ScanKind::Dependency,
            Severity::High,
            "Cargo.lock",
            "dependency-yanked",
            "A yanked crate was added",
            "…",
        );
        run_with(model.clone(), &config(), &diffs, &[dependency]).await;

        assert_eq!(model.calls(), 1, "a scanner match is worth adjudicating");
    }

    #[tokio::test]
    async fn a_pull_request_of_only_generated_files_never_calls_the_model() {
        let model = MockModel::silent();
        let diffs = vec![parse_file_patch(
            "web/dist/app.min.js",
            "@@ -1 +1,2 @@\n a\n+var a=1\n",
        )];
        let outcome = run_with(model.clone(), &config(), &diffs, &[]).await;

        assert_eq!(model.calls(), 0);
        assert!(outcome.skipped.is_some(), "{:?}", outcome.skipped);
    }

    #[tokio::test]
    async fn a_pull_request_with_nothing_to_review_never_calls_the_model() {
        let model = MockModel::new();
        let outcome = run_with(model.clone(), &config(), &[], &[]).await;

        assert_eq!(model.calls(), 0);
        assert!(outcome.skipped.is_some());
    }

    #[tokio::test]
    async fn a_draft_is_skipped_unless_the_repository_opts_in() {
        let config = config();
        let model = MockModel::new();
        let pr = PullRequest {
            draft: true,
            ..pull_request()
        };
        let diffs = diffs();

        let outcome = Security::new(Arc::new(model.clone()))
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
            })
            .await
            .expect("runs");

        assert!(outcome.skipped.is_some());
        assert_eq!(model.calls(), 0);
    }

    #[tokio::test]
    async fn a_credential_the_model_quoted_never_reaches_a_finding() {
        let key = format!("{}{}", "AKIA", "IOSFODNN7EXAMPLE");
        let model = MockModel::panel(json!({
            "summary": "…",
            "findings": [{
                "path": "src/handler.rs", "line": 2,
                "rule": "hardcoded-credential",
                "title": format!("Remove the key {key}"),
                "body": format!("`{key}` is committed."),
                "severity": "critical", "confidence": 1.0
            }]
        }));
        let outcome = run_with(model, &config(), &diffs(), &[]).await;

        let rendered = serde_json::to_string(&outcome.findings).unwrap();
        assert!(!rendered.contains("IOSFODNN7EXAMPLE"), "{rendered}");
    }

    #[tokio::test]
    async fn a_file_reviewed_last_push_and_unchanged_costs_nothing_this_push() {
        // The largest avoidable cost in a review. This lane spends one model
        // call per file, so before this a push touching one file still paid for
        // every file in the pull request — and the bill grew with the branch
        // rather than with the change.
        let unchanged = parse_file_patch("src/handler.rs", PATCH);
        let touched = parse_file_patch(
            "src/other.rs",
            "@@ -1,1 +1,2 @@\n fn other() {\n+    let _ = std::process::Command::new(\"sh\");\n",
        );
        let previous = crate::evidence::replay::render(std::slice::from_ref(&unchanged));

        let model = MockModel::silent();
        let both = vec![unchanged, touched];
        run_with_reviewed(model.clone(), &config(), &both, &[], &previous).await;

        let paths: Vec<String> = model
            .requests()
            .into_iter()
            .map(|r| r.messages[1].content.clone())
            .collect();
        assert_eq!(paths.len(), 1, "one call, for the file that changed");
        assert!(
            paths[0].contains("src/other.rs") && !paths[0].contains("src/handler.rs"),
            "{paths:?}"
        );
    }

    #[tokio::test]
    async fn a_scanner_match_is_re_reported_even_on_an_unchanged_file() {
        // A committed key is reported every push until it is gone. "We
        // mentioned it last time" is not a reason to stop, so a scanner match
        // forces its file back past the incremental skip.
        let unchanged = parse_file_patch(".github/workflows/ci.yml", PATCH);
        let previous = crate::evidence::replay::render(std::slice::from_ref(&unchanged));

        let outcome = run_with_reviewed(
            MockModel::silent(),
            &config(),
            std::slice::from_ref(&unchanged),
            &[workflow_finding()],
            &previous,
        )
        .await;

        assert!(
            outcome
                .findings
                .iter()
                .any(|f| f.rule == "workflow-write-all"),
            "{outcome:?}"
        );
    }
}
