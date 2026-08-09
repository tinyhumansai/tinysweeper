//! An in-memory forge that records every write.
//!
//! This is not a stub. It is the implementation the whole test suite runs
//! against, and the one `--dry-run` uses in production to render what *would*
//! be posted. Because it records rather than discards, a test can assert on the
//! exact check runs, comments and labels a run produced — which is how the
//! noise-control rules stay honest as the lanes change.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use crate::error::{Error, Result};
use crate::forge::types::{
    ChangedFile, CheckConclusion, CheckRun, CheckStatus, Commit, Issue, IssueComment, PullRequest,
    RepoId, ReviewComment, ReviewEvent, ReviewVerdict,
};
use crate::ports::forge::{ForgeRead, ForgeWrite};

/// One thing the mock was asked to write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Write {
    /// A check run was published.
    Check(CheckRun),
    /// A new issue comment was created.
    Comment {
        /// The item it was posted on.
        number: u64,
        /// The body.
        body: String,
    },
    /// An existing comment was edited in place.
    CommentUpdate {
        /// The comment that was edited.
        comment_id: u64,
        /// The new body.
        body: String,
    },
    /// A review with inline comments was submitted.
    Review {
        /// The pull request.
        number: u64,
        /// The review body.
        body: String,
        /// The inline comments.
        comments: Vec<ReviewComment>,
        /// Whether it blocks the merge button.
        event: ReviewEvent,
    },
    /// Labels were added.
    Labels {
        /// The item.
        number: u64,
        /// The labels added.
        labels: Vec<String>,
    },
    /// A label was removed.
    LabelRemoved {
        /// The item.
        number: u64,
        /// The label removed.
        label: String,
    },
    /// An issue was closed.
    IssueClosed {
        /// The issue.
        number: u64,
    },
    /// An issue was opened.
    IssueCreated {
        /// The title.
        title: String,
        /// The body.
        body: String,
        /// The labels applied at creation.
        labels: Vec<String>,
    },
    /// A pull request was merged.
    Merged {
        /// The pull request.
        number: u64,
        /// The merge method used.
        method: String,
    },
}

/// The canned state a [`MockForge`] serves reads from.
#[derive(Debug, Clone, Default)]
pub struct MockState {
    /// Pull requests, keyed by number.
    pub pull_requests: BTreeMap<u64, PullRequest>,
    /// Changed files, keyed by pull request number.
    pub files: BTreeMap<u64, Vec<ChangedFile>>,
    /// Commits, keyed by pull request number.
    ///
    /// A `patch` set here is served by `commit_patch` and withheld from
    /// `commits`, which is how the real forge behaves.
    pub commits: BTreeMap<u64, Vec<Commit>>,
    /// Issue comments, keyed by item number.
    pub comments: BTreeMap<u64, Vec<IssueComment>>,
    /// Inline review comments, keyed by pull request number.
    pub review_comments: BTreeMap<u64, Vec<ReviewComment>>,
    /// Issues, keyed by number.
    pub issues: BTreeMap<u64, Issue>,
    /// tinysweeper's own last review state, keyed by pull request number.
    pub own_reviews: BTreeMap<u64, ReviewEvent>,
    /// Check runs, keyed by the commit they report on and then by check name.
    pub checks: BTreeMap<String, BTreeMap<String, CheckStatus>>,
    /// Reviews, oldest first, keyed by pull request number.
    pub reviews: BTreeMap<u64, Vec<ReviewVerdict>>,
    /// Repository file contents, keyed by [`file_key`].
    ///
    /// Keyed by commit as well as path because that is the distinction the
    /// knowledge centre depends on: a test has to be able to prove a file was
    /// read at the pull request's head and not at some other ref.
    pub blobs: BTreeMap<String, String>,
}

/// The key a file's contents are stored under.
///
/// A unit separator, like the index's document ids, so a path containing the
/// delimiter cannot forge a different commit's key.
pub fn file_key(sha: &str, path: &str) -> String {
    format!("{sha}\u{1f}{path}")
}

impl MockState {
    /// Serve `content` for `path` at `sha`.
    pub fn set_file(&mut self, sha: &str, path: &str, content: &str) {
        self.blobs.insert(file_key(sha, path), content.to_string());
    }

    /// Report `name` on `sha`. `conclusion: None` means still running.
    pub fn set_check(&mut self, sha: &str, name: &str, conclusion: Option<CheckConclusion>) {
        self.checks.entry(sha.to_string()).or_default().insert(
            name.to_string(),
            CheckStatus {
                name: name.to_string(),
                conclusion,
            },
        );
    }
}

