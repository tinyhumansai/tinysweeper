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
    /// Whether GitHub reports the author as `type: "Bot"`.
    ///
    /// Carried separately from the login because auto-merge's dependency-bump
    /// exemption needs both: anyone may register an account called
    /// `dependabot-ish`, but only GitHub can say an account is a bot. Defaults
    /// to `false`, so an adapter that does not know says "human", which merely
    /// costs a dependency bump its exemption.
    pub author_is_bot: bool,
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
    /// Whether it was actually merged.
    ///
    /// Distinct from "closed": issue triage may only close an issue as fixed by
    /// a pull request that *landed*, and a closed-unmerged pull request fixed
    /// nothing. Kept as a plain bool so the offline mock needs no clock.
    pub merged: bool,
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

/// A check run as it was observed on a commit.
///
/// Distinct from [`CheckRun`], which is a check being *written*: a check being
/// read may not have finished, and the difference is load-bearing for
/// `src/automerge`. A conclusion of `None` means queued or in progress, which
/// is never treated as a pass — merging while a required check is still running
/// is exactly the mistake the gate exists to avoid.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckStatus {
    /// The check name, e.g. `tinysweeper/security`.
    pub name: String,
    /// The outcome, or `None` while it is still running.
    pub conclusion: Option<CheckConclusion>,
}

impl CheckStatus {
    /// Build from the forge's wire strings.
    ///
    /// Lives here rather than in the GitHub adapter so the offline suite can
    /// test it: the default build does not compile the adapter, and this
    /// mapping is exactly the part of it that decides whether a pull request
    /// gets merged.
    ///
    /// `None` is a check that has not concluded. An unrecognised conclusion
    /// becomes [`CheckConclusion::Failure`] on purpose — a conclusion added to
    /// the API after this was written must not be read as a pass.
    pub fn from_api(name: &str, conclusion: Option<&str>) -> Self {
        Self {
            name: name.to_string(),
            conclusion: conclusion.map(|raw| match raw {
                "success" => CheckConclusion::Success,
                "neutral" => CheckConclusion::Neutral,
                "skipped" | "stale" => CheckConclusion::Skipped,
                "action_required" => CheckConclusion::ActionRequired,
                _ => CheckConclusion::Failure,
            }),
        }
    }

    /// Whether this check is affirmative evidence that it passed.
    ///
    /// Only `Success` counts. `Neutral` and `Skipped` are deliberately not
    /// green: a check that declined to run has reported nothing, and the
    /// conservative reading of "nothing" is "not yet".
    pub fn is_green(&self) -> bool {
        self.conclusion == Some(CheckConclusion::Success)
    }

    /// Whether this check has not concluded yet.
    pub fn is_pending(&self) -> bool {
        self.conclusion.is_none()
    }

    /// Whether this check is affirmative evidence that something is wrong.
    ///
    /// The complement of [`CheckStatus::is_green`] is *not* this: between the
    /// two sit `Neutral` and `Skipped`, which say "this did not apply" and
    /// "this could not run". Treating either as a failure is what would make
    /// auto-merge unreachable in practice — a workflow job with an `if:` that
    /// is false on pull requests concludes `skipped` on every pull request
    /// forever, so "not green" as a merge blocker means never merging.
    pub fn is_failing(&self) -> bool {
        self.conclusion.is_some_and(CheckConclusion::blocks)
    }

    /// Whether this check ran and found nothing to say.
    ///
    /// Only `Neutral`, which is a lane reporting that it did not apply — the
    /// `commits` lane on a pull request whose commit range holds no secret,
    /// say. `Skipped` is excluded deliberately: a check that *could not run*
    /// has produced no evidence at all, and a required check with no evidence
    /// behind it is the case the gate exists for.
    pub fn is_inapplicable(&self) -> bool {
        self.conclusion == Some(CheckConclusion::Neutral)
    }
}

/// A check run to publish.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckRun {
    /// The check name, e.g. `tinysweeper/security`.
    pub name: String,
    /// The commit it reports on.
    pub head_sha: String,
    /// The outcome, or `None` to report work still in progress.
    ///
    /// `None` is the same convention [`CheckStatus::conclusion`] already reads
    /// back, now expressible on the write side too. It exists so the umbrella
    /// review check can be published the moment a delivery is accepted, long
    /// before there is a verdict: a contributor watching a pull request should
    /// be able to tell "the reviewer has not started" from "the reviewer is
    /// thinking", and until this was an `Option` the only publishable states
    /// were the five terminal ones.
    ///
    /// A pending check refuses auto-merge — see
    /// `automerge::policy::check_refusal`, which treats *any* pending check as
    /// a refusal, not only a required one. That is the behaviour we want while
    /// a review is in flight, and it is also why every path that publishes a
    /// `None` owes the same commit a terminal conclusion afterwards. Leaving
    /// one pending is not a cosmetic bug: it stalls the merge gate forever.
    pub conclusion: Option<CheckConclusion>,
    /// One-line title shown in the checks list.
    pub title: String,
    /// The markdown summary.
    pub summary: String,
}

