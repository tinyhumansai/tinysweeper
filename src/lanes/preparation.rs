//! Borrowed evidence shared by pure lane-candidate preparation.
//!
//! This view deliberately excludes model capabilities and prompt context. The
//! application can prepare candidate groups before paid extraction, and the
//! existing lane runners use the same predicates and grouping inputs.

use crate::config::types::Config;
use crate::evidence::diff::FileDiff;
use crate::forge::types::PullRequest;
use crate::lanes::LaneOutcome;
use crate::lanes::grouping::{self, FileGroup, GroupBounds};
use crate::scan::types::{Finding as ScanFinding, ScanKind};

/// The host-derived inputs needed to decide which groups a lane can review.
#[derive(Clone, Copy)]
pub(crate) struct LanePreparationInput<'a> {
    /// Review policy that controls draft handling and file grouping.
    pub(crate) config: &'a Config,
    /// Pull request metadata used by deterministic lane eligibility checks.
    pub(crate) pull_request: &'a PullRequest,
    /// Read-only changed-file evidence used to select and group candidates.
    pub(crate) diffs: &'a [FileDiff],
    /// Host-side scanner results used to force security-sensitive paths in.
    pub(crate) scan_findings: &'a [ScanFinding],
    /// Prior review evidence used to omit paths that remain unchanged.
    pub(crate) reviewed_evidence: &'a str,
    /// Read-only repository neighborhood used when grouping related files.
    pub(crate) graph: Option<&'a crate::index::types::Neighbourhood>,
}

impl<'a> LanePreparationInput<'a> {
    /// Whether any file has added or modified lines for a model lane.
    pub(crate) fn has_reviewable_content(&self) -> bool {
        self.diffs.iter().any(|diff| !diff.changed_lines.is_empty())
    }

    /// Every changed path, for selecting the path rules that apply.
    pub(crate) fn changed_paths(&self) -> Vec<String> {
        self.diffs.iter().map(|diff| diff.path.clone()).collect()
    }

    /// Scanner findings owned by the requested lane.
    pub(crate) fn scanner_findings_of(&self, kinds: &[ScanKind]) -> Vec<&'a ScanFinding> {
        self.scan_findings
            .iter()
            .filter(|finding| kinds.contains(&finding.kind))
            .collect()
    }

    /// Group already-selected paths under this review's graph and bounds.
    pub(crate) fn group(&self, paths: &[String], bounds: &GroupBounds) -> Vec<FileGroup> {
        grouping::group(paths, self.diffs, self.graph, bounds)
    }

    /// Skip a draft using the lane's existing draft-review policy.
    pub(crate) fn skip_as_draft(&self) -> Option<LaneOutcome> {
        (self.pull_request.draft && !self.config.review.draft_prs).then(|| {
            LaneOutcome::skipped(
                "Draft pull request; set `review.draft_prs = true` to review drafts.",
            )
        })
    }
}