/// An offline forge that serves canned reads and records every write.
#[derive(Debug, Clone, Default)]
pub struct MockForge {
    state: Arc<Mutex<MockState>>,
    writes: Arc<Mutex<Vec<Write>>>,
    next_id: Arc<Mutex<u64>>,
    /// When true, every write is dropped rather than recorded as applied.
    ///
    /// Reads still work. This is what `--dry-run` sets.
    read_only: bool,
}

impl MockForge {
    /// An empty forge.
    pub fn new() -> Self {
        Self::default()
    }

    /// A forge serving `state`.
    pub fn with_state(state: MockState) -> Self {
        Self {
            state: Arc::new(Mutex::new(state)),
            ..Self::default()
        }
    }

    /// Add a pull request, along with its files and commits.
    pub fn with_pull_request(
        self,
        pull_request: PullRequest,
        files: Vec<ChangedFile>,
        commits: Vec<Commit>,
    ) -> Self {
        {
            let mut state = self.state.lock().expect("mock state lock");
            let number = pull_request.number;
            state.pull_requests.insert(number, pull_request);
            state.files.insert(number, files);
            state.commits.insert(number, commits);
        }
        self
    }

    /// Add an issue.
    pub fn with_issue(self, issue: Issue) -> Self {
        {
            let mut state = self.state.lock().expect("mock state lock");
            state.issues.insert(issue.number, issue);
        }
        self
    }

    /// Add existing issue comments to an item.
    pub fn with_comments(self, number: u64, comments: Vec<IssueComment>) -> Self {
        {
            let mut state = self.state.lock().expect("mock state lock");
            state.comments.insert(number, comments);
        }
        self
    }

    /// Add existing inline review comments to a pull request.
    pub fn with_review_comments(self, number: u64, comments: Vec<ReviewComment>) -> Self {
        {
            let mut state = self.state.lock().expect("mock state lock");
            state.review_comments.insert(number, comments);
        }
        self
    }

    /// Report a check run on a commit.
    pub fn with_check(self, sha: &str, name: &str, conclusion: Option<CheckConclusion>) -> Self {
        {
            let mut state = self.state.lock().expect("mock state lock");
            state.set_check(sha, name, conclusion);
        }
        self
    }

    /// Add the reviews left on a pull request, oldest first.
    pub fn with_reviews(self, number: u64, reviews: Vec<ReviewVerdict>) -> Self {
        {
            let mut state = self.state.lock().expect("mock state lock");
            state.reviews.insert(number, reviews);
        }
        self
    }

    /// Pretend tinysweeper already left a review of this state.
    pub fn with_own_review(self, number: u64, event: ReviewEvent) -> Self {
        {
            let mut state = self.state.lock().expect("mock state lock");
            state.own_reviews.insert(number, event);
        }
        self
    }

    /// Simulate a push: move a pull request's head and replace its files.
    ///
    /// Everything already posted on it — review comments, the last review state
    /// — is deliberately kept, because that is what a real push does and what
    /// cross-push dedupe has to survive.
    pub fn push(&self, number: u64, head_sha: &str, files: Vec<ChangedFile>) {
        let mut state = self.state.lock().expect("mock state lock");
        if let Some(pull_request) = state.pull_requests.get_mut(&number) {
            pull_request.head_sha = head_sha.to_string();
        }
        state.files.insert(number, files);
    }

    /// Record writes but never apply them — what `--dry-run` uses.
    pub fn read_only(mut self) -> Self {
        self.read_only = true;
        self
    }

    /// Every write, in the order it was attempted.
    pub fn writes(&self) -> Vec<Write> {
        self.writes.lock().expect("mock writes lock").clone()
    }

    /// The check runs that were published, keyed by name.
    pub fn checks(&self) -> BTreeMap<String, CheckRun> {
        self.writes()
            .into_iter()
            .filter_map(|w| match w {
                Write::Check(check) => Some((check.name.clone(), check)),
                _ => None,
            })
            .collect()
    }

    /// Whether anything at all was written.
    pub fn wrote_nothing(&self) -> bool {
        self.writes().is_empty()
    }

    fn record(&self, write: Write) {
        self.writes.lock().expect("mock writes lock").push(write);
    }

    fn allocate_id(&self) -> u64 {
        let mut next = self.next_id.lock().expect("mock id lock");
        *next += 1;
        *next
    }

