//! The real GitHub adapter, behind the `github` feature.
//!
//! Split into two types on purpose, mirroring the ports: [`GitHubRead`] is what
//! the review path gets, [`GitHubWrite`] is what only `apply` gets. They are
//! separate structs rather than one struct implementing both traits, so a
//! read-only caller cannot upcast its way into write access.

use std::collections::HashMap;

use async_trait::async_trait;
use octocrab::Octocrab;

use crate::error::{Error, Result};
use crate::evidence::diff::truncate_patch;
use crate::forge::types::{
    ChangedFile, CheckConclusion, CheckRun, CheckStatus, Commit, FileStatus, Issue, IssueComment,
    PullRequest, RepoId, ReviewComment, ReviewEvent, ReviewVerdict,
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

/// How many items one page of a paged endpoint returns. GitHub's maximum.
const PER_PAGE: usize = 100;

/// How many pages of check runs are read before giving up.
///
/// A bound rather than an unbounded loop: a repository with a pathological
/// number of check runs must not be able to hold a worker forever.
const MAX_CHECK_PAGES: usize = 10;

/// How many pages of reviews are read before giving up.
///
/// Its own constant rather than an alias of [`MAX_CHECK_PAGES`]. The two bound
/// unrelated endpoints and the reasoning behind each is different — a thousand
/// check runs on one commit is pathological, a thousand reviews on a long-lived
/// pull request is merely unusual — so a shared constant would silently move
/// one bound whenever somebody tuned the other.
const MAX_REVIEW_PAGES: usize = 20;

/// Read every page of a paged endpoint, or fail loudly.
///
/// The subtle bug this exists to prevent: exiting the loop on
/// `fetched < PER_PAGE` only fires when the *last* page is short. A final page
/// that happens to be exactly full falls out of the range instead, and whatever
/// was on the next page is dropped **without a diagnostic**.
///
/// That matters because both callers fail closed on missing data — a check run
/// nobody reported is not a pass, an unretired changes-request nobody saw is
/// not an approval. Silently short data does not fail closed; it fails *open*,
/// because absence is what both readers treat as innocent. So exhausting the
/// bound with a full page is an error rather than a truncation: the caller
/// refuses instead of merging on a history it only partly read.
async fn read_all_pages<T, F, Fut>(
    fetch: F,
    items_of: impl Fn(&serde_json::Value) -> Option<&Vec<serde_json::Value>>,
    map_page: impl Fn(&[serde_json::Value]) -> Vec<T>,
    max_pages: usize,
    what: &str,
) -> Result<Vec<T>>
where
    F: Fn(usize) -> Fut,
    Fut: std::future::Future<Output = Result<serde_json::Value>>,
{
    let mut out = Vec::new();

    for page in 1..=max_pages {
        let raw = fetch(page).await?;
        let Some(items) = items_of(&raw) else {
            break;
        };
        let fetched = items.len();
        out.extend(map_page(items));

        if fetched < PER_PAGE {
            return Ok(out);
        }
    }

    // Every page was full right up to the bound, so there may well be another.
    Err(Error::Forge(format!(
        "{what} did not fit in {max_pages} pages of {PER_PAGE}; refusing to \
         report a partial list, because a missing entry reads as innocent"
    )))
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

    /// Blob sizes at one revision, keyed by path.
    ///
    /// One recursive tree request for the whole revision rather than a blob
    /// request per file. The per-file shape looks cheaper on a small pull
    /// request and is not: it costs a round trip each against a rate limit
    /// shared with everything else the installation does, and the file that
    /// most needs a size — a large one — is the one whose blob is most
    /// expensive to serve.
    ///
    /// Best-effort by design. GitHub truncates the tree for very large
    /// repositories, and a size we could not learn must read as unknown rather
    /// than as zero: `scan::blobs` treats `None` as "unknown", where a zero
    /// would silently mean "safely small".
    async fn blob_sizes(&self, repo: &RepoId, sha: &str) -> HashMap<String, u64> {
        let route = format!(
            "/repos/{}/{}/git/trees/{}?recursive=1",
            repo.owner, repo.name, sha
        );

        // Raw route and `serde_json::Value`, matching `commits` above:
        // octocrab has no typed model for the git-tree response, and the three
        // fields wanted here are stable.
        let raw: serde_json::Value = match self.client.get(&route, None::<&()>).await {
            Ok(raw) => raw,
            Err(err) => {
                // Not fatal to the review. Sizes are an enrichment, and failing
                // the whole review because one optional field could not be
                // filled trades a small blind spot for a total one.
                tracing::warn!(%err, "could not read blob sizes; large-blob detection is off");
                return HashMap::new();
            }
        };

        if raw["truncated"].as_bool().unwrap_or(false) {
            tracing::warn!(
                repo = %format!("{}/{}", repo.owner, repo.name),
                "the git tree was truncated; some files will have no known size"
            );
        }

        Self::sizes_from_tree(&raw)
    }

    /// The size map, split out from the request so it can be tested offline.
    fn sizes_from_tree(raw: &serde_json::Value) -> HashMap<String, u64> {
        raw["tree"]
            .as_array()
            .map(|entries| {
                entries
                    .iter()
                    // `size` is absent on tree and commit entries and present
                    // only on blobs, so this filter also excludes directories
                    // without needing to read `type`.
                    .filter_map(|entry| {
                        Some((entry["path"].as_str()?.to_string(), entry["size"].as_u64()?))
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// How many distinct reviewers currently approve.
    ///
    /// Folded from [`ForgeRead::reviews`] rather than parsing the endpoint a
    /// second time. Two parsers over one payload drift, and this pair already
    /// proved it: one paged the endpoint and one did not, so they disagreed
    /// about a pull request with a long history.
    ///
    /// GitHub returns every review ever left, so a plain count of `APPROVED`
    /// double-counts a reviewer who approved twice and, worse, keeps counting
    /// one who later requested changes. Only each reviewer's *latest* verdict
    /// says anything about the present state, and `reviews` already drops the
    /// states that carry no verdict at all.
    ///
    /// Best-effort: an unreadable list yields zero, the conservative direction
    /// for every caller — nothing gates *on* having fewer approvals than exist.
    async fn approvals(&self, repo: &RepoId, number: u64) -> u32 {
        match self.reviews(repo, number).await {
            Ok(reviews) => approvals_of(&reviews),
            Err(err) => {
                tracing::warn!(%err, "could not read reviews; reporting zero approvals");
                0
            }
        }
    }
}

/// Count the reviewers whose *latest* verdict is an approval.
///
/// Pure, so the rule it encodes is covered offline: a reviewer who approved and
/// then requested changes must not still be counted, and approving twice counts
/// once.
fn approvals_of(reviews: &[ReviewVerdict]) -> u32 {
    let mut latest: HashMap<&str, &ReviewVerdict> = HashMap::new();
    for review in reviews {
        // `Comment` is not a verdict. GitHub lets a reviewer comment without
        // touching their approval, so letting one overwrite the previous entry
        // would silently retire an approval that still stands — and in the
        // merge-gate direction that matters, it reads as *fewer* approvals than
        // exist, which stalls rather than merges. Still a bug, still wrong.
        if review.state == ReviewEvent::Comment {
            continue;
        }
        latest.insert(review.reviewer.as_str(), review);
    }

    latest
        .values()
        .filter(|verdict| verdict.state == ReviewEvent::Approve)
        .count() as u32
}

/// Map one page of the reviews endpoint.
///
/// Split out from the request so the shape decisions — which states carry a
/// verdict, and what makes a reviewer a bot — are covered by the offline suite
/// rather than only behind the `github` feature.
fn verdicts_from_page(items: &[serde_json::Value]) -> Vec<ReviewVerdict> {
    items
        .iter()
        .filter_map(|review| {
            // `DISMISSED` and `PENDING` carry no verdict: one was retired by a
            // human, the other was never submitted. The mapping is tested
            // offline on `ReviewEvent`.
            let state = ReviewEvent::from_api(review["state"].as_str()?)?;
            Some(ReviewVerdict {
                reviewer: review["user"]["login"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string(),
                // The account type, as GitHub reports it, rather than a guess
                // from the login. An approval only counts towards
                // `require_approvals` when a human left it.
                bot: review["user"]["type"].as_str() == Some("Bot"),
                state,
            })
        })
        .collect()
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
            approvals: self.approvals(repo, number).await,
        })
    }

    async fn changed_files(&self, repo: &RepoId, number: u64) -> Result<Vec<ChangedFile>> {
        let mut page = self
            .client
            .pulls(&repo.owner, &repo.name)
            .list_files(number)
            .await
            .map_err(api)?;

        // Sizes come from the head revision's tree, because the files endpoint
        // does not report them. A removed file is absent from that tree and so
        // keeps `None`, which is correct: it has no size at the head, and the
        // blob scanner is asking about what the change *introduces*.
        let sizes = match self.pull_request(repo, number).await {
            Ok(pr) => self.blob_sizes(repo, &pr.head_sha).await,
            Err(err) => {
                tracing::warn!(%err, "could not resolve the head sha; sizes unavailable");
                HashMap::new()
            }
        };

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
                    // Still `None` when the tree was truncated or unreadable.
                    // `scan::blobs` reads that as "unknown" rather than
                    // "small", which is the honest answer and the safe one.
                    size_bytes: sizes.get(&file.filename).copied(),
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
                    line: c.line,
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

    async fn check_runs(&self, repo: &RepoId, sha: &str) -> Result<Vec<CheckStatus>> {
        // Read to exhaustion rather than truncated at the first page. A missing
        // check reads to the auto-merge gate as "not reported", so short data
        // would turn a red required check into a merge.
        read_all_pages(
            |page| async move {
                let route = format!(
                    "/repos/{}/{}/commits/{sha}/check-runs?per_page={PER_PAGE}&page={page}",
                    repo.owner, repo.name
                );
                self.client.get(route, None::<&()>).await.map_err(api)
            },
            |raw| raw["check_runs"].as_array(),
            |items| {
                items
                    .iter()
                    // The wire-string mapping lives on `CheckStatus` rather
                    // than here, so the offline suite can test the one decision
                    // that matters: an unrecognised conclusion is never a pass.
                    .map(|item| {
                        CheckStatus::from_api(
                            item["name"].as_str().unwrap_or_default(),
                            item["conclusion"].as_str(),
                        )
                    })
                    .collect()
            },
            MAX_CHECK_PAGES,
            "the check runs for this commit",
        )
        .await
    }

    async fn reviews(&self, repo: &RepoId, number: u64) -> Result<Vec<ReviewVerdict>> {
        // Same reasoning as `check_runs`, and the stakes are higher: the port
        // promises the *whole* history so the caller can fold it to a
        // latest-verdict-per-reviewer. The oldest entries are where an unretired
        // `CHANGES_REQUESTED` lives, and dropping them would leave the gate
        // reading a history in which nobody ever blocked the merge.
        read_all_pages(
            |page| async move {
                let route = format!(
                    "/repos/{}/{}/pulls/{number}/reviews?per_page={PER_PAGE}&page={page}",
                    repo.owner, repo.name
                );
                self.client.get(route, None::<&()>).await.map_err(api)
            },
            |raw| raw.as_array(),
            verdicts_from_page,
            MAX_REVIEW_PAGES,
            "the review history for this pull request",
        )
        .await
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_tree_reports_blob_sizes_and_skips_directories() {
        // Directories carry no `size`, so they must not enter the map at all —
        // a directory with a zero size would read as an empty file.
        let tree = json!({
            "truncated": false,
            "tree": [
                {"path": "src", "type": "tree"},
                {"path": "src/main.rs", "type": "blob", "size": 1234},
                {"path": "assets/logo.png", "type": "blob", "size": 4_000_000},
            ]
        });

        let sizes = GitHubRead::sizes_from_tree(&tree);

        assert_eq!(sizes.get("src/main.rs"), Some(&1234));
        assert_eq!(sizes.get("assets/logo.png"), Some(&4_000_000));
        assert_eq!(sizes.get("src"), None);
    }

    #[test]
    fn a_truncated_tree_yields_no_size_rather_than_a_zero() {
        // The dangerous failure is a size of zero, which reads as "safely
        // small" to the blob scanner. Absence reads as unknown, which is true.
        let sizes = GitHubRead::sizes_from_tree(&json!({"truncated": true, "tree": []}));
        assert!(sizes.is_empty());
        assert_eq!(sizes.get("src/main.rs"), None);
    }

    /// A verdict, built without going near the wire format.
    fn verdict(reviewer: &str, state: ReviewEvent) -> ReviewVerdict {
        ReviewVerdict {
            reviewer: reviewer.into(),
            bot: false,
            state,
        }
    }

    #[test]
    fn only_a_reviewers_latest_verdict_counts_towards_approvals() {
        // The bug this exists to stop: someone approves, then finds a problem
        // and requests changes, and a naive count still reports an approval.
        let reviews = [
            verdict("ana", ReviewEvent::Approve),
            verdict("ana", ReviewEvent::RequestChanges),
            verdict("bo", ReviewEvent::Approve),
        ];

        assert_eq!(approvals_of(&reviews), 1);
    }

    #[test]
    fn approving_twice_counts_once() {
        let reviews = [
            verdict("ana", ReviewEvent::Approve),
            verdict("ana", ReviewEvent::Approve),
        ];

        assert_eq!(approvals_of(&reviews), 1);
    }

    #[test]
    fn a_later_comment_does_not_withdraw_an_approval() {
        // GitHub lets a reviewer comment without changing their verdict.
        // `verdicts_from_page` deliberately keeps `COMMENTED` — the history is
        // the port's contract and other callers want it — so the fold is where
        // the rule has to live. This test caught it being dropped.
        let page = json!([
            {"user": {"login": "ana", "type": "User"}, "state": "APPROVED"},
            {"user": {"login": "ana", "type": "User"}, "state": "COMMENTED"},
        ]);

        let reviews = verdicts_from_page(page.as_array().expect("an array"));

        assert_eq!(approvals_of(&reviews), 1);
    }

    #[test]
    fn no_reviews_is_no_approvals() {
        // Conservative direction: nothing gates on having *fewer* approvals.
        assert_eq!(approvals_of(&[]), 0);
    }

    #[test]
    fn a_page_of_reviews_maps_only_the_states_that_carry_a_verdict() {
        let page = serde_json::json!([
            {"user": {"login": "ana", "type": "User"}, "state": "APPROVED"},
            {"user": {"login": "bo", "type": "User"}, "state": "CHANGES_REQUESTED"},
            // Retired by a human; it no longer blocks anything.
            {"user": {"login": "cy", "type": "User"}, "state": "DISMISSED"},
            // Never submitted.
            {"user": {"login": "di", "type": "User"}, "state": "PENDING"},
            {"user": {"login": "tinysweeper", "type": "Bot"}, "state": "APPROVED"},
        ]);

        let verdicts = verdicts_from_page(page.as_array().expect("an array"));

        let seen: Vec<&str> = verdicts.iter().map(|v| v.reviewer.as_str()).collect();
        assert_eq!(seen, ["ana", "bo", "tinysweeper"], "{verdicts:?}");
        assert!(
            verdicts
                .iter()
                .find(|v| v.reviewer == "tinysweeper")
                .expect("the bot")
                .bot,
            "the account type comes from GitHub, not from the login: {verdicts:?}"
        );
        assert!(
            !verdicts
                .iter()
                .find(|v| v.reviewer == "ana")
                .expect("ana")
                .bot,
            "{verdicts:?}"
        );
    }

    #[test]
    fn a_review_without_a_recognised_state_is_dropped_rather_than_guessed() {
        let page = serde_json::json!([
            {"user": {"login": "ana", "type": "User"}, "state": "SOMETHING_NEW"},
        ]);

        assert!(verdicts_from_page(page.as_array().expect("an array")).is_empty());
    }

    /// A client that answers every request with the same canned page.
    ///
    /// Enough to drive `read_all_pages` past its bound, which is the case the
    /// offline suite could not otherwise reach — and the one that used to drop
    /// data silently.
    fn full_page(items: usize) -> serde_json::Value {
        serde_json::Value::Array(
            (0..items)
                .map(|i| serde_json::json!({"user": {"login": format!("r{i}"), "type": "User"}, "state": "APPROVED"}))
                .collect(),
        )
    }

    #[tokio::test]
    async fn a_short_final_page_ends_the_read() {
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen = calls.clone();

        let out = read_all_pages(
            move |_page| {
                let n = seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                // A full page, then a short one: the normal shape.
                std::future::ready(Ok(if n == 0 {
                    full_page(PER_PAGE)
                } else {
                    full_page(3)
                }))
            },
            |raw| raw.as_array(),
            verdicts_from_page,
            MAX_REVIEW_PAGES,
            "reviews",
        )
        .await
        .expect("a short page is a clean end");

        assert_eq!(out.len(), PER_PAGE + 3);
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn exhausting_the_bound_on_a_full_page_is_an_error_not_a_truncation() {
        // The silent-drop bug: every page full right up to the cap means there
        // is probably another one. Returning what we have would hand the
        // auto-merge gate a history it believes is complete — and absence is
        // exactly what that gate reads as innocent.
        let err = read_all_pages(
            |_page| std::future::ready(Ok(full_page(PER_PAGE))),
            |raw| raw.as_array(),
            verdicts_from_page,
            3,
            "reviews",
        )
        .await
        .expect_err("a full last page must not read as the end");

        assert!(
            format!("{err}").contains("refusing to report a partial list"),
            "{err}"
        );
    }
}
