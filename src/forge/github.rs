//! The real GitHub adapter, behind the `github` feature.
//!
//! Split into two types on purpose, mirroring the ports: [`GitHubRead`] is what
//! the review path gets, [`GitHubWrite`] is what only `apply` gets. They are
//! separate structs rather than one struct implementing both traits, so a
//! read-only caller cannot upcast its way into write access.

use async_trait::async_trait;
use octocrab::Octocrab;

use crate::error::{Error, Result};
use crate::forge::types::{
    ChangedFile, CheckConclusion, CheckRun, Commit, FileStatus, Issue, IssueComment, PullRequest,
    RepoId, ReviewComment,
};
use crate::ports::forge::{ForgeRead, ForgeWrite};

fn client(token: &str) -> Result<Octocrab> {
    Octocrab::builder()
        .personal_token(token.to_string())
        .build()
        .map_err(|err| Error::Forge(format!("could not build a GitHub client: {err}")))
}

fn api(err: octocrab::Error) -> Error {
    Error::Forge(err.to_string())
}

/// Read-only GitHub access.
pub struct GitHubRead {
    client: Octocrab,
}

impl GitHubRead {
    /// Build from a token.
    pub fn new(token: &str) -> Result<Self> {
        Ok(Self {
            client: client(token)?,
        })
    }

    /// Build from `GITHUB_TOKEN`.
    pub fn from_env() -> Result<Self> {
        let token = std::env::var("GITHUB_TOKEN")
            .map_err(|_| Error::Forge("GITHUB_TOKEN is not set".into()))?;
        Self::new(&token)
    }
}

#[async_trait]
impl ForgeRead for GitHubRead {
    async fn pull_request(&self, repo: &RepoId, number: u64) -> Result<PullRequest> {
        let pr = self
            .client
            .pulls(&repo.owner, &repo.name)
            .get(number)
            .await
            .map_err(api)?;

        let base = pr.base;
        let head = pr.head;

        Ok(PullRequest {
            number,
            title: pr.title.unwrap_or_default(),
            body: pr.body.unwrap_or_default(),
            author: pr.user.map(|u| u.login).unwrap_or_default(),
            draft: pr.draft.unwrap_or(false),
            base_ref: base.ref_field,
            base_sha: base.sha,
            head_ref: head.ref_field.clone(),
            head_sha: head.sha,
            // `head.repo` differing from `base.repo` is what makes a pull
            // request a fork one, and that changes which token can post.
            from_fork: head
                .repo
                .as_ref()
                .and_then(|r| r.owner.as_ref().map(|o| o.login.clone()))
                .map(|owner| owner != repo.owner)
                .unwrap_or(false),
            labels: pr
                .labels
                .unwrap_or_default()
                .into_iter()
                .map(|l| l.name)
                .collect(),
            mergeable: pr.mergeable,
            approvals: 0,
        })
    }

    async fn changed_files(&self, repo: &RepoId, number: u64) -> Result<Vec<ChangedFile>> {
        let mut page = self
            .client
            .pulls(&repo.owner, &repo.name)
            .list_files(number)
            .await
            .map_err(api)?;

        let mut files = Vec::new();
        loop {
            for file in &page.items {
                files.push(ChangedFile {
                    path: file.filename.clone(),
                    previous_path: file.previous_filename.clone(),
                    status: match file.status {
                        octocrab::models::repos::DiffEntryStatus::Added => FileStatus::Added,
                        octocrab::models::repos::DiffEntryStatus::Removed => FileStatus::Removed,
                        octocrab::models::repos::DiffEntryStatus::Renamed => FileStatus::Renamed,
                        _ => FileStatus::Modified,
                    },
                    additions: file.additions,
                    deletions: file.deletions,
                    patch: file.patch.clone(),
                    // The files endpoint does not report size; the blob scanner
                    // treats `None` as "unknown" rather than "small", so this
                    // is honest rather than a silent zero.
                    size_bytes: None,
                });
            }
            match self.client.get_page(&page.next).await.map_err(api)? {
                Some(next) => page = next,
                None => break,
            }
        }

        Ok(files)
    }