    fn missing(kind: &str, number: u64) -> Error {
        Error::Forge(format!("mock forge has no {kind} #{number}"))
    }
}

#[async_trait]
impl ForgeRead for MockForge {
    async fn pull_request(&self, _repo: &RepoId, number: u64) -> Result<PullRequest> {
        let state = self.state.lock().expect("mock state lock");
        state
            .pull_requests
            .get(&number)
            .cloned()
            .ok_or_else(|| Self::missing("pull request", number))
    }

    async fn changed_files(&self, _repo: &RepoId, number: u64) -> Result<Vec<ChangedFile>> {
        let state = self.state.lock().expect("mock state lock");
        Ok(state.files.get(&number).cloned().unwrap_or_default())
    }

    async fn commits(&self, _repo: &RepoId, number: u64) -> Result<Vec<Commit>> {
        let state = self.state.lock().expect("mock state lock");
        Ok(state
            .commits
            .get(&number)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            // Metadata only, exactly like the real listing endpoint. A test
            // that sees a patch here would be passing on behaviour GitHub does
            // not have, and the plumbing this mock exists to check —
            // `pull_request_context` fetching each patch — would be untested.
            .map(|commit| Commit {
                patch: None,
                ..commit
            })
            .collect())
    }

    async fn commit_patch(&self, _repo: &RepoId, sha: &str) -> Result<Option<String>> {
        let state = self.state.lock().expect("mock state lock");
        Ok(state
            .commits
            .values()
            .flatten()
            .find(|commit| commit.sha == sha)
            .and_then(|commit| commit.patch.clone()))
    }

    async fn comments(&self, _repo: &RepoId, number: u64) -> Result<Vec<IssueComment>> {
        let state = self.state.lock().expect("mock state lock");
        Ok(state.comments.get(&number).cloned().unwrap_or_default())
    }

    async fn review_comments(&self, _repo: &RepoId, number: u64) -> Result<Vec<ReviewComment>> {
        let state = self.state.lock().expect("mock state lock");
        Ok(state
            .review_comments
            .get(&number)
            .cloned()
            .unwrap_or_default())
    }

    async fn check_runs(&self, _repo: &RepoId, sha: &str) -> Result<Vec<CheckStatus>> {
        let state = self.state.lock().expect("mock state lock");
        Ok(state
            .checks
            .get(sha)
            .map(|checks| checks.values().cloned().collect())
            .unwrap_or_default())
    }

    async fn reviews(&self, _repo: &RepoId, number: u64) -> Result<Vec<ReviewVerdict>> {
        let state = self.state.lock().expect("mock state lock");
        Ok(state.reviews.get(&number).cloned().unwrap_or_default())
    }

    async fn own_review_state(&self, _repo: &RepoId, number: u64) -> Result<Option<ReviewEvent>> {
        let state = self.state.lock().expect("mock state lock");
        Ok(state.own_reviews.get(&number).copied())
    }

    async fn file_at(&self, _repo: &RepoId, path: &str, sha: &str) -> Result<Option<String>> {
        let state = self.state.lock().expect("mock state lock");
        Ok(state.blobs.get(&file_key(sha, path)).cloned())
    }

    async fn issue(&self, _repo: &RepoId, number: u64) -> Result<Issue> {
        let state = self.state.lock().expect("mock state lock");
        state
            .issues
            .get(&number)
            .cloned()
            .ok_or_else(|| Self::missing("issue", number))
    }

    async fn open_issues(&self, _repo: &RepoId, limit: usize) -> Result<Vec<Issue>> {
        let state = self.state.lock().expect("mock state lock");
        Ok(state
            .issues
            .values()
            .filter(|issue| issue.open)
            .take(limit)
            .cloned()
            .collect())
    }
}

#[async_trait]
impl ForgeWrite for MockForge {
    async fn publish_check(&self, _repo: &RepoId, check: CheckRun) -> Result<()> {
        self.record(Write::Check(check));
        Ok(())
    }

    async fn create_comment(&self, _repo: &RepoId, number: u64, body: &str) -> Result<u64> {
        self.record(Write::Comment {
            number,
            body: body.to_string(),
        });
        let id = self.allocate_id();
        if !self.read_only {
            let mut state = self.state.lock().expect("mock state lock");
            state
                .comments
                .entry(number)
                .or_default()
                .push(IssueComment {
                    id: Some(id),
                    author: "tinysweeper".into(),
                    body: body.to_string(),
                });
        }
        Ok(id)
    }

