//! Lanes: one agent, one narrow job, one check run.
//!
//! A lane takes evidence and returns a [`LaneOutcome`]. It does **not** take a
//! [`ForgeWrite`](crate::ports::forge::ForgeWrite), so it cannot mutate a pull
//! request even by mistake — lanes propose, `src/apply` disposes. That is the
//! security boundary from `AGENTS.md`, enforced by the type system rather than
//! by discipline.

pub mod anchor;
pub mod commits;
pub mod critique;
pub mod description;
pub mod fanout;
pub mod security;
pub mod tests;
pub mod triage;

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;

use crate::config::types::{Config, LaneId, Severity};
use crate::error::Result;
use crate::evidence::diff::FileDiff;
use crate::findings::types::Finding;
use crate::forge::types::{CheckConclusion, Commit, PullRequest};
use crate::harness::schema::LaneResponse;
use crate::ports::corpus::Corpus;
use crate::ports::model::Spend;
use crate::scan::types::{Finding as ScanFinding, ScanKind};

/// Everything a lane is given.
pub struct LaneInput<'a> {
    /// The effective configuration.
    pub config: &'a Config,
    /// The pull request under review.
    pub pull_request: &'a PullRequest,
    /// The parsed diffs, one per changed file.
    pub diffs: &'a [FileDiff],
    /// Head-revision content of the changed files, keyed by path.
    ///
    /// Optional evidence: a run with a checkout fills it, a forge-only run
    /// leaves it empty. It exists so `src/position` can fall back to the whole
    /// file when a finding quotes context the diff did not include; with it
    /// empty that stage is simply skipped.
    pub file_contents: &'a BTreeMap<String, String>,
    /// Findings the deterministic scanners already produced, for the lanes that
    /// adjudicate rather than re-discover.
    pub scan_findings: &'a [ScanFinding],
    /// The commits in the pull request's range, for the `commits` lane.
    pub commits: &'a [Commit],
    /// Curated repository policy: the pinned knowledge documents in scope,
    /// written by operators through the admin API. Cacheable prefix material.
    pub repo_policy: Option<&'a str>,
    /// Rules the sandboxed extraction pass read out of the repository's own
    /// instruction files. **Untrusted** — the pull request's author wrote them
    /// — so they land in the prompt's volatile suffix, fenced and labelled.
    pub extracted_rules: &'a [String],
    /// The diff already reviewed at the last reviewed SHA, replayed verbatim so
    /// the prompt prefix stays cacheable. Empty on a first review.
    pub reviewed_evidence: &'a str,
    /// Titles of findings raised in earlier cycles.
    pub prior_findings: &'a [String],
    /// Code retrieved from the index for this pull request: what the change
    /// resembles, and what it reaches.
    ///
    /// **Volatile.** It is composed from this diff, so it belongs in the
    /// prompt's suffix and nowhere near the cacheable prefix — see
    /// `crate::harness::prompt`. Empty when retrieval is off, degraded, or
    /// found nothing, in which case the lane reviews the diff alone.
    pub retrieved_context: &'a str,
    /// Read-only access to the tree, for the tools a sub-agent may call.
    ///
    /// `None` is the ordinary case and not a degradation: sub-agents are off by
    /// default, and with them on but no corpus available they answer from the
    /// evidence alone, exactly as they did before tools existed. What must
    /// never happen is a corpus that can reach outside the repository under
    /// review — see [`crate::flows::tools::ReadOnlyTools`], which refuses that
    /// in front of every implementation rather than trusting each one.
    pub corpus: Option<Arc<dyn Corpus>>,
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

    /// Every changed path, for selecting the path rules that apply.
    pub fn changed_paths(&self) -> Vec<String> {
        self.diffs.iter().map(|d| d.path.clone()).collect()
    }

    /// The scanner findings of the given kinds.
    ///
    /// Which lane adjudicates which kind is a partition, not an overlap: two
    /// lanes discussing the same scanner match is the double-reporting this
    /// whole design exists to avoid.
    pub fn scanner_findings_of(&self, kinds: &[ScanKind]) -> Vec<&ScanFinding> {
        self.scan_findings
            .iter()
            .filter(|f| kinds.contains(&f.kind))
            .collect()
    }

    /// Whether the pull request should be skipped as an unreviewed draft.
    pub fn skip_as_draft(&self) -> Option<LaneOutcome> {
        (self.pull_request.draft && !self.config.review.draft_prs).then(|| {
            LaneOutcome::skipped(
                "Draft pull request; set `review.draft_prs = true` to review drafts.",
            )
        })
    }
}

/// What a lane does with a finding it cannot anchor to a changed line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Anchoring {
    /// Drop it. For lanes whose subject matter is the code itself.
    Strict,
    /// Keep it without a line. For lanes whose subject matter — a commit
    /// message, a missing description — has no line to point at.
    Demote,
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
    /// What the model calls cost, and which models actually answered.
    pub spend: Spend,
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

    /// Turn a parsed model response into an outcome, applying `anchoring`.
    ///
    /// Every lane shares this so the anchoring rule cannot drift between them:
    /// a lane that quietly kept mis-anchored findings would post comments on
    /// code its pull request never touched, and nothing downstream re-checks.
    pub fn from_response(
        lane: LaneId,
        parsed: LaneResponse,
        diffs: &[FileDiff],
        anchoring: Anchoring,
        spend: Spend,
    ) -> Self {
        let mut findings = Vec::new();
        let mut discarded = 0usize;
        for raw in parsed.findings {
            // Structured responses anchor with a quoted snippet, not a line
            // number. Resolve that quote before strict anchoring so schema-
            // conforming findings are not all discarded as line-less.
            let range = raw.existing_code.as_deref().and_then(|snippet| {
                diffs
                    .iter()
                    .find(|diff| diff.path == raw.path)
                    .and_then(|diff| crate::position::locate(snippet, Some(diff), None).range())
            });
            let mut finding = raw.into_finding(lane);
            if let Some((start, end)) = range {
                finding.line = Some(start);
                finding.end_line = (end > start).then_some(end);
            }
            match anchoring {
                Anchoring::Strict if !anchor::anchored_in_diff(&finding, diffs) => {
                    // Not an error: models do this routinely. Dropped quietly
                    // and counted, so the count can surface in the summary if
                    // it ever gets large enough to mean something.
                    discarded += 1;
                }
                Anchoring::Strict => findings.push(finding),
                Anchoring::Demote => findings.push(anchor::demote_unanchored(finding, diffs)),
            }
        }

        let summary = if discarded > 0 {
            format!(
                "{} ({discarded} finding{} discarded for not matching a changed line)",
                parsed.summary.trim(),
                if discarded == 1 { "" } else { "s" }
            )
        } else {
            parsed.summary.trim().to_string()
        };

        Self {
            summary,
            findings,
            resolved: parsed.resolved,
            spend,
            skipped: None,
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

// Named for what it covers rather than `tests`: `lanes::tests` is already the
// tests *lane*, and the two module names would collide.
#[cfg(test)]
mod outcome_tests {
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
            applicable: None,
            late: false,
            identity: None,
            corroboration: 1,
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
