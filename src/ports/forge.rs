//! The forge port: everything tinysweeper does to GitHub.
//!
//! The split between the read half and the write half is deliberate and
//! load-bearing. Lanes take a [`ForgeRead`]; only `src/apply` takes a
//! [`ForgeWrite`]. A lane therefore *cannot* mutate a pull request even by
//! mistake, because it never holds a handle that could — the security boundary
//! in `AGENTS.md` is enforced by the type system rather than by discipline.

use async_trait::async_trait;

use crate::error::Result;
use crate::forge::types::{
    ChangedFile, CheckRun, Commit, Issue, IssueComment, PullRequest, PullRequestContext, RepoId,
    ReviewComment, ReviewEvent,
};

/// Read access to a forge. This is what lanes get.
#[async_trait]
pub trait ForgeRead: Send + Sync {
    /// Fetch a pull request.
    async fn pull_request(&self, repo: &RepoId, number: u64) -> Result<PullRequest>;

    /// Fetch the files a pull request changed.
    async fn changed_files(&self, repo: &RepoId, number: u64) -> Result<Vec<ChangedFile>>;

    /// Fetch the commits in a pull request's range.
    async fn commits(&self, repo: &RepoId, number: u64) -> Result<Vec<Commit>>;

    /// Fetch the issue comments on a pull request or issue.
    async fn comments(&self, repo: &RepoId, number: u64) -> Result<Vec<IssueComment>>;

    /// Fetch the inline review comments on a pull request.
    ///
    /// Used for fingerprint dedupe: a finding already posted is never posted
    /// again, across pushes.
    async fn review_comments(&self, repo: &RepoId, number: u64) -> Result<Vec<ReviewComment>>;

    /// The state of tinysweeper's own most recent review on a pull request.
    ///
    /// `None` when it has never reviewed. Used to clear a stale
    /// changes-requested verdict, which GitHub will otherwise leave blocking
    /// the merge button forever.
    async fn own_review_state(&self, repo: &RepoId, number: u64)
    -> Result<Option<ReviewEvent>>;

    /// Fetch an issue.
    async fn issue(&self, repo: &RepoId, number: u64) -> Result<Issue>;

    /// List open issues, most recently updated first.
    async fn open_issues(&self, repo: &RepoId, limit: usize) -> Result<Vec<Issue>>;

    /// Fetch everything a lane needs about a pull request in one go.
    ///
    /// Default-implemented in terms of the calls above so an adapter only has
    /// to override it when the forge offers something cheaper.
    async fn pull_request_context(&self, repo: &RepoId, number: u64) -> Result<PullRequestContext> {
        Ok(PullRequestContext {
            pull_request: self.pull_request(repo, number).await?,
            files: self.changed_files(repo, number).await?,
            commits: self.commits(repo, number).await?,
            comments: self.comments(repo, number).await?,
            checks: Default::default(),
        })
    }
}

/// Write access to a forge. Only `src/apply` gets one of these, and only after
/// every model call has returned.
#[async_trait]
pub trait ForgeWrite: Send + Sync {
    /// Create or update a check run.
    async fn publish_check(&self, repo: &RepoId, check: CheckRun) -> Result<()>;

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

    /// Remove a label. Absent labels are not an error.
    async fn remove_label(&self, repo: &RepoId, number: u64, label: &str) -> Result<()>;

    /// Close an issue.
    async fn close_issue(&self, repo: &RepoId, number: u64) -> Result<()>;

    /// Open an issue, returning its number.
    async fn create_issue(
        &self,
        repo: &RepoId,
        title: &str,
        body: &str,
        labels: &[String],
    ) -> Result<u64>;

    /// Merge a pull request.
    ///
    /// Callers must have already re-validated live state; this method does no
    /// policy checking of its own.
    async fn merge(&self, repo: &RepoId, number: u64, method: &str) -> Result<()>;
}