    async fn update_comment(&self, _repo: &RepoId, comment_id: u64, body: &str) -> Result<()> {
        self.record(Write::CommentUpdate {
            comment_id,
            body: body.to_string(),
        });
        if !self.read_only {
            let mut state = self.state.lock().expect("mock state lock");
            for comments in state.comments.values_mut() {
                for comment in comments.iter_mut() {
                    if comment.id == Some(comment_id) {
                        comment.body = body.to_string();
                    }
                }
            }
        }
        Ok(())
    }

    async fn create_review(
        &self,
        _repo: &RepoId,
        number: u64,
        body: &str,
        comments: Vec<ReviewComment>,
        event: ReviewEvent,
    ) -> Result<()> {
        // Applied to state as well as recorded: fingerprint dedupe reads back
        // the review comments already on a pull request, so a mock that only
        // recorded them would make every dedupe test vacuously pass.
        if !self.read_only {
            let mut state = self.state.lock().expect("mock state lock");
            state
                .review_comments
                .entry(number)
                .or_default()
                .extend(comments.iter().cloned().map(|mut comment| {
                    // The forge assigns the author, and dedupe only trusts our
                    // own. A mock that left it empty would make the three-push
                    // regression test pass for the wrong reason.
                    comment.author = "tinysweeper[bot]".into();
                    comment
                }));
            state.own_reviews.insert(number, event);
        }
        self.record(Write::Review {
            number,
            body: body.to_string(),
            comments,
            event,
        });
        Ok(())
    }

    async fn add_labels(&self, _repo: &RepoId, number: u64, labels: &[String]) -> Result<()> {
        self.record(Write::Labels {
            number,
            labels: labels.to_vec(),
        });
        if !self.read_only {
            let mut state = self.state.lock().expect("mock state lock");
            if let Some(issue) = state.issues.get_mut(&number) {
                for label in labels {
                    if !issue.labels.contains(label) {
                        issue.labels.push(label.clone());
                    }
                }
            }
            if let Some(pr) = state.pull_requests.get_mut(&number) {
                for label in labels {
                    if !pr.labels.contains(label) {
                        pr.labels.push(label.clone());
                    }
                }
            }
        }
        Ok(())
    }

    async fn remove_label(&self, _repo: &RepoId, number: u64, label: &str) -> Result<()> {
        self.record(Write::LabelRemoved {
            number,
            label: label.to_string(),
        });
        if !self.read_only {
            let mut state = self.state.lock().expect("mock state lock");
            if let Some(issue) = state.issues.get_mut(&number) {
                issue.labels.retain(|l| l != label);
            }
            if let Some(pr) = state.pull_requests.get_mut(&number) {
                pr.labels.retain(|l| l != label);
            }
        }
        Ok(())
    }

    async fn close_issue(&self, _repo: &RepoId, number: u64) -> Result<()> {
        self.record(Write::IssueClosed { number });
        if !self.read_only {
            let mut state = self.state.lock().expect("mock state lock");
            if let Some(issue) = state.issues.get_mut(&number) {
                issue.open = false;
            }
        }
        Ok(())
    }

    async fn create_issue(
        &self,
        _repo: &RepoId,
        title: &str,
        body: &str,
        labels: &[String],
    ) -> Result<u64> {
        self.record(Write::IssueCreated {
            title: title.to_string(),
            body: body.to_string(),
            labels: labels.to_vec(),
        });
        let number = self.allocate_id();
        if !self.read_only {
            let mut state = self.state.lock().expect("mock state lock");
            state.issues.insert(
                number,
                Issue {
                    number,
                    title: title.to_string(),
                    body: body.to_string(),
                    author: "tinysweeper".into(),
                    labels: labels.to_vec(),
                    open: true,
                    ..Issue::default()
                },
            );
        }
        Ok(number)
    }

