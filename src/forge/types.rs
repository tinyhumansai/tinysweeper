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
    /// Size of the file at the head revision, when the forge reported it.
    ///
    /// The blob scanner needs this to distinguish "a binary file changed" from
    /// "a four-megabyte binary entered the history", and those deserve very
    /// different reactions.
    pub size_bytes: Option<u64>,
}

impl ChangedFile {
    /// Whether the forge gave no textual diff for this file.
    ///
    /// True for genuine binaries *and* for diffs the forge truncated, which is
    /// why callers must not treat it as proof of binary content on its own.
    pub fn is_opaque(&self) -> bool {
        self.patch.is_none()
    }

    /// Whether this file changed in a way we were never shown.
    ///
    /// The two reasons a patch is absent are not equally innocent. A binary
    /// file has no textual diff to give and GitHub reports it as zero lines
    /// added and zero removed — there is nothing a reviewer could have read. A
    /// *truncated* patch is different: the forge is telling us lines changed
    /// and simultaneously declining to say which.
    ///
    /// Counting the second as "no reviewable content" is how a large file can
    /// pass through a review untouched and still leave the gate green. Callers
    /// must surface it instead, which is what separates "we looked and found
    /// nothing" from "we never looked".
    pub fn evidence_missing(&self) -> bool {
        self.patch.is_none() && self.additions + self.deletions > 0
    }
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
    /// The unified patch this commit introduced, when it was fetched.
    ///
    /// `None` means **not fetched** — beyond the patch budget, or served by an
    /// adapter that only returns metadata — and never "this commit changed
    /// nothing". The `commits` lane depends on the distinction: a finding may
    /// only cite a patch it was actually shown, so a commit whose patch is
    /// absent has to be rendered as such rather than as an empty diff.
    #[serde(default)]
    pub patch: Option<String>,
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

/// How a review is submitted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewEvent {
    /// Advisory. Blocks nothing.
    Comment,
    /// Blocks the merge button until a human resolves it.
    RequestChanges,
    /// Clears a previous changes-requested verdict from the same reviewer.
    ///
    /// Needed as much as the blocking verdict itself: GitHub keeps only the
    /// latest review per reviewer, so without this a pull request that has been
    /// fixed stays blocked by a stale objection until someone dismisses it by
    /// hand.
    Approve,
}

impl ReviewEvent {
    /// The GitHub API's name for this event.
    pub fn as_api(self) -> &'static str {
        match self {
            ReviewEvent::Comment => "COMMENT",
            ReviewEvent::RequestChanges => "REQUEST_CHANGES",
            ReviewEvent::Approve => "APPROVE",
        }
    }
}

/// An inline review comment anchored to a line of the diff.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewComment {
    /// The file it anchors to.
    pub path: String,
    /// The line in the head revision.
    ///
    /// `None` on the read path for a comment GitHub does not attach to a line —
    /// a reply on an outdated diff, or one whose anchor was rebased away. That
    /// case used to arrive as the literal `0`, indistinguishable from a real
    /// line zero and silently sorting first in anything ordered by line.
    /// Required on the write path; `apply` only builds a comment for a finding
    /// that resolved to a line.
    pub line: Option<u64>,
    /// The first line, when the comment spans a range.
    pub start_line: Option<u64>,
    /// The login of whoever wrote it. Empty on a comment being *written* —
    /// the forge assigns it.
    ///
    /// Load-bearing on the read path: the fingerprint markers used for dedupe
    /// are only honoured on comments tinysweeper itself wrote, and this is the
    /// field that says so. Without it a contributor could paste a marker into
    /// their own comment and suppress a real finding.
    #[serde(default)]
    pub author: String,
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

    #[test]
    fn a_binary_file_is_opaque_but_not_missing_evidence() {
        // GitHub reports a binary change as zero lines either way, so there was
        // never a diff to withhold. Treating this as missing evidence would
        // caveat every pull request that touches an image.
        let file = ChangedFile {
            path: "logo.png".into(),
            patch: None,
            additions: 0,
            deletions: 0,
            ..ChangedFile::default()
        };

        assert!(file.is_opaque());
        assert!(!file.evidence_missing());
    }

    #[test]
    fn a_truncated_patch_is_missing_evidence() {
        // The forge says lines changed and declines to say which. This is the
        // case that used to read as "nothing to review" and leave the gate
        // green over a file nobody had seen.
        let file = ChangedFile {
            path: "src/huge.rs".into(),
            patch: None,
            additions: 4000,
            deletions: 12,
            ..ChangedFile::default()
        };

        assert!(file.evidence_missing());
    }

    #[test]
    fn a_file_with_a_patch_is_never_missing_evidence() {
        let file = ChangedFile {
            path: "src/main.rs".into(),
            patch: Some("@@ -1 +1 @@\n-a\n+b\n".into()),
            additions: 1,
            deletions: 1,
            ..ChangedFile::default()
        };

        assert!(!file.is_opaque());
        assert!(!file.evidence_missing());
    }
}
