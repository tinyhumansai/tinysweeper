//! Lanes: one agent, one narrow job, one check run.
//!
//! A lane takes evidence and returns a [`LaneOutcome`]. It does **not** take a
//! [`ForgeWrite`](crate::ports::forge::ForgeWrite), so it cannot mutate a pull
//! request even by mistake — lanes propose, `src/apply` disposes. That is the
//! security boundary from `AGENTS.md`, enforced by the type system rather than
//! by discipline.

pub mod critique;

use async_trait::async_trait;

use crate::config::types::{Config, LaneId, Severity};
use crate::error::Result;
use crate::evidence::diff::FileDiff;
use crate::findings::types::Finding;
use crate::forge::types::{CheckConclusion, PullRequest};
use crate::ports::model::Usage;
use crate::scan::types::Finding as ScanFinding;

/// Everything a lane is given.
pub struct LaneInput<'a> {
    /// The effective configuration.
    pub config: &'a Config,
    /// The pull request under review.
    pub pull_request: &'a PullRequest,
    /// The parsed diffs, one per changed file.
    pub diffs: &'a [FileDiff],
    /// Findings the deterministic scanners already produced, for the lanes that
    /// adjudicate rather than re-discover.
    pub scan_findings: &'a [ScanFinding],
    /// Repository policy gathered from ancestor `AGENTS.md` files.
    pub repo_policy: Option<&'a str>,
    /// The diff already reviewed at the last reviewed SHA, replayed verbatim so
    /// the prompt prefix stays cacheable. Empty on a first review.
    pub reviewed_evidence: &'a str,
    /// Titles of findings raised in earlier cycles.
    pub prior_findings: &'a [String],
}

impl LaneInput<'_> {
    /// Total lines this pull request added, across every file.
    pub fn additions(&self) -> usize {
        self.diffs.iter().map(FileDiff::additions).sum()
    }

    /// Whether any file has a reviewable diff at all.
    pub fn has_reviewable_content(&self) -> bool {
        self.diffs.iter().any(|d| !d.changed_lines.is_empty())
    }
}

/// What a lane concluded.
#[derive(Debug, Clone, Default)]
pub struct LaneOutcome {
    /// One or two sentences for the check-run summary.
    pub summary: String,
    /// The findings, before the shared filtering pipeline runs.
    pub findings: Vec<Finding>,
    /// Titles of earlier findings this revision fixed.
    pub resolved: Vec<String>,
    /// What the model calls cost.
    pub usage: Usage,
    /// Set when the lane did not apply to this pull request at all.
    pub skipped: Option<String>,
}

impl LaneOutcome {
    /// A lane that had nothing to do.
    pub fn skipped(reason: impl Into<String>) -> Self {
        let reason = reason.into();
        Self {
            summary: reason.clone(),
            skipped: Some(reason),
            ..Self::default()
        }
    }

    /// The check-run conclusion for this outcome.
    ///
    /// `fail_on` comes from the lane's config. A skipped lane is `Neutral`
    /// rather than `Success`: claiming success for work that never happened
    /// would make branch protection meaningless.
    pub fn conclusion(&self, fail_on: Severity) -> CheckConclusion {
        if self.skipped.is_some() {
            return CheckConclusion::Neutral;
        }
        if self.findings.iter().any(|f| f.severity >= fail_on) {
            return CheckConclusion::Failure;
        }
        CheckConclusion::Success
    }
}

/// One reviewing lane.
#[async_trait]
pub trait Lane: Send + Sync {
    /// Which lane this is.
    fn id(&self) -> LaneId;

    /// Run it.
    async fn run(&self, input: LaneInput<'_>) -> Result<LaneOutcome>;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn finding(severity: Severity) -> Finding {
        Finding {
            lane: LaneId::Critique,
            severity,
            confidence: 1.0,
            path: "src/lib.rs".into(),
            line: Some(1),
            end_line: None,
            rule: "r".into(),
            title: "t".into(),
            body: "b".into(),
            suggestion: None,
            late: false,
        }
    }

    #[test]
    fn a_clean_lane_succeeds() {
        let outcome = LaneOutcome {
            summary: "Nothing to report.".into(),
            ..LaneOutcome::default()
        };
        assert_eq!(outcome.conclusion(Severity::High), CheckConclusion::Success);
    }

    #[test]
    fn a_finding_below_the_bar_does_not_fail_the_check() {
        let outcome = LaneOutcome {
            findings: vec![finding(Severity::Medium)],
            ..LaneOutcome::default()
        };
        assert_eq!(outcome.conclusion(Severity::High), CheckConclusion::Success);
    }

    #[test]
    fn a_finding_at_or_above_the_bar_fails_it() {
        let outcome = LaneOutcome {
            findings: vec![finding(Severity::High)],
            ..LaneOutcome::default()
        };
        assert_eq!(outcome.conclusion(Severity::High), CheckConclusion::Failure);
        assert_eq!(
            LaneOutcome {
                findings: vec![finding(Severity::Critical)],
                ..LaneOutcome::default()
            }
            .conclusion(Severity::High),
            CheckConclusion::Failure
        );
    }

    #[test]
    fn a_skipped_lane_is_neutral_not_successful() {
        // Reporting success for work that never happened would make a required
        // check meaningless.
        let outcome = LaneOutcome::skipped("no reviewable content");
        assert_eq!(outcome.conclusion(Severity::High), CheckConclusion::Neutral);
        assert!(!outcome.conclusion(Severity::High).blocks());
    }
}
