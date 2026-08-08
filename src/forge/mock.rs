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
    ChangedFile, CheckRun, Commit, Issue, IssueComment, PullRequest, RepoId, ReviewComment,
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
    pub commits: BTreeMap<u64, Vec<Commit>>,
    /// Issue comments, keyed by item number.
    pub comments: BTreeMap<u64, Vec<IssueComment>>,
    /// Inline review comments, keyed by pull request number.
    pub review_comments: BTreeMap<u64, Vec<ReviewComment>>,
    /// Issues, keyed by number.
    pub issues: BTreeMap<u64, Issue>,
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
        Ok(state.commits.get(&number).cloned().unwrap_or_default())
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
                .extend(comments.iter().cloned());
        }
        self.record(Write::Review {
            number,
            body: body.to_string(),
            comments,
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
            line: 42,
            start_line: None,
            body: "finding".into(),
        };
        forge
            .create_review(&repo(), 7, "summary", vec![comment.clone()])
            .await
            .expect("posted");

        assert_eq!(
            forge.review_comments(&repo(), 7).await.expect("read"),
            vec![comment],
            "dedupe reads these back; a mock that dropped them would make every \
             dedupe test pass for the wrong reason"
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
}
