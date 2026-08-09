//! The real GitHub adapter, behind the `github` feature.
//!
//! Split into two types on purpose, mirroring the ports: [`GitHubRead`] is what
//! the review path gets, [`GitHubWrite`] is what only `apply` gets. They are
//! separate structs rather than one struct implementing both traits, so a
//! read-only caller cannot upcast its way into write access.

use async_trait::async_trait;
use octocrab::Octocrab;

use crate::error::{Error, Result};
use crate::evidence::diff::truncate_patch;
use crate::forge::types::{
    ChangedFile, CheckConclusion, CheckRun, Commit, FileStatus, Issue, IssueComment, PullRequest,
    RepoId, ReviewComment, ReviewEvent,
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

/// How much of one file's patch is kept inside a commit's patch.
///
/// Applied at the boundary rather than at render time: a commit that adds a
/// vendored tree would otherwise be held in memory in full before anything got
/// the chance to shorten it. The `commits` lane applies its own, tighter budget
/// across the whole range on top of this.
const MAX_FILE_PATCH_BYTES: usize = 16 * 1024;

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
        // Raw route rather than the typed builder: octocrab's commit builder
        // does not resolve to a future here, and the payload we need is three
        // fields deep in a shape that is not going to change.
        let route = format!(
            "/repos/{}/{}/pulls/{number}/commits?per_page=100",
            repo.owner, repo.name
        );
        let raw: serde_json::Value = self.client.get(route, None::<&()>).await.map_err(api)?;

        Ok(raw
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .map(|c| Commit {
                        sha: c["sha"].as_str().unwrap_or_default().to_string(),
                        message: c["commit"]["message"]
                            .as_str()
                            .unwrap_or_default()
                            .to_string(),
                        author_name: c["commit"]["author"]["name"]
                            .as_str()
                            .unwrap_or_default()
                            .to_string(),
                        author_email: c["commit"]["author"]["email"]
                            .as_str()
                            .unwrap_or_default()
                            .to_string(),
                        // Metadata only. The patch is a separate request per
                        // commit, made by `pull_request_context` for the
                        // commits it is willing to pay for.
                        patch: None,
                    })
                    .collect()
            })
            .unwrap_or_default())
    }

    async fn commit_patch(&self, repo: &RepoId, sha: &str) -> Result<Option<String>> {
        // The commit endpoint carries per-file patches. Assembled here into one
        // unified text rather than requested as `application/vnd.github.diff`,
        // because the JSON shape lets each file be capped on the way in: a
        // vendored directory landing in one commit must not pull megabytes into
        // memory before anything gets to truncate it.
        let route = format!("/repos/{}/{}/commits/{sha}", repo.owner, repo.name);
        let raw: serde_json::Value = match self.client.get(&route, None::<&()>).await {
            Ok(raw) => raw,
            // A commit the forge will not render a diff for is not an error.
            Err(octocrab::Error::GitHub { source, .. }) if source.status_code == 404 => {
                return Ok(None);
            }
            Err(err) => return Err(api(err)),
        };

        let Some(files) = raw["files"].as_array() else {
            return Ok(None);
        };

        let mut patch = String::new();
        for file in files {
            let path = file["filename"].as_str().unwrap_or_default();
            let Some(hunks) = file["patch"].as_str() else {
                // No patch means binary, or a file GitHub truncated. Named
                // anyway: "this commit touched a binary" is evidence, and
                // silence would read as "it touched nothing".
                let status = file["status"].as_str().unwrap_or("changed");
                patch.push_str(&format!("--- {path} ({status}, no textual patch)\n"));
                continue;
            };
            patch.push_str(&format!("--- a/{path}\n+++ b/{path}\n"));
            patch.push_str(truncate_patch(hunks, MAX_FILE_PATCH_BYTES).trim_end());
            patch.push('\n');
        }

        Ok((!patch.is_empty()).then_some(patch))
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
        let mut page = self
            .client
            .pulls(&repo.owner, &repo.name)
            .list_comments(Some(number))
            .send()
            .await
            .map_err(api)?;

        let mut comments = Vec::new();
        loop {
            for c in &page.items {
                comments.push(ReviewComment {
                    path: c.path.clone(),
                    line: c.line.unwrap_or(0),
                    start_line: c.start_line,
                    // Carried through because dedupe refuses to trust a marker in
                    // anyone else's comment. No author means no trusted marker: an
                    // unattributed comment is treated exactly like a stranger's.
                    author: c.user.as_ref().map(|u| u.login.clone()).unwrap_or_default(),
                    body: c.body.clone(),
                });
            }
            match self.client.get_page(&page.next).await.map_err(api)? {
                Some(next) => page = next,
                None => break,
            }
        }

        Ok(comments)
    }

    async fn own_review_state(&self, repo: &RepoId, number: u64) -> Result<Option<ReviewEvent>> {
        let route = format!(
            "/repos/{}/{}/pulls/{number}/reviews?per_page=100",
            repo.owner, repo.name
        );
        let raw: serde_json::Value = self.client.get(route, None::<&()>).await.map_err(api)?;

        // Only our own reviews, latest last. GitHub keeps every review in this
        // list, so the state that matters is the final one we left — an earlier
        // block followed by our own approval is not a block.
        Ok(raw.as_array().and_then(|reviews| {
            reviews
                .iter()
                .filter(|r| {
                    // Exact rather than `starts_with`, which would have counted
                    // a review left by an account called `tinysweeper-anything`
                    // as our own. See `findings::prior::is_own_login`.
                    let login = r["user"]["login"].as_str().unwrap_or_default();
                    crate::findings::prior::is_own_login(login)
                })
                .filter_map(|r| match r["state"].as_str() {
                    Some("CHANGES_REQUESTED") => Some(ReviewEvent::RequestChanges),
                    Some("APPROVED") => Some(ReviewEvent::Approve),
                    Some("COMMENTED") => Some(ReviewEvent::Comment),
                    _ => None,
                })
                .next_back()
        }))
    }

    async fn file_at(&self, repo: &RepoId, path: &str, sha: &str) -> Result<Option<String>> {
        let result = self
            .client
            .repos(&repo.owner, &repo.name)
            .get_content()
            .path(path)
            .r#ref(sha)
            .send()
            .await;

        let contents = match result {
            Ok(contents) => contents,
            // A repository without the file is the common answer, not a
            // failure: most repositories have no `AGENTS.md`, and turning that
            // into an error would make the knowledge centre noisy on nearly
            // every review.
            Err(octocrab::Error::GitHub { source, .. }) if source.status_code == 404 => {
                return Ok(None);
            }
            Err(err) => return Err(api(err)),
        };

        // A directory answers with several items and no content of its own;
        // taking the first item's content yields `None` for it, which is the
        // right answer — a directory is not an instruction file.
        Ok(contents
            .items
            .into_iter()
            .next()
            .and_then(|item| item.decoded_content()))
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
        let conclusion = match check.conclusion {
            CheckConclusion::Success => "success",
            CheckConclusion::Failure => "failure",
            CheckConclusion::ActionRequired => "action_required",
            CheckConclusion::Neutral => "neutral",
            CheckConclusion::Skipped => "skipped",
        };

        // A check-run summary is capped at 65535 characters by the API, and a
        // rejected request means no check at all — which reads as "the bot did
        // not run" rather than "the bot said too much".
        let summary: String = check.summary.chars().take(60_000).collect();

        let body = serde_json::json!({
            "name": check.name,
            "head_sha": check.head_sha,
            "status": "completed",
            "conclusion": conclusion,
            "output": { "title": check.title, "summary": summary },
        });

        let route = format!("/repos/{}/{}/check-runs", repo.owner, repo.name);
        let _: serde_json::Value = self.client.post(route, Some(&body)).await.map_err(api)?;
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
        event: ReviewEvent,
    ) -> Result<()> {
        let mut review = serde_json::json!({
            "body": body,
            "event": event.as_api(),
            "comments": comments
                .iter()
                .map(|c| serde_json::json!({
                    "path": c.path,
                    "line": c.line,
                    // Every finding here is anchored to a line the diff
                    // actually touches (see `anchored_in_diff`), always on
                    // the head revision. GitHub defaults `side` to `RIGHT`
                    // when omitted, but naming it keeps that from being an
                    // implicit fact one API change away from silently
                    // failing every inline comment.
                    "side": "RIGHT",
                    "body": c.body,
                }))
                .collect::<Vec<_>>(),
        });
        if comments.is_empty() {
            review["comments"] = serde_json::json!([]);
        }

        let route = format!("/repos/{}/{}/pulls/{number}/reviews", repo.owner, repo.name);
        let _: serde_json::Value = self.client.post(route, Some(&review)).await.map_err(api)?;
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
