//! The forge port: everything tinysweeper does to GitHub.
//!
//! The split between the read half and the write half is deliberate and
//! load-bearing. Lanes take a [`ForgeRead`]; only `src/apply` takes a
//! [`ForgeWrite`]. A lane therefore *cannot* mutate a pull request even by
//! mistake, because it never holds a handle that could — the security boundary
//! in `AGENTS.md` is enforced by the type system rather than by discipline.
//!
//! [`ForgeWrite::merge`] carries the same idea one level further: it takes a
//! [`MergeApproved`] that only the auto-merge policy can mint, so the one
//! operation that can put code on the default branch unattended cannot be
//! called at all without a passing gate.

use async_trait::async_trait;

use crate::automerge::policy::MergeApproved;
use crate::error::Result;
use crate::forge::types::{
    ChangedFile, CheckRun, CheckStatus, Commit, Issue, IssueComment, PullRequest,
    PullRequestContext, RepoId, ReviewComment, ReviewEvent, ReviewThread, ReviewVerdict,
};

/// How many commits of a range get their patch fetched.
///
/// Matches the number the `commits` lane will render, so the calls that are
/// made are the calls whose answers are used. Commits past it arrive with
/// `patch: None`, which the lane reports rather than hides.
pub const MAX_PATCHED_COMMITS: usize = 50;

/// Read access to a forge. This is what lanes get.
#[async_trait]
pub trait ForgeRead: Send + Sync {
    /// Fetch a pull request.
    async fn pull_request(&self, repo: &RepoId, number: u64) -> Result<PullRequest>;

    /// Fetch the files a pull request changed.
    async fn changed_files(&self, repo: &RepoId, number: u64) -> Result<Vec<ChangedFile>>;

    /// Fetch the commits in a pull request's range, as metadata only.
    ///
    /// The patch is a separate call on every forge worth supporting, so it is
    /// a separate method here: [`commit_patch`](Self::commit_patch).
    async fn commits(&self, repo: &RepoId, number: u64) -> Result<Vec<Commit>>;

    /// Fetch the unified patch one commit introduced.
    ///
    /// `None` when the forge has no patch to give — a merge commit, a commit
    /// whose diff the forge declines to render — which is not an error: the
    /// `commits` lane renders the absence rather than inventing a diff.
    async fn commit_patch(&self, repo: &RepoId, sha: &str) -> Result<Option<String>>;

    /// Fetch the issue comments on a pull request or issue.
    async fn comments(&self, repo: &RepoId, number: u64) -> Result<Vec<IssueComment>>;

    /// Fetch the inline review comments on a pull request.
    ///
    /// Used for fingerprint dedupe: a finding already posted is never posted
    /// again, across pushes.
    async fn review_comments(&self, repo: &RepoId, number: u64) -> Result<Vec<ReviewComment>>;

    /// The check runs reported against one commit.
    ///
    /// Pinned to a SHA rather than to a pull request number on purpose: the
    /// auto-merge gate asks whether *this commit* is green, and a check that
    /// passed on the previous head says nothing about the one about to be
    /// merged. A commit nothing has reported on yields an empty list, which is
    /// not an error — and is not a pass either.
    ///
    /// No default implementation. An adapter that forgot to answer would
    /// otherwise report "no checks", and a gate that cannot see a red check is
    /// worse than no gate.
    async fn check_runs(&self, repo: &RepoId, sha: &str) -> Result<Vec<CheckStatus>>;

    /// Every review left on a pull request, oldest first.
    ///
    /// The history rather than a verdict: only the caller knows that a later
    /// `COMMENT` must not retire an earlier `CHANGES_REQUESTED`, and folding
    /// here would bury that rule in an adapter no offline test can reach.
    /// Dismissed reviews are omitted — they no longer block anything.
    async fn reviews(&self, repo: &RepoId, number: u64) -> Result<Vec<ReviewVerdict>>;

    /// The review conversations on a pull request, with their resolved state.
    ///
    /// No default implementation, for the same reason `check_runs` has none: an
    /// adapter that forgot to answer would report "no threads", and thread
    /// resolution would silently become a no-op nobody noticed.
    async fn review_threads(&self, repo: &RepoId, number: u64) -> Result<Vec<ReviewThread>>;

    /// The state of tinysweeper's own most recent review on a pull request.
    ///
    /// `None` when it has never reviewed. Used to clear a stale
    /// changes-requested verdict, which GitHub will otherwise leave blocking
    /// the merge button forever.
    async fn own_review_state(&self, repo: &RepoId, number: u64) -> Result<Option<ReviewEvent>>;

    /// Fetch one file's contents at a commit.
    ///
    /// `None` when the file does not exist there, which is the common answer
    /// and not an error: most repositories have no `AGENTS.md`.
    ///
    /// Pinned to a commit rather than a branch on purpose. The knowledge
    /// centre reads the repository's instruction files through this, and
    /// reading them at a moving ref would mean reviewing one tree while
    /// applying another tree's policy — and would let a push land new policy
    /// between the review starting and the file being read.
    async fn file_at(&self, repo: &RepoId, path: &str, sha: &str) -> Result<Option<String>>;

