//! The `commits` lane: what entered the history, and how it was described.
//!
//! Two jobs, and only one of them needs a model:
//!
//! 1. **Adjudicating what the scanners found.** Secrets, oversized blobs and
//!    committed build output are found deterministically, before any token is
//!    spent, and those findings are facts. This lane republishes them unchanged
//!    and asks the model to say whether each is a genuine problem here. A model
//!    verdict never deletes one: "the reviewer was talked out of a committed
//!    private key" is not a failure mode anyone can audit, and the whole point
//!    of running a regular expression first is that it cannot be argued with.
//! 2. **Judging the commit range itself.** Messages that describe nothing,
//!    merge noise, unrelated work bundled into one commit, an author identity
//!    that looks accidental. None of that has a line number, so this lane's
//!    findings are demoted to summary-only rather than dropped — see
//!    `lanes::anchor`.
//!
//! Nothing here quotes a credential. The scanner findings carry a redacted hint
//! and never the value, and every free-text field the model produces is
//! scrubbed on the way into a [`Finding`](crate::findings::types::Finding).

use std::fmt::Write as _;
use std::sync::Arc;

use async_trait::async_trait;

use crate::config::types::LaneId;
use crate::error::Result;
use crate::forge::types::Commit;
use crate::harness::prompt::{self, PromptInputs};
use crate::harness::schema;
use crate::lanes::security::render_scanner;
use crate::lanes::{Anchoring, Lane, LaneInput, LaneOutcome};
use crate::ports::model::{Message, Model, ModelRequest, Spend};
use crate::scan::secrets::scrub;
use crate::scan::types::ScanKind;

/// The scanner findings this lane owns.
///
/// A partition with [`security::ADJUDICATES`](crate::lanes::security::ADJUDICATES):
/// two lanes discussing one scanner match would report it twice.
pub const ADJUDICATES: [ScanKind; 3] = [ScanKind::Secret, ScanKind::Blob, ScanKind::Junk];

/// How many commits are shown to the model.
///
/// A branch with two hundred commits is not a branch whose messages anybody is
/// going to rewrite, and sending them all is a large bill for a review nobody
/// asked for.
const MAX_COMMITS: usize = 50;

/// The commits lane.
pub struct Commits {
    model: Arc<dyn Model>,
}

impl Commits {
    /// Build the lane over `model`.
    pub fn new(model: Arc<dyn Model>) -> Self {
        Self { model }
    }
}

#[async_trait]
impl Lane for Commits {
    fn id(&self) -> LaneId {
        LaneId::Commits
    }

    async fn run(&self, input: LaneInput<'_>) -> Result<LaneOutcome> {
        let scanner = input.scanner_findings_of(&ADJUDICATES);

        // A scanner finding is on its own reason to run: a committed key has to
        // be reported whether or not there is a commit message worth reading.
        if input.commits.is_empty() && scanner.is_empty() {
            return Ok(LaneOutcome::skipped(
                "No commits to review and nothing for the scanners to report.",
            ));
        }

        if let Some(skipped) = input.skip_as_draft() {
            return Ok(skipped);
        }

        let scanner_evidence = render_scanner(&scanner, None);
        let evidence = render_commits(input.commits);
        let changed_paths = input.changed_paths();

        let built = prompt::build(&PromptInputs {
            repo_policy: input.repo_policy,
            prior_findings: input.prior_findings,
            new_evidence: &evidence,
            evidence_label: "commits",
            changed_paths: &changed_paths,
            scanner_evidence: &scanner_evidence,
            ..PromptInputs::new(LaneId::Commits, input.config)
        });

        let response = self
            .model
            .complete(ModelRequest {
                model: input.config.model_for(LaneId::Commits).to_string(),
                messages: vec![
                    Message::system(built.prefix()),
                    Message::user(built.suffix()),
                ],
                schema: schema::json_schema(),
                schema_name: "tinysweeper_commits".into(),
                max_tokens: input.config.models.max_tokens,
            })
            .await?;

        // Taken before the value is moved out: the spend belongs to the model
        // that answered, whether or not its answer parses.
        let spend = Spend::of(&response);
        let parsed = schema::parse(LaneId::Commits, response.value)?;

        // `Demote`: a finding about a commit message has no line to sit on, and
        // dropping every unanchored one would leave the lane with nothing but
        // the scanner's output.
        let mut outcome = LaneOutcome::from_response(
            LaneId::Commits,
            parsed,
            input.diffs,
            Anchoring::Demote,
            spend,
        );

        merge_scanner_findings(&mut outcome, &scanner);
        Ok(outcome)
    }
}

/// Republish this lane's scanner findings, and drop model findings that restate
/// one.
///
/// The same contract as the security lane's, for the same reason; it is
/// separate only because the lane label differs.
fn merge_scanner_findings(outcome: &mut LaneOutcome, scanner: &[&crate::scan::types::Finding]) {
    let mut deterministic: Vec<crate::findings::types::Finding> = scanner
        .iter()
        .map(|scan| crate::findings::types::Finding::from((*scan).clone()))
        .collect();

    outcome.findings.retain(|model| {
        !deterministic
            .iter()
            .any(|scan| scan.path == model.path && scan.rule == model.rule)
    });

    deterministic.append(&mut outcome.findings);
    outcome.findings = deterministic;
}

