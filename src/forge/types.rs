//! The domain types the [`Forge`](crate::ports::forge::Forge) port speaks in.
//!
//! These are deliberately *not* GitHub's wire types. Keeping a hand-written
//! shape here is what lets the offline mock be a first-class implementation
//! rather than a stub, and what keeps octocrab out of the default build.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// A repository, as `owner/name`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct RepoId {
    /// The owning user or organisation.
    pub owner: String,
    /// The repository name.
    pub name: String,
}

impl RepoId {
    /// Parse `owner/name`.
    pub fn parse(value: &str) -> Option<Self> {
        let (owner, name) = value.split_once('/')?;
        if owner.is_empty() || name.is_empty() || name.contains('/') {
            return None;
        }
        Some(Self {
            owner: owner.to_string(),
            name: name.to_string(),
        })
    }
}

impl std::fmt::Display for RepoId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.owner, self.name)
    }
}

/// A pull request, reduced to what the lanes actually read.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequest {
    /// The pull request number.
    pub number: u64,
    /// Its title.
    pub title: String,
    /// Its body. Empty is meaningful — the `description` lane fails on it.
    pub body: String,
    /// The login of whoever opened it.
    pub author: String,
    /// Whether it is a draft.
    pub draft: bool,
    /// The branch being merged into.
    pub base_ref: String,
    /// The tip of the base branch.
    pub base_sha: String,
    /// The branch being merged.
    pub head_ref: String,
    /// The tip of the pull request.
    pub head_sha: String,
    /// Whether the head branch lives in a fork. Fork pull requests get a
    /// read-only token under `pull_request`, which changes what can be posted.
    pub from_fork: bool,
    /// Labels currently applied.
    pub labels: Vec<String>,
    /// Whether GitHub considers it mergeable, when known.
    pub mergeable: Option<bool>,
    /// Approving reviews currently on it.
    pub approvals: u32,
}

/// A file changed by a pull request.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangedFile {
    /// Path at the head commit.
    pub path: String,
    /// Path before the change, when the file was renamed.
    pub previous_path: Option<String>,
    /// What happened to it.
    pub status: FileStatus,
    /// Lines added.
    pub additions: u64,
    /// Lines removed.
    pub deletions: u64,
    /// The unified diff for this file, when the forge supplied one. Absent for
    /// binary files and for diffs the forge truncated.
    pub patch: Option<String>,
}

/// What happened to a changed file.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FileStatus {
    /// Newly added.
    Added,
    /// Content changed.
    #[default]
    Modified,
    /// Removed.
    Removed,
    /// Moved, possibly with content changes.
    Renamed,
}

/// A commit in the pull request's range.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Commit {
    /// The full SHA.
    pub sha: String,
    /// The full commit message, subject and body.
    pub message: String,
    /// The author's display name.
    pub author_name: String,
    /// The author's email. Checked for noreply/mismatch shapes by the `commits`
    /// lane, and never echoed into a comment.
    pub author_email: String,
}

/// The state a check run reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckConclusion {
    /// The lane passed.
    Success,
    /// The lane found something disqualifying.
    Failure,
    /// The lane found something a human has to decide on.
    ActionRequired,
    /// The lane did not apply to this pull request.
    Neutral,
    /// The lane could not run.
    Skipped,
}

impl CheckConclusion {
    /// Whether this conclusion blocks the aggregate gate.
    pub fn blocks(self) -> bool {
        matches!(
            self,
            CheckConclusion::Failure | CheckConclusion::ActionRequired
        )
    }
}

/// A check run to publish.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckRun {
    /// The check name, e.g. `tinysweeper/security`.
    pub name: String,
    /// The commit it reports on.
    pub head_sha: String,
    /// The outcome.
    pub conclusion: CheckConclusion,
    /// One-line title shown in the checks list.
    pub title: String,
    /// The markdown summary.
    pub summary: String,
}

/// An inline review comment anchored to a line of the diff.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewComment {
    /// The file it anchors to.
    pub path: String,
    /// The line in the head revision.
    pub line: u64,
    /// The first line, when the comment spans a range.
    pub start_line: Option<u64>,
    /// The markdown body, including the fingerprint marker.
    pub body: String,
}

/// An issue comment, either a new one or an edit of an existing one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssueComment {
    /// The comment id, when it already exists.
    pub id: Option<u64>,
    /// The login of whoever wrote it.
    pub author: String,
    /// The markdown body.
    pub body: String,
}

/// An issue, for the triage path.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Issue {
    /// The issue number.
    pub number: u64,
    /// Its title.
    pub title: String,
    /// Its body.
    pub body: String,
    /// The login of whoever opened it.
    pub author: String,
    /// Labels currently applied.
    pub labels: Vec<String>,
    /// Whether it is still open.
    pub open: bool,
    /// Age in days. Kept as a plain number so the offline mock does not need a
    /// clock and the age guards stay trivially testable.
    pub age_days: u32,
    /// Days since the last comment by anyone other than tinysweeper.
    pub quiet_days: u32,
    /// How many comments it has.
    pub comments: u32,
}

/// Everything a lane needs about one pull request, fetched once.
#[derive(Debug, Clone, Default)]
pub struct PullRequestContext {
    /// The pull request itself.
    pub pull_request: PullRequest,
    /// Its changed files.
    pub files: Vec<ChangedFile>,
    /// Its commits.
    pub commits: Vec<Commit>,
    /// Issue comments already on it, including tinysweeper's own.
    pub comments: Vec<IssueComment>,
    /// Check runs already reported against the head SHA, by name.
    pub checks: BTreeMap<String, CheckConclusion>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_ids_round_trip() {
        let repo = RepoId::parse("tinyhumansai/tinysweeper").expect("parses");
        assert_eq!(repo.owner, "tinyhumansai");
        assert_eq!(repo.name, "tinysweeper");
        assert_eq!(repo.to_string(), "tinyhumansai/tinysweeper");
    }

    #[test]
    fn malformed_repo_ids_are_rejected() {
        for bad in ["tinysweeper", "/tinysweeper", "owner/", "a/b/c", ""] {
            assert!(RepoId::parse(bad).is_none(), "accepted `{bad}`");
        }
    }

    #[test]
    fn only_failing_conclusions_block_the_gate() {
        assert!(CheckConclusion::Failure.blocks());
        assert!(CheckConclusion::ActionRequired.blocks());
        assert!(!CheckConclusion::Success.blocks());
        assert!(!CheckConclusion::Neutral.blocks());
        assert!(!CheckConclusion::Skipped.blocks());
    }
}