    /// Fetch an issue.
    async fn issue(&self, repo: &RepoId, number: u64) -> Result<Issue>;

    /// List open issues, most recently updated first.
    async fn open_issues(&self, repo: &RepoId, limit: usize) -> Result<Vec<Issue>>;

    /// The commit a branch currently points at.
    ///
    /// `None` when the branch is not there, which is an ordinary answer: a pull
    /// request can outlive the branch it targets.
    ///
    /// Exists so `crate::pr_triage::landed` can read every file of one pull
    /// request at a **single** revision. Reading them at the branch *name*
    /// resolves independently per file, so a base branch that moves mid-sweep
    /// can serve file A from one commit and file B from a later one — and a
    /// change can then look landed when no single revision contains all of it.
    async fn branch_head(&self, repo: &RepoId, branch: &str) -> Result<Option<String>>;

    /// List open pull requests, oldest first.
    ///
    /// Oldest first, and that ordering is load-bearing rather than cosmetic:
    /// `crate::pr_triage::dedupe` calls the *older* of two near-identical pull
    /// requests the original and the newer one the duplicate, and a caller that
    /// paged newest-first would truncate away the originals and leave a
    /// shortlist of duplicates with nothing to be duplicates of.
    ///
    /// No default implementation. An adapter that forgot to answer would
    /// report an empty repository, and a duplicate sweep that can see no other
    /// pull requests silently concludes that nothing is a duplicate.
    async fn open_pull_requests(&self, repo: &RepoId, limit: usize) -> Result<Vec<PullRequest>>;

    /// The names of the issue types the repository's owner defines.
    ///
    /// Read rather than hard-coded: "Bug", "Feature" and "Task" are only
    /// GitHub's defaults, and another organisation renames or replaces them.
    /// An owner with no issue types — a user account, or an organisation that
    /// never enabled them — yields an empty list, which is not an error and
    /// means triage sets no type at all.
    async fn issue_types(&self, repo: &RepoId) -> Result<Vec<String>>;

    /// Search issues in one repository, in GitHub issue-search syntax.
    ///
    /// The adapter scopes the query to `repo`; callers pass only the terms.
    ///
    /// **Open *and* closed issues are returned** unless the query narrows it.
    /// The Sentry dedupe path depends on that: a promoted issue somebody has
    /// since fixed and closed must still be found, or every sweep after the
    /// fix reopens the same report. That is also why this is a search rather
    /// than a filter over [`Self::open_issues`].
    ///
    /// Search is best-effort by nature — GitHub's index is eventually
    /// consistent and its own rate limit is separate and low. A caller that
    /// must not act on a stale answer should verify the hit it gets, and the
    /// dedupe path does exactly that against the full issue body.
    async fn search_issues(&self, repo: &RepoId, query: &str) -> Result<Vec<Issue>>;

    /// Fetch everything a lane needs about a pull request in one go.
    ///
    /// Default-implemented in terms of the calls above so an adapter only has
    /// to override it when the forge offers something cheaper.
    async fn pull_request_context(&self, repo: &RepoId, number: u64) -> Result<PullRequestContext> {
        let mut commits = self.commits(repo, number).await?;

        // One request per commit, so the count is capped rather than left to
        // the branch. A two-hundred-commit branch would otherwise spend two
        // hundred API calls — and the whole rate-limit budget of a review — on
        // patches the `commits` lane will not even render.
        for commit in commits.iter_mut().take(MAX_PATCHED_COMMITS) {
            commit.patch = self.commit_patch(repo, &commit.sha).await?;
        }

        Ok(PullRequestContext {
            pull_request: self.pull_request(repo, number).await?,
            files: self.changed_files(repo, number).await?,
            commits,
            comments: self.comments(repo, number).await?,
            checks: Default::default(),
        })
    }
}

/// Write access to a forge. Only `src/apply` gets one of these, and only after
/// every model call has returned.
#[async_trait]
pub trait ForgeWrite: Send + Sync {
    /// Create a check run, returning its id.
    ///
    /// The id is returned so a check published as in-progress can be concluded
    /// later through [`update_check`](Self::update_check). Posting a second
    /// check run of the same name does *not* replace the first — GitHub keeps
    /// both, and the pull request grows a duplicate row that never concludes —
    /// so the id is the only way to finish what this started.
    async fn publish_check(&self, repo: &RepoId, check: CheckRun) -> Result<u64>;

    /// Replace an existing check run, by the id [`publish_check`] returned.
    ///
    /// Separate from `publish_check` rather than an `Option<u64>` parameter on
    /// it, because the two have genuinely different preconditions: creating
    /// needs a commit, updating needs a check that is already on that commit.
    ///
    /// [`publish_check`]: Self::publish_check
    async fn update_check(&self, repo: &RepoId, check_id: u64, check: CheckRun) -> Result<()>;

    /// Create an issue comment, returning its id.
    async fn create_comment(&self, repo: &RepoId, number: u64, body: &str) -> Result<u64>;