    async fn commits(&self, repo: &RepoId, number: u64) -> Result<Vec<Commit>> {
        let page = self
            .client
            .pulls(&repo.owner, &repo.name)
            .pr_commits(number)
            .await
            .map_err(api)?;

        Ok(page
            .items
            .into_iter()
            .map(|c| Commit {
                sha: c.sha,
                message: c.commit.message,
                author_name: c.commit.author.as_ref().map(|a| a.name.clone()).unwrap_or_default(),
                author_email: c
                    .commit
                    .author
                    .as_ref()
                    .map(|a| a.email.clone())
                    .unwrap_or_default(),
            })
            .collect())
    }

    async fn comments(&self, repo: &RepoId, number: u64) -> Result<Vec<IssueComment>> {
        let page = self
            .client
            .issues(&repo.owner, &repo.name)
            .list_comments(number)
            .send()
            .await
            .map_err(api)?;

        Ok(page
            .items
            .into_iter()
            .map(|c| IssueComment {
                id: Some(*c.id),
                author: c.user.login,
                body: c.body.unwrap_or_default(),
            })
            .collect())
    }

    async fn review_comments(&self, repo: &RepoId, number: u64) -> Result<Vec<ReviewComment>> {
        let page = self
            .client
            .pulls(&repo.owner, &repo.name)
            .list_comments(Some(number))
            .send()
            .await
            .map_err(api)?;

        Ok(page
            .items
            .into_iter()
            .map(|c| ReviewComment {
                path: c.path,
                line: c.line.unwrap_or(0),
                start_line: c.start_line,
                body: c.body,
            })
            .collect())
    }

    async fn issue(&self, repo: &RepoId, number: u64) -> Result<Issue> {
        let issue = self
            .client
            .issues(&repo.owner, &repo.name)
            .get(number)
            .await
            .map_err(api)?;

        Ok(Issue {
            number,
            title: issue.title,
            body: issue.body.unwrap_or_default(),
            author: issue.user.login,
            labels: issue.labels.into_iter().map(|l| l.name).collect(),
            open: matches!(issue.state, octocrab::models::IssueState::Open),
            age_days: 0,
            quiet_days: 0,
            comments: issue.comments,
        })
    }

    async fn open_issues(&self, repo: &RepoId, limit: usize) -> Result<Vec<Issue>> {
        let page = self
            .client
            .issues(&repo.owner, &repo.name)
            .list()
            .state(octocrab::params::State::Open)
            .per_page(limit.min(100) as u8)
            .send()
            .await
            .map_err(api)?;

        Ok(page
            .items
            .into_iter()
            // The issues endpoint returns pull requests too, which are not
            // issues for triage purposes.
            .filter(|i| i.pull_request.is_none())
            .map(|i| Issue {
                number: i.number,
                title: i.title,
                body: i.body.unwrap_or_default(),
                author: i.user.login,
                labels: i.labels.into_iter().map(|l| l.name).collect(),
                open: true,
                age_days: 0,
                quiet_days: 0,
                comments: i.comments,
            })
            .collect())
    }
}

/// Write access. Constructed only by `apply`.
pub struct GitHubWrite {
    client: Octocrab,
}

impl GitHubWrite {
    /// Build from a token.
    pub fn new(token: &str) -> Result<Self> {
        Ok(Self {
            client: client(token)?,
        })
    }

    /// Build from `GITHUB_TOKEN`.
    pub fn from_env() -> Result<Self> {
        let token = std::env::var("GITHUB_TOKEN")
            .map_err(|_| Error::Forge("GITHUB_TOKEN is not set".into()))?;
        Self::new(&token)
    }
}