impl CheckRun {
    /// Whether this check reports work that has not finished.
    pub fn is_in_progress(&self) -> bool {
        self.conclusion.is_none()
    }
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
    /// Read a submitted review state off the wire.
    ///
    /// `None` for anything that carries no verdict: `DISMISSED` was retired by
    /// a human, `PENDING` was never submitted, and neither says anything about
    /// the merge button.
    pub fn from_api(state: &str) -> Option<Self> {
        match state {
            "APPROVED" => Some(ReviewEvent::Approve),
            "CHANGES_REQUESTED" => Some(ReviewEvent::RequestChanges),
            "COMMENTED" => Some(ReviewEvent::Comment),
            _ => None,
        }
    }

    /// The GitHub API's name for this event.
    pub fn as_api(self) -> &'static str {
        match self {
            ReviewEvent::Comment => "COMMENT",
            ReviewEvent::RequestChanges => "REQUEST_CHANGES",
            ReviewEvent::Approve => "APPROVE",
        }
    }
}

/// One review someone left on a pull request.
///
/// Reported in the order the reviews were submitted, so the caller can fold
/// them down to the latest verdict per reviewer itself. Doing the fold here
/// would hide the one rule that matters — a later `COMMENT` does not retire an
/// earlier `CHANGES_REQUESTED` — inside an adapter no test can reach.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewVerdict {
    /// The reviewer's login.
    pub reviewer: String,
    /// Whether the reviewer is a bot account.
    ///
    /// Decided by the forge, from the account type, rather than guessed from
    /// the login: `automerge.require_approvals` counts human approvals, and a
    /// gate that a bot can satisfy on its own is not a gate.
    pub bot: bool,
    /// The verdict itself.
    pub state: ReviewEvent,
}

impl ReviewVerdict {
    /// Whether this review counts towards `automerge.require_approvals`.
    pub fn is_human_approval(&self) -> bool {
        !self.bot && self.state == ReviewEvent::Approve
    }

    /// Whether this review blocks the merge button.
    pub fn is_blocking(&self) -> bool {
        self.state == ReviewEvent::RequestChanges
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

/// One conversation on a pull request's diff, as GraphQL reports it.
///
/// REST has no notion of a *thread*: it serves flat review comments and says
/// nothing about whether the conversation they belong to has been resolved.
/// That state only exists in GraphQL, and reading it back is what stops an
/// already-settled thread being evaluated — and resolved — on every delivery.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewThread {
    /// The GraphQL node id, which is what `resolveReviewThread` takes.
    pub id: String,
    /// Whether the conversation is already resolved.
    pub is_resolved: bool,
    /// Whether the code the thread anchors to has changed since it was written.
    ///
    /// GitHub sets this when the thread's lines no longer appear in the current
    /// diff, which is the cheapest honest evidence that somebody touched the
    /// code the objection was about. The deterministic resolve rule requires
    /// it: a finding that stopped reproducing without the code moving may just
    /// be a model that changed its mind.
    pub is_outdated: bool,
    /// Its comments, oldest first. The first one is whoever opened the thread.
    pub comments: Vec<ThreadComment>,
}

/// One comment inside a [`ReviewThread`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadComment {
    /// The login of whoever wrote it.
    pub author: String,
    /// The markdown body. Untrusted: anyone who can reply writes this.
    pub body: String,
    /// Whether the author is a bot account.
    ///
    /// Recorded from the forge rather than guessed from the login, because it
    /// decides whether a reply counts as a human asking for another look. Two
    /// bots replying to each other is the failure mode this field prevents.
    pub bot: bool,
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
    /// GitHub's native issue type, by name, when the issue carries one.
    ///
    /// A single field rather than a set, which is why triage never overwrites
    /// it: unlike a label, writing one destroys whatever a human chose.
    pub issue_type: Option<String>,
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
    fn an_unrecognised_check_conclusion_is_read_as_a_failure() {
        // The direction of this mistake is chosen deliberately. A conclusion
        // GitHub adds after this code was written must not be read as a pass by
        // the auto-merge gate; refusing to merge costs a wait, merging on a
        // conclusion nobody has seen costs a revert.
        assert_eq!(
            CheckStatus::from_api("ci", Some("success")).conclusion,
            Some(CheckConclusion::Success)
        );
        assert_eq!(
            CheckStatus::from_api("ci", Some("neutral")).conclusion,
            Some(CheckConclusion::Neutral)
        );
        assert_eq!(
            CheckStatus::from_api("ci", Some("skipped")).conclusion,
            Some(CheckConclusion::Skipped)
        );
        assert_eq!(
            CheckStatus::from_api("ci", Some("stale")).conclusion,
            Some(CheckConclusion::Skipped)
        );
        assert_eq!(
            CheckStatus::from_api("ci", Some("action_required")).conclusion,
            Some(CheckConclusion::ActionRequired)
        );
        for raw in ["failure", "cancelled", "timed_out", "something_new"] {
            assert_eq!(
                CheckStatus::from_api("ci", Some(raw)).conclusion,
                Some(CheckConclusion::Failure),
                "`{raw}` must not be mistaken for a pass"
            );
        }

        let pending = CheckStatus::from_api("ci", None);
        assert_eq!(pending.name, "ci");
        assert!(pending.is_pending());
    }