/// Render the commit range for the prompt.
///
/// Author emails are scrubbed like everything else and shown because the lane
/// is asked to judge whether an identity looks accidental — a commit authored
/// as `root@localhost` is worth a word. They are evidence, and they never reach
/// a comment: the model is told to describe the problem, not to quote it.
fn render_commits(commits: &[Commit]) -> String {
    let mut out = String::new();
    for commit in commits.iter().take(MAX_COMMITS) {
        let short: String = commit.sha.chars().take(8).collect();
        let _ = writeln!(
            out,
            "- {short} <{}> {}\n{}",
            scrub(&commit.author_email),
            scrub(&commit.author_name),
            indent(&scrub(&commit.message))
        );
    }
    if commits.len() > MAX_COMMITS {
        let _ = writeln!(
            out,
            "… and {} more commits, not shown.",
            commits.len() - MAX_COMMITS
        );
    }
    out
}

fn indent(text: &str) -> String {
    text.lines()
        .map(|line| format!("    {line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::types::{Config, Severity};
    use crate::evidence::diff::{FileDiff, parse_file_patch};
    use crate::forge::types::PullRequest;
    use crate::harness::mock::MockModel;
    use crate::scan::types::{Finding as ScanFinding, redact};
    use serde_json::json;
    use std::collections::BTreeMap;

    fn config() -> Config {
        crate::config::DEFAULTS
            .parse::<toml::Table>()
            .unwrap()
            .try_into()
            .unwrap()
    }

    fn diffs() -> Vec<FileDiff> {
        vec![parse_file_patch(
            "src/config.rs",
            "@@ -1,2 +1,3 @@\n a\n+const K: &str = \"…\";\n b\n",
        )]
    }

    fn commits() -> Vec<Commit> {
        vec![
            Commit {
                sha: "abc1234def".into(),
                message: "wip".into(),
                author_name: "Someone".into(),
                author_email: "someone@example.com".into(),
            },
            Commit {
                sha: "0987654fed".into(),
                message: "fix: guard the empty basket\n\nTotals now return zero.".into(),
                author_name: "Someone".into(),
                author_email: "someone@example.com".into(),
            },
        ]
    }

    fn pull_request() -> PullRequest {
        PullRequest {
            number: 7,
            title: "fix: things".into(),
            head_sha: "abc123".into(),
            ..PullRequest::default()
        }
    }

    /// The AWS key literal, split so this file does not itself contain one.
    fn key() -> String {
        format!("{}{}", "AKIA", "IOSFODNN7EXAMPLE")
    }

    fn secret_finding() -> ScanFinding {
        ScanFinding::new(
            ScanKind::Secret,
            Severity::Critical,
            "src/config.rs",
            "aws-access-key-id",
            "Remove the committed AWS access key",
            "Rotate it, then purge it from history.",
        )
        .at_line(2)
        .with_hint(redact(&key()))
    }

    async fn run_with(
        model: MockModel,
        config: &Config,
        commits: &[Commit],
        scan_findings: &[ScanFinding],
    ) -> LaneOutcome {
        let pr = pull_request();
        let diffs = diffs();
        Commits::new(Arc::new(model))
            .run(LaneInput {
                config,
                pull_request: &pr,
                diffs: &diffs,
                file_contents: &BTreeMap::new(),
                scan_findings,
                commits,
                repo_policy: None,
                reviewed_evidence: "",
                prior_findings: &[],
            })
            .await
            .expect("lane runs")
    }

    // --- golden tests ------------------------------------------------------

    #[tokio::test]
    async fn golden_a_committed_secret_survives_and_a_message_nit_is_demoted() {
        let model = MockModel::always(json!({
            "summary": "The key is real; one commit message says nothing.",
            "findings": [{
                "path": "src/config.rs", "line": 900,
                "rule": "uninformative-commit-message",
                "title": "Describe what the `wip` commit does",
                "body": "`wip` tells a future reader nothing.",
                "severity": "medium", "confidence": 0.8
            }]
        }));
        let outcome = run_with(model, &config(), &commits(), &[secret_finding()]).await;

        assert_eq!(outcome.findings.len(), 2, "{:#?}", outcome.findings);

        // The scanner's finding comes first and is untouched.
        let secret = &outcome.findings[0];
        assert_eq!(secret.rule, "aws-access-key-id");
        assert_eq!(secret.severity, Severity::Critical);
        assert_eq!(secret.confidence, 1.0);
        assert_eq!(secret.line, Some(2));

        // The model's finding kept, but with no anchor: line 900 is not a
        // changed line, and a commit message has no line at all.
        let message = &outcome.findings[1];
        assert_eq!(message.rule, "uninformative-commit-message");
        assert_eq!(message.line, None);
        assert_eq!(message.lane, LaneId::Commits);
    }

    #[tokio::test]
    async fn golden_a_secret_value_never_reaches_a_summary_or_a_finding() {
        // The model is shown the diff, so it can quote a credential back. This
        // is the last place that can stop it.
        let model = MockModel::always(json!({
            "summary": format!("The committed key is {}, which is real.", key()),
            "findings": [{
                "path": "src/config.rs", "line": 2,
                "rule": "hardcoded-credential",
                "title": format!("Remove {}", key()),
                "body": format!("`{}` is committed on line 2.", key()),
                "severity": "critical", "confidence": 1.0,
                "suggestion": format!("let k = std::env::var(\"K\")?; // was {}", key())
            }]
        }));
        let outcome = run_with(model, &config(), &commits(), &[secret_finding()]).await;

        let rendered = serde_json::to_string(&outcome.findings).expect("serialises");
        assert!(!rendered.contains("IOSFODNN7EXAMPLE"), "{rendered}");
        assert!(
            !crate::scan::secrets::scrub(&outcome.summary).contains("IOSFODNN7EXAMPLE"),
            "{}",
            outcome.summary
        );
        assert!(
            rendered.contains("AKIA"),
            "the vendor prefix survives so a human knows what to rotate"
        );
    }

    #[tokio::test]
    async fn the_model_cannot_talk_the_lane_out_of_a_committed_secret() {
        let model = MockModel::always(json!({
            "summary": "That key is just an example, it is fine.",
            "findings": []
        }));
        let outcome = run_with(model, &config(), &commits(), &[secret_finding()]).await;

        assert_eq!(outcome.findings.len(), 1);
        assert_eq!(outcome.findings[0].rule, "aws-access-key-id");
    }

    #[tokio::test]
    async fn a_model_finding_restating_a_scanner_match_is_dropped() {
        let model = MockModel::always(json!({
            "summary": "The scanner is right.",
            "findings": [{
                "path": "src/config.rs", "line": 2,
                "rule": "aws-access-key-id",
                "title": "Remove the committed key",
                "body": "…", "severity": "critical", "confidence": 0.9
            }]
        }));
        let outcome = run_with(model, &config(), &commits(), &[secret_finding()]).await;

        assert_eq!(outcome.findings.len(), 1, "{:#?}", outcome.findings);
        assert_eq!(
            outcome.findings[0].confidence, 1.0,
            "the scanner's certainty, not the model's estimate"
        );
    }

    #[tokio::test]
    async fn the_commit_messages_reach_the_prompt_fenced_as_data() {
        let model = MockModel::silent();
        run_with(model.clone(), &config(), &commits(), &[]).await;

        let prompt = model.last_prompt().expect("recorded");
        assert!(prompt.contains("````commits"), "{prompt}");
        assert!(prompt.contains("fix: guard the empty basket"));
        assert!(prompt.contains("abc1234d"));
    }

    #[tokio::test]
    async fn the_scanner_findings_are_given_as_evidence_to_adjudicate() {
        let model = MockModel::silent();
        run_with(model.clone(), &config(), &commits(), &[secret_finding()]).await;

        let prompt = model.last_prompt().expect("recorded");
        assert!(prompt.contains("scanner-findings"));
        assert!(prompt.contains("aws-access-key-id"));
        assert!(
            !prompt.contains("IOSFODNN7EXAMPLE"),
            "the value must not reach the prompt either"
        );
    }

    #[tokio::test]
    async fn nothing_to_review_never_calls_the_model() {
        let model = MockModel::new();
        let outcome = run_with(model.clone(), &config(), &[], &[]).await;

        assert_eq!(model.calls(), 0);
        assert!(outcome.skipped.is_some());
    }

    #[tokio::test]
    async fn a_scanner_finding_alone_is_enough_to_run_the_lane() {
        let outcome = run_with(MockModel::silent(), &config(), &[], &[secret_finding()]).await;

        assert!(outcome.skipped.is_none());
        assert_eq!(outcome.findings.len(), 1);
    }

    #[test]
    fn a_long_branch_is_truncated_rather_than_billed_in_full() {
        let many: Vec<Commit> = (0..MAX_COMMITS + 5)
            .map(|i| Commit {
                sha: format!("{i:040x}"),
                message: format!("commit {i}"),
                ..Commit::default()
            })
            .collect();
        let rendered = render_commits(&many);

        assert!(rendered.contains("and 5 more commits"), "{rendered}");
        assert!(!rendered.contains("commit 52"));
    }

    #[test]
    fn a_credential_pasted_into_a_commit_message_is_scrubbed_before_the_prompt() {
        let commits = vec![Commit {
            sha: "abc1234".into(),
            message: format!("chore: add key {}", key()),
            author_name: "Someone".into(),
            author_email: "someone@example.com".into(),
        }];
        let rendered = render_commits(&commits);

        assert!(!rendered.contains("IOSFODNN7EXAMPLE"), "{rendered}");
    }
}