    async fn merge(&self, _repo: &RepoId, number: u64, method: &str) -> Result<()> {
        self.record(Write::Merged {
            number,
            method: method.to_string(),
        });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::forge::types::CheckConclusion;

    fn repo() -> RepoId {
        RepoId::parse("tinyhumansai/tinysweeper").expect("parses")
    }

    fn pull_request(number: u64) -> PullRequest {
        PullRequest {
            number,
            title: "feat: something".into(),
            head_sha: "abc123".into(),
            ..PullRequest::default()
        }
    }

    #[tokio::test]
    async fn reads_serve_canned_state() {
        let forge = MockForge::new().with_pull_request(pull_request(7), vec![], vec![]);
        let pr = forge.pull_request(&repo(), 7).await.expect("found");
        assert_eq!(pr.title, "feat: something");
    }

    #[tokio::test]
    async fn a_missing_pull_request_is_an_error_not_a_default() {
        let forge = MockForge::new();
        let err = forge.pull_request(&repo(), 7).await.unwrap_err();
        assert!(err.to_string().contains("no pull request #7"), "{err}");
    }

    #[tokio::test]
    async fn writes_are_recorded_in_order() {
        let forge = MockForge::new();
        forge
            .publish_check(
                &repo(),
                CheckRun {
                    name: "tinysweeper/critique".into(),
                    head_sha: "abc123".into(),
                    conclusion: CheckConclusion::Success,
                    title: "No findings".into(),
                    summary: String::new(),
                },
            )
            .await
            .expect("published");
        forge
            .add_labels(&repo(), 7, &["automerge".to_string()])
            .await
            .expect("labelled");

        let writes = forge.writes();
        assert_eq!(writes.len(), 2);
        assert!(matches!(writes[0], Write::Check(_)));
        assert!(matches!(writes[1], Write::Labels { number: 7, .. }));
    }

    #[tokio::test]
    async fn checks_are_addressable_by_name() {
        let forge = MockForge::new();
        forge
            .publish_check(
                &repo(),
                CheckRun {
                    name: "tinysweeper/security".into(),
                    head_sha: "abc123".into(),
                    conclusion: CheckConclusion::Failure,
                    title: "1 high finding".into(),
                    summary: String::new(),
                },
            )
            .await
            .expect("published");

        let checks = forge.checks();
        assert_eq!(
            checks["tinysweeper/security"].conclusion,
            CheckConclusion::Failure
        );
    }

    #[tokio::test]
    async fn a_created_comment_becomes_readable() {
        let forge = MockForge::new();
        let id = forge.create_comment(&repo(), 7, "hello").await.expect("id");
        let comments = forge.comments(&repo(), 7).await.expect("read");

        assert_eq!(comments.len(), 1);
        assert_eq!(comments[0].id, Some(id));
        assert_eq!(comments[0].body, "hello");
    }

    #[tokio::test]
    async fn a_comment_is_edited_in_place() {
        let forge = MockForge::new();
        let id = forge.create_comment(&repo(), 7, "first").await.expect("id");
        forge
            .update_comment(&repo(), id, "second")
            .await
            .expect("updated");

        let comments = forge.comments(&repo(), 7).await.expect("read");
        assert_eq!(
            comments.len(),
            1,
            "editing must not create a second comment"
        );
        assert_eq!(comments[0].body, "second");
    }

    #[tokio::test]
    async fn posted_review_comments_become_readable_for_dedupe() {
        let forge = MockForge::new();
        let comment = ReviewComment {
            path: "src/lib.rs".into(),
            line: Some(42),
            start_line: None,
            author: String::new(),
            body: "finding".into(),
        };
        forge
            .create_review(
                &repo(),
                7,
                "summary",
                vec![comment.clone()],
                ReviewEvent::Comment,
            )
            .await
            .expect("posted");

        let read_back = forge.review_comments(&repo(), 7).await.expect("read");
        assert_eq!(
            read_back.len(),
            1,
            "dedupe reads these back; a mock that dropped them would make every \
             dedupe test pass for the wrong reason"
        );
        assert_eq!(read_back[0].body, comment.body);
        assert_eq!(
            read_back[0].author, "tinysweeper[bot]",
            "the forge assigns the author, and dedupe only trusts our own"
        );
    }

    #[tokio::test]
    async fn read_only_records_the_write_but_does_not_apply_it() {
        let forge = MockForge::new().read_only();
        forge
            .create_comment(&repo(), 7, "would have posted this")
            .await
            .expect("recorded");

        assert_eq!(forge.writes().len(), 1, "the intent is still recorded");
        assert!(
            forge.comments(&repo(), 7).await.expect("read").is_empty(),
            "but nothing was actually applied"
        );
    }

    #[tokio::test]
    async fn closing_an_issue_flips_it_shut() {
        let forge = MockForge::new().with_issue(Issue {
            number: 3,
            open: true,
            ..Issue::default()
        });
        forge.close_issue(&repo(), 3).await.expect("closed");

        assert!(!forge.issue(&repo(), 3).await.expect("read").open);
        assert!(
            forge
                .open_issues(&repo(), 10)
                .await
                .expect("read")
                .is_empty()
        );
    }

    #[tokio::test]
    async fn labels_are_not_duplicated() {
        let forge = MockForge::new().with_issue(Issue {
            number: 3,
            labels: vec!["bug".into()],
            open: true,
            ..Issue::default()
        });
        forge
            .add_labels(&repo(), 3, &["bug".to_string(), "triage".to_string()])
            .await
            .expect("labelled");

        assert_eq!(
            forge.issue(&repo(), 3).await.expect("read").labels,
            vec!["bug".to_string(), "triage".to_string()]
        );
    }

    #[tokio::test]
    async fn the_context_helper_fetches_everything_in_one_call() {
        let forge = MockForge::new().with_pull_request(
            pull_request(7),
            vec![ChangedFile {
                path: "src/lib.rs".into(),
                ..ChangedFile::default()
            }],
            vec![Commit {
                sha: "abc123".into(),
                message: "feat: something".into(),
                ..Commit::default()
            }],
        );

        let context = forge
            .pull_request_context(&repo(), 7)
            .await
            .expect("context");
        assert_eq!(context.pull_request.number, 7);
        assert_eq!(context.files.len(), 1);
        assert_eq!(context.commits.len(), 1);
    }

    #[tokio::test]
    async fn the_context_helper_fetches_a_patch_for_every_commit() {
        // The listing endpoint returns metadata; the patch is a second call.
        // The `commits` lane cannot review `git log -p` unless this helper
        // makes it — issue #47.
        let forge = MockForge::new().with_pull_request(
            pull_request(7),
            Vec::new(),
            vec![Commit {
                sha: "abc123".into(),
                message: "feat: something".into(),
                patch: Some("--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1 +1 @@\n+ok\n".into()),
                ..Commit::default()
            }],
        );

        assert_eq!(
            forge.commits(&repo(), 7).await.expect("listed")[0].patch,
            None,
            "the listing endpoint returns metadata only"
        );

        let context = forge
            .pull_request_context(&repo(), 7)
            .await
            .expect("context");
        assert!(
            context.commits[0]
                .patch
                .as_deref()
                .is_some_and(|patch| patch.contains("+ok")),
            "{:?}",
            context.commits[0].patch
        );
    }

    #[tokio::test]
    async fn check_runs_are_served_per_commit_not_per_pull_request() {
        // Keyed on the SHA because that is the question the auto-merge gate
        // asks: a check that is green on the previous head says nothing about
        // the commit about to be merged.
        let mut state = MockState::default();
        state.set_check("abc123", "ci/build", Some(CheckConclusion::Success));
        state.set_check("def456", "ci/build", Some(CheckConclusion::Failure));
        let forge = MockForge::with_state(state);

        let checks = forge.check_runs(&repo(), "abc123").await.expect("read");
        assert_eq!(checks.len(), 1);
        assert!(checks[0].is_green());

        assert!(
            !forge.check_runs(&repo(), "def456").await.expect("read")[0].is_green(),
            "a different commit has its own checks"
        );
        assert!(
            forge
                .check_runs(&repo(), "unknown")
                .await
                .expect("read")
                .is_empty(),
            "an unreported commit has no checks, which is not an error"
        );
    }

    #[tokio::test]
    async fn reviews_are_served_in_submission_order() {
        // The fold to a latest verdict per reviewer belongs to the policy, so
        // the port has to hand over the history rather than a summary.
        let forge = MockForge::new().with_reviews(
            7,
            vec![
                ReviewVerdict {
                    reviewer: "maintainer".into(),
                    bot: false,
                    state: ReviewEvent::RequestChanges,
                },
                ReviewVerdict {
                    reviewer: "maintainer".into(),
                    bot: false,
                    state: ReviewEvent::Approve,
                },
            ],
        );

        let reviews = forge.reviews(&repo(), 7).await.expect("read");
        assert_eq!(reviews.len(), 2);
        assert_eq!(reviews[0].state, ReviewEvent::RequestChanges);
        assert_eq!(reviews[1].state, ReviewEvent::Approve);
    }

    #[tokio::test]
    async fn an_unknown_commit_has_no_patch_rather_than_an_error() {
        let forge = MockForge::new();
        assert_eq!(
            forge.commit_patch(&repo(), "nope").await.expect("read"),
            None
        );
    }
}