    /// Replace the body of an existing issue comment.
    ///
    /// tinysweeper keeps exactly one durable comment per item and edits it in
    /// place forever, so this is the common path and `create_comment` is the
    /// rare one.
    async fn update_comment(&self, repo: &RepoId, comment_id: u64, body: &str) -> Result<()>;

    /// Post inline review comments as a single review.
    ///
    /// `event` decides whether the review blocks the merge button. Callers are
    /// responsible for clearing a previous block with
    /// [`ReviewEvent::Approve`] once the findings are gone; GitHub keeps only
    /// the latest review per reviewer, and a stale objection blocks forever.
    async fn create_review(
        &self,
        repo: &RepoId,
        number: u64,
        body: &str,
        comments: Vec<ReviewComment>,
        event: ReviewEvent,
    ) -> Result<()>;

    /// Add labels to an issue or pull request.
    async fn add_labels(&self, repo: &RepoId, number: u64, labels: &[String]) -> Result<()>;

    /// Set an issue's native issue type, by type name.
    ///
    /// The type is a single field, so this *replaces* whatever is there.
    /// Callers must have established that the issue carries no type yet;
    /// nothing here can tell a human's choice from an empty one.
    async fn set_issue_type(&self, repo: &RepoId, number: u64, type_name: &str) -> Result<()>;

    /// Remove a label. Absent labels are not an error.
    async fn remove_label(&self, repo: &RepoId, number: u64, label: &str) -> Result<()>;

    /// Close an issue.
    async fn close_issue(&self, repo: &RepoId, number: u64) -> Result<()>;

    /// Close a pull request without merging it.
    ///
    /// Separate from [`close_issue`](Self::close_issue) even though GitHub's
    /// issues endpoint would close either one. Two reasons, and the second is
    /// the real one: an adapter for a forge that does not conflate the two
    /// needs somewhere to differ, and a `MockForge` that records the two as one
    /// write would let a test asserting "it closed the issue" pass on a run
    /// that closed a pull request instead.
    ///
    /// Nothing here merges. The only path to the default branch is
    /// [`merge`](Self::merge), which needs a `MergeApproved` this cannot mint.
    async fn close_pull_request(&self, repo: &RepoId, number: u64) -> Result<()>;

    /// Open an issue, returning its number.
    async fn create_issue(
        &self,
        repo: &RepoId,
        title: &str,
        body: &str,
        labels: &[String],
    ) -> Result<u64>;

    /// Reply in one review conversation, by GraphQL node id.
    ///
    /// Exists so [`resolve_review_thread`](Self::resolve_review_thread) does
    /// not have to happen silently. A thread that collapses with no explanation
    /// looks like the bot losing interest; the same thread with "addressed in
    /// `abc1234`" under it is a claim the author can check, and disagree with,
    /// against a specific commit.
    ///
    /// The body is written by this crate, never by a model — see
    /// `threads::resolution_note`.
    async fn reply_to_review_thread(
        &self,
        repo: &RepoId,
        thread_id: &str,
        body: &str,
    ) -> Result<()>;

    /// Resolve one review conversation, by GraphQL node id.
    ///
    /// A mutation, so it lives here and never on the read half a lane holds: a
    /// model verdict about a thread is advisory, and the decision to call this
    /// is taken by deterministic policy in `crate::threads`.
    async fn resolve_review_thread(&self, repo: &RepoId, thread_id: &str) -> Result<()>;

    /// Merge the pull request `approval` was granted for.
    ///
    /// ## The approval is the precondition, in the type system
    ///
    /// [`MergeApproved`] can only be minted by
    /// [`automerge::policy::evaluate`](crate::automerge::policy::evaluate)
    /// returning `Allow`, so a merge without a passing gate does not compile.
    /// Before this, the guarantee was that the only call site in the tree sat
    /// behind two evaluations — true, but discipline rather than structure,
    /// and exactly the distinction `AGENTS.md` is making when it separates
    /// `ForgeRead` from `ForgeWrite`. This is that argument one level down, on
    /// the one operation that can put code on the default branch with nobody
    /// watching.
    ///
    /// The number comes from `approval` rather than from a parameter of its
    /// own, so approving one pull request and merging another is not an
    /// expressible operation.
    ///
    /// ## What it still does not promise
    ///
    /// That the approval is *current*. A witness records that the policy
    /// passed against some snapshot, not that the snapshot still holds, so
    /// callers must still re-validate live state — `automerge::merge_snapshot`
    /// re-reads and re-evaluates, and compares head SHAs, before it gets here.
    /// This method does no policy checking of its own.
    ///
    /// ## Why this trait names an `automerge` type
    ///
    /// The dependency looks inverted, and is deliberate: a private field
    /// restricts minting to the defining module, so the witness has to be
    /// declared where the decision is made. Declaring it here instead would
    /// mean any module in the crate could construct one, which is the property
    /// being bought.
    async fn merge(&self, repo: &RepoId, approval: &MergeApproved, method: &str) -> Result<()>;
}
