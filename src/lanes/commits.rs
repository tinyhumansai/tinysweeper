//! The `commits` lane: nothing sensitive enters the history.
//!
//! **This lane makes no model call.** It republishes what the deterministic
//! scanners found in the commit range — secrets, oversized blobs, committed
//! build output — and nothing else.
//!
//! It used to do a second job: judge the commit range itself, for messages that
//! describe nothing, merge noise, unrelated work bundled together. That job is
//! gone, and the reason is worth recording so nobody restores it by accident.
//!
//! A model asked "is anything wrong with these commits?" will always find
//! something, because commit prose is infinitely criticisable and the question
//! presumes a defect. What it produced in practice was a stream of
//! style objections on a repository whose commits are frequently written by an
//! automated checkpointing hook — objections nobody would act on, attached to a
//! check that could block a merge. One of them arrived carrying the rule
//! `Commit message style only — not flagged` and flagged it anyway.
//!
//! The judgement is also the part that cannot be audited, and the scan is the
//! part that can. Keeping only the scan makes this lane's verdict deterministic:
//! it fails when a regular expression matched, and for no other reason. That is
//! a stronger security property than it had before, and it costs nothing.
//!
//! Nothing here quotes a credential. Scanner findings carry a redacted hint and
//! never the value.

use async_trait::async_trait;

use crate::config::types::LaneId;
use crate::error::Result;
use crate::lanes::{Lane, LaneInput, LaneOutcome};
use crate::scan::types::ScanKind;

/// The scanner findings this lane owns.
///
/// A partition with [`security::ADJUDICATES`](crate::lanes::security::ADJUDICATES):
/// two lanes discussing one scanner match would report it twice.
pub const ADJUDICATES: [ScanKind; 3] = [ScanKind::Secret, ScanKind::Blob, ScanKind::Junk];

/// The commits lane.
///
/// Deliberately holds no model. The field is not merely unused — its absence is
/// what makes "this lane cannot form an opinion" true at the type level rather
/// than by convention.
#[derive(Default)]
pub struct Commits;

impl Commits {
    /// Build the lane.
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Lane for Commits {
    fn id(&self) -> LaneId {
        LaneId::Commits
    }

    async fn run(&self, input: LaneInput<'_>) -> Result<LaneOutcome> {
        let scanner = input.scanner_findings_of(&ADJUDICATES);

        if scanner.is_empty() {
            return Ok(LaneOutcome::skipped(
                "Nothing sensitive found in what this pull request commits.",
            ));
        }

        // Deliberately *not* skipped for a draft. Every other lane defers on a
        // draft because its opinion can wait; a committed credential cannot.
        // The moment it is pushed it is in the history, and marking the pull
        // request draft afterwards does not take it back out.

        // `LaneOutcome::default()` rather than a model response: this lane no
        // longer has one, so there is no spend to record and nothing to anchor.
        let mut outcome = LaneOutcome::default();
        merge_scanner_findings(&mut outcome, &scanner);

        outcome.summary = match outcome.findings.len() {
            1 => "1 sensitive item entered this pull request's history.".to_string(),
            n => format!("{n} sensitive items entered this pull request's history."),
        };

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::types::{Config, Severity};
    use crate::evidence::diff::{FileDiff, parse_file_patch};
    use crate::forge::types::{Commit, PullRequest};
    use crate::ports::model::Spend;
    use crate::scan::types::{Finding as ScanFinding, redact};
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
        vec![Commit {
            sha: "abc1234def".into(),
            message: "wip".into(),
            author_name: "Someone".into(),
            author_email: "someone@example.com".into(),
            patch: Some("--- a/src/config.rs\n+++ b/src/config.rs\n@@ -1,2 +1,3 @@\n a\n+const K: &str = \"…\";\n b\n".into()),
        }]
    }

    fn pull_request() -> PullRequest {
        PullRequest {
            number: 7,
            title: "fix: things".into(),
            head_sha: "abc123".into(),
            ..PullRequest::default()
        }
    }

    /// The AWS key literal, assembled so this file never contains one. GitHub
    /// push protection has rejected a push over a fixture before.
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
        config: &Config,
        commits: &[Commit],
        scan_findings: &[ScanFinding],
        draft: bool,
    ) -> LaneOutcome {
        let pr = PullRequest {
            draft,
            ..pull_request()
        };
        let diffs = diffs();
        Commits::new()
            .run(LaneInput {
                config,
                pull_request: &pr,
                diffs: &diffs,
                file_contents: &BTreeMap::new(),
                scan_findings,
                commits,
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

    #[tokio::test]
    async fn a_committed_secret_is_reported() {
        let outcome = run_with(&config(), &commits(), &[secret_finding()], false).await;

        assert_eq!(outcome.findings.len(), 1, "{outcome:?}");
        assert_eq!(outcome.findings[0].rule, "aws-access-key-id");
        assert_eq!(outcome.findings[0].severity, Severity::Critical);
    }

    #[tokio::test]
    async fn the_lane_spends_nothing_because_it_calls_no_model() {
        // The point of the lane. If a model call ever returns here, this is the
        // test that says so — `Spend` is only ever non-default after one.
        let outcome = run_with(&config(), &commits(), &[secret_finding()], false).await;

        assert_eq!(
            outcome.spend,
            Spend::default(),
            "the commits lane must cost nothing: {:?}",
            outcome.spend
        );
    }

    #[tokio::test]
    async fn commits_with_nothing_sensitive_in_them_are_not_reported_at_all() {
        // The behaviour this change exists to remove: an opinion about the
        // commit messages. A clean range now produces a skip, not a critique.
        let outcome = run_with(&config(), &commits(), &[], false).await;

        assert!(outcome.findings.is_empty(), "{outcome:?}");
        assert!(outcome.skipped.is_some(), "{outcome:?}");
    }

    #[tokio::test]
    async fn a_secret_value_never_reaches_a_finding_or_the_summary() {
        let outcome = run_with(&config(), &commits(), &[secret_finding()], false).await;

        let rendered = format!("{:?}\n{}", outcome.findings, outcome.summary);
        assert!(
            !rendered.contains(&key()),
            "the value must never leave the scanner: {rendered}"
        );
    }

    #[tokio::test]
    async fn a_draft_pull_request_is_still_scanned() {
        // Every other lane defers on a draft because its opinion can wait. A
        // committed credential cannot: it is in the history the moment it is
        // pushed, and the draft flag does not take it back out.
        let mut config = config();
        config.review.draft_prs = false;

        let outcome = run_with(&config, &commits(), &[secret_finding()], true).await;

        assert_eq!(outcome.findings.len(), 1, "{outcome:?}");
        assert!(outcome.skipped.is_none(), "{outcome:?}");
    }

    #[tokio::test]
    async fn only_this_lanes_scanner_kinds_are_republished() {
        // The partition with the security lane. Both republishing one match
        // would report it twice.
        let foreign = ScanFinding::new(
            ScanKind::Dependency,
            Severity::High,
            "Cargo.toml",
            "yanked-crate",
            "A yanked crate",
            "Bump it.",
        );

        let outcome = run_with(&config(), &commits(), &[foreign], false).await;

        assert!(outcome.findings.is_empty(), "{outcome:?}");
    }
}