    #[test]
    fn only_submitted_review_states_carry_a_verdict() {
        assert_eq!(
            ReviewEvent::from_api("APPROVED"),
            Some(ReviewEvent::Approve)
        );
        assert_eq!(
            ReviewEvent::from_api("CHANGES_REQUESTED"),
            Some(ReviewEvent::RequestChanges)
        );
        assert_eq!(
            ReviewEvent::from_api("COMMENTED"),
            Some(ReviewEvent::Comment)
        );
        // Dismissed was retired by a human and pending was never submitted;
        // neither says anything about the merge button.
        for raw in ["DISMISSED", "PENDING", "", "approved"] {
            assert_eq!(ReviewEvent::from_api(raw), None, "`{raw}` is not a verdict");
        }
    }

    #[test]
    fn a_bot_review_is_not_a_human_approval() {
        // `automerge.require_approvals` counts *human* approvals. A bot that
        // approves every clean pull request — tinysweeper itself does — would
        // otherwise satisfy the requirement on its own, which would make the
        // setting mean nothing.
        let human = ReviewVerdict {
            reviewer: "maintainer".into(),
            bot: false,
            state: ReviewEvent::Approve,
        };
        let bot = ReviewVerdict {
            reviewer: "tinysweeper[bot]".into(),
            bot: true,
            state: ReviewEvent::Approve,
        };

        assert!(human.is_human_approval());
        assert!(!bot.is_human_approval());
        assert!(
            !ReviewVerdict {
                state: ReviewEvent::Comment,
                ..human
            }
            .is_human_approval(),
            "a plain comment is not an approval"
        );
    }

    #[test]
    fn only_a_changes_request_blocks_the_merge_button() {
        // A bot's changes-request blocks exactly as a human's does: tinysweeper
        // blocking its own auto-merge is the whole point of the gate.
        for bot in [false, true] {
            assert!(
                ReviewVerdict {
                    reviewer: "someone".into(),
                    bot,
                    state: ReviewEvent::RequestChanges,
                }
                .is_blocking()
            );
        }
        for state in [ReviewEvent::Approve, ReviewEvent::Comment] {
            assert!(
                !ReviewVerdict {
                    reviewer: "someone".into(),
                    bot: false,
                    state,
                }
                .is_blocking(),
                "{state:?} does not block"
            );
        }
    }

    #[test]
    fn a_check_is_green_only_when_it_concluded_successfully() {
        // Everything else — pending, neutral, skipped, failed — is not
        // evidence that the check passed, and the auto-merge gate treats it as
        // a refusal rather than as an absence.
        let green = CheckStatus {
            name: "ci/build".into(),
            conclusion: Some(CheckConclusion::Success),
        };
        assert!(green.is_green());

        for conclusion in [
            None,
            Some(CheckConclusion::Failure),
            Some(CheckConclusion::ActionRequired),
            Some(CheckConclusion::Neutral),
            Some(CheckConclusion::Skipped),
        ] {
            let check = CheckStatus {
                name: "ci/build".into(),
                conclusion,
            };
            assert!(!check.is_green(), "{conclusion:?} is not green");
        }
    }

    #[test]
    fn a_check_still_running_is_pending_rather_than_failed() {
        let pending = CheckStatus {
            name: "ci/build".into(),
            conclusion: None,
        };
        assert!(pending.is_pending());
        assert!(
            !CheckStatus {
                name: "ci/build".into(),
                conclusion: Some(CheckConclusion::Failure),
            }
            .is_pending()
        );
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