#[async_trait]
impl ForgeWrite for GitHubWrite {
    async fn publish_check(&self, repo: &RepoId, check: CheckRun) -> Result<()> {
        use octocrab::params::checks::{CheckRunConclusion, CheckRunStatus};

        let conclusion = match check.conclusion {
            CheckConclusion::Success => CheckRunConclusion::Success,
            CheckConclusion::Failure => CheckRunConclusion::Failure,
            CheckConclusion::ActionRequired => CheckRunConclusion::ActionRequired,
            CheckConclusion::Neutral => CheckRunConclusion::Neutral,
            CheckConclusion::Skipped => CheckRunConclusion::Skipped,
        };

        self.client
            .checks(&repo.owner, &repo.name)
            .create_check_run(check.name, check.head_sha)
            .status(CheckRunStatus::Completed)
            .conclusion(conclusion)
            .output(octocrab::models::checks::CheckRunOutput {
                title: check.title,
                summary: check.summary,
                text: None,
                annotations: vec![],
                images: vec![],
            })
            .send()
            .await
            .map_err(api)?;

        Ok(())
    }

    async fn create_comment(&self, repo: &RepoId, number: u64, body: &str) -> Result<u64> {
        let comment = self
            .client
            .issues(&repo.owner, &repo.name)
            .create_comment(number, body)
            .await
            .map_err(api)?;
        Ok(*comment.id)
    }

    async fn update_comment(&self, repo: &RepoId, comment_id: u64, body: &str) -> Result<()> {
        self.client
            .issues(&repo.owner, &repo.name)
            .update_comment(octocrab::models::CommentId(comment_id), body)
            .await
            .map_err(api)?;
        Ok(())
    }

    async fn create_review(
        &self,
        repo: &RepoId,
        number: u64,
        body: &str,
        comments: Vec<ReviewComment>,
    ) -> Result<()> {
        // Deliberately COMMENT rather than REQUEST_CHANGES. The check runs are
        // what gate a merge; a formal changes-requested verdict from a bot also
        // has to be dismissed by a human before anything can proceed, which
        // turns every false positive into manual work.
        let mut review = serde_json::json!({
            "body": body,
            "event": "COMMENT",
            "comments": comments
                .iter()
                .map(|c| serde_json::json!({
                    "path": c.path,
                    "line": c.line,
                    "body": c.body,
                }))
                .collect::<Vec<_>>(),
        });
        if comments.is_empty() {
            review["comments"] = serde_json::json!([]);
        }

        let route = format!("/repos/{}/{}/pulls/{number}/reviews", repo.owner, repo.name);
        let _: serde_json::Value = self
            .client
            .post(route, Some(&review))
            .await
            .map_err(api)?;
        Ok(())
    }

    async fn add_labels(&self, repo: &RepoId, number: u64, labels: &[String]) -> Result<()> {
        self.client
            .issues(&repo.owner, &repo.name)
            .add_labels(number, labels)
            .await
            .map_err(api)?;
        Ok(())
    }

    async fn remove_label(&self, repo: &RepoId, number: u64, label: &str) -> Result<()> {
        // A label that is not there is not an error: apply runs after a delay
        // and the world may have moved.
        let _ = self
            .client
            .issues(&repo.owner, &repo.name)
            .remove_label(number, label)
            .await;
        Ok(())
    }

    async fn close_issue(&self, repo: &RepoId, number: u64) -> Result<()> {
        self.client
            .issues(&repo.owner, &repo.name)
            .update(number)
            .state(octocrab::models::IssueState::Closed)
            .send()
            .await
            .map_err(api)?;
        Ok(())
    }

    async fn create_issue(
        &self,
        repo: &RepoId,
        title: &str,
        body: &str,
        labels: &[String],
    ) -> Result<u64> {
        let issue = self
            .client
            .issues(&repo.owner, &repo.name)
            .create(title)
            .body(body)
            .labels(labels.to_vec())
            .send()
            .await
            .map_err(api)?;
        Ok(issue.number)
    }

    async fn merge(&self, repo: &RepoId, number: u64, method: &str) -> Result<()> {
        use octocrab::params::pulls::MergeMethod;

        let method = match method {
            "merge" => MergeMethod::Merge,
            "rebase" => MergeMethod::Rebase,
            _ => MergeMethod::Squash,
        };

        self.client
            .pulls(&repo.owner, &repo.name)
            .merge(number)
            .method(method)
            .send()
            .await
            .map_err(api)?;
        Ok(())
    }
}
