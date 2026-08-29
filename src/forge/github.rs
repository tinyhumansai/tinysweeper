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
    PullRequest, RepoId, ReviewComment, ReviewEvent, ReviewThread, ReviewVerdict, ThreadComment,
};
use crate::ports::forge::{ForgeRead, ForgeWrite};

fn client(token: &str) -> Result<Octocrab> {
    Octocrab::builder()
        .personal_token(token.to_string())
        .build()
        .map_err(|err| Error::Forge(format!("could not build a GitHub client: {err}")))
}

fn api(err: octocrab::Error) -> Error {
    Error::Forge(chain(&err))
}

/// Render an error together with everything it wraps.
///
/// `err.to_string()` alone is not enough here, and the reason is specific
/// rather than stylistic. octocrab's `Error::GitHub` variant carries no
/// `#[snafu(display(...))]`, so Snafu falls back to printing the variant
/// name — the literal string `GitHub` — while the status code, GitHub's own
/// message and the `errors` array all sit in the `source` and are discarded.
///
/// That is the commonest failure this adapter has: a 403 from a missing
/// permission, a 404 from an uninstalled app, a 422 from a malformed write.
/// Every one of them logged as `forge: GitHub` and nothing else, which is
/// indistinguishable from every other one of them. A review that failed for
/// want of a scope looked exactly like a review that failed on a typo'd path.
///
/// Nothing here can leak a credential: the chain is GitHub's own response
/// body, which never contains the token that was sent.
fn chain(err: &dyn std::error::Error) -> String {
    let mut rendered = err.to_string();
    let mut source = err.source();
    while let Some(inner) = source {
        let next = inner.to_string();
        // Snafu repeats the variant's display in the source for some variants.
        // Appending `GitHub: GitHub` helps nobody.
        if !rendered.ends_with(&next) {
            rendered.push_str(": ");
            rendered.push_str(&next);
        }
        source = inner.source();
    }
    rendered
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

/// How many pages of review threads are read before giving up.
const MAX_THREAD_PAGES: usize = 20;

/// How many threads, and comments per thread, one GraphQL page asks for.
///
/// Below GitHub's 100 because the node budget is multiplicative: threads times
/// comments. A page that asks for too much is rejected outright, which would
/// make thread resolution fail on exactly the busy pull requests it is for.
const GRAPHQL_PAGE: usize = 50;

/// The review threads on a pull request, with the state REST does not expose.
const REVIEW_THREADS_QUERY: &str = r#"
query($owner: String!, $name: String!, $number: Int!, $after: String) {
  repository(owner: $owner, name: $name) {
    pullRequest(number: $number) {
      reviewThreads(first: $first, after: $after) {
        pageInfo { hasNextPage endCursor }
        nodes {
          id
          isResolved
          isOutdated
          comments(first: $first) {
            nodes { body author { login __typename } }
          }
        }
      }
    }
  }
}
"#;

/// The mutation that resolves one thread.
const RESOLVE_THREAD_MUTATION: &str = r#"
mutation($id: ID!) {
  resolveReviewThread(input: {threadId: $id}) {
    thread { id isResolved }
  }
}
"#;

/// Reply inside an existing review conversation.
///
/// `addPullRequestReviewThreadReply` rather than the REST
/// `pulls/comments/{id}/replies` route, because the thread is already
/// identified by the GraphQL node id the resolve mutation takes — going
/// through REST would mean carrying a second identifier for the same thread
/// purely to say one sentence in it.
const REPLY_THREAD_MUTATION: &str = r#"
mutation($id: ID!, $body: String!) {
  addPullRequestReviewThreadReply(input: {pullRequestReviewThreadId: $id, body: $body}) {
    comment { id }
  }
}
"#;

/// The request body shared by creating and updating a check run.
///
/// The status/conclusion pair is the part worth keeping in one place: GitHub
/// rejects a `conclusion` on a run that is not `completed`, and silently leaves
/// a run pending forever if `status` says `in_progress` when a verdict exists.
/// Deriving both from the same `Option` makes the mismatch unrepresentable.
fn check_payload(check: &CheckRun) -> serde_json::Value {
    // A check-run summary is capped at 65535 characters by the API, and a
    // rejected request means no check at all — which reads as "the bot did not
    // run" rather than "the bot said too much".
    let summary: String = check.summary.chars().take(60_000).collect();

    let mut body = serde_json::json!({
        "name": check.name,
        "output": { "title": check.title, "summary": summary },
    });

    match check.conclusion {
        Some(conclusion) => {
            body["status"] = serde_json::json!("completed");
            body["conclusion"] = serde_json::json!(match conclusion {
                CheckConclusion::Success => "success",
                CheckConclusion::Failure => "failure",
                CheckConclusion::ActionRequired => "action_required",
                CheckConclusion::Neutral => "neutral",
                CheckConclusion::Skipped => "skipped",
            });
        }
        None => body["status"] = serde_json::json!("in_progress"),
    }

    body
}

/// The `reviewThreads` connection, wherever the client left it.
///
/// Accepts both the raw GraphQL envelope (`data.repository…`) and an already
/// unwrapped `data`, because which of the two a client hands back is a detail
/// of the client and not something worth a runtime surprise.
fn threads_connection(raw: &serde_json::Value) -> &serde_json::Value {
    let unwrapped = &raw["repository"]["pullRequest"]["reviewThreads"];
    if unwrapped.is_object() {
        return unwrapped;
    }
    &raw["data"]["repository"]["pullRequest"]["reviewThreads"]
}

/// Map one page of review threads.
///
/// A comment whose author is gone — a deleted account — keeps its place with an
/// empty login rather than being dropped: the login is only ever compared for
/// equality against our own, and an empty one matches nothing, while a dropped
/// comment would change which comment looks like the thread's opener.
fn threads_from_graphql(raw: &serde_json::Value) -> Vec<ReviewThread> {
    let Some(nodes) = threads_connection(raw)["nodes"].as_array() else {
        return Vec::new();
    };
    nodes
        .iter()
        .map(|node| ReviewThread {
            id: node["id"].as_str().unwrap_or_default().to_string(),
            is_resolved: node["isResolved"].as_bool().unwrap_or(false),
            is_outdated: node["isOutdated"].as_bool().unwrap_or(false),
            comments: node["comments"]["nodes"]
                .as_array()
                .map(|comments| {
                    comments
                        .iter()
                        .map(|comment| ThreadComment {
                            author: comment["author"]["login"]
                                .as_str()
                                .unwrap_or_default()
                                .to_string(),
                            body: comment["body"].as_str().unwrap_or_default().to_string(),
                            bot: comment["author"]["__typename"].as_str() == Some("Bot"),
                        })
                        .collect()
                })
                .unwrap_or_default(),
        })
        .collect()
}

/// Fail on a GraphQL response that carried errors.
///
/// GraphQL answers HTTP 200 with an `errors` array, so a caller that only
/// checks the status reads a refused query as an empty result — which for
/// review threads means "nothing to resolve" and looks exactly like success.
fn graphql_errors(raw: &serde_json::Value, what: &str) -> Result<()> {
    match raw["errors"].as_array() {
        Some(errors) if !errors.is_empty() => Err(Error::Forge(format!(
            "GitHub refused {what}: {}",
            serde_json::to_string(errors).unwrap_or_default()
        ))),
        _ => Ok(()),
    }
}

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

/// One issue, as the REST issues endpoint renders it.
///
/// Parsed by hand rather than through octocrab's model because the model has no
/// field for `type`, and the native issue type is the whole point of this path.
/// Every field degrades to its empty value: a triage that loses the body is
/// worse than useless, but an error here would lose the triage entirely.
fn issue_from_json(raw: &serde_json::Value) -> Issue {
    Issue {
        number: raw["number"].as_u64().unwrap_or_default(),
        title: raw["title"].as_str().unwrap_or_default().to_string(),
        body: raw["body"].as_str().unwrap_or_default().to_string(),
        author: raw["user"]["login"]
            .as_str()
            .unwrap_or_default()
            .to_string(),
        labels: raw["labels"]
            .as_array()
            .map(|labels| {
                labels
                    .iter()
                    .filter_map(|label| label["name"].as_str())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default(),
        open: raw["state"].as_str() == Some("open"),
        age_days: 0,
        quiet_days: 0,
        comments: raw["comments"].as_u64().unwrap_or_default() as u32,
        // Absent, null, or an object without a name all mean "nobody has
        // chosen a type", which is the only state triage may write into.
        issue_type: raw["type"]["name"].as_str().map(str::to_string),
    }
}

/// Whole days between `then` (a Unix timestamp) and now.
///
/// `None` — a timestamp GitHub did not send — is **zero days**, not an error
/// and not a large number. Every guard that reads an age refuses when the item
/// is too *young*, so an unknown age has to read as "brand new" or a missing
/// field would unlock the close it was meant to gate.
fn days_since(then: Option<i64>) -> u32 {
    let Some(then) = then else { return 0 };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_secs() as i64)
        .unwrap_or(then);
    ((now - then).max(0) / 86_400) as u32
}

/// Map octocrab's pull request onto the crate's, with `approvals` supplied.
///
/// The approval count is a parameter rather than a fetch because it costs a
/// request of its own: the single-pull-request path pays for it, and the list
/// path — which reads a hundred at a time and does not use the figure — does
/// not.
fn pull_request_from(
    pr: octocrab::models::pulls::PullRequest,
    repo: &RepoId,
    approvals: u32,
) -> PullRequest {
    let number = pr.number;
    let base = pr.base;
    let head = pr.head;

    PullRequest {
        number,
        title: pr.title.unwrap_or_default(),
        body: pr.body.unwrap_or_default(),
        author: pr
            .user
            .as_ref()
            .map(|u| u.login.clone())
            .unwrap_or_default(),
        // GitHub's own answer, not a guess from the login. Auto-merge's
        // dependency-bump exemption is only as safe as this field.
        author_is_bot: pr
            .user
            .as_ref()
            .map(|u| u.r#type.eq_ignore_ascii_case("bot"))
            .unwrap_or(false),
        draft: pr.draft.unwrap_or(false),
        base_ref: base.ref_field,
        base_sha: base.sha,
        head_ref: head.ref_field.clone(),
        head_sha: head.sha,
        // `head.repo` differing from `base.repo` is what makes a pull
        // request a fork one, and that changes which token can post.
        //
        // Both halves of the name, not only the owner: an organisation's own
        // fork — `org/fork` targeting `org/upstream` — shares the owner and is
        // still a fork, and comparing only the login calls it a branch.
        from_fork: head
            .repo
            .as_ref()
            .map(|head_repo| {
                let owner = head_repo
                    .owner
                    .as_ref()
                    .map(|o| o.login.as_str())
                    .unwrap_or_default();
                owner != repo.owner || head_repo.name != repo.name
            })
            .unwrap_or(false),
        labels: pr
            .labels
            .unwrap_or_default()
            .into_iter()
            .map(|l| l.name)
            .collect(),
        mergeable: pr.mergeable,
        // `merged_at` rather than `merged`: octocrab only populates the
        // latter on some endpoints, and a missing bool would read as "not
        // merged" on exactly the path that decides whether an issue closes.
        merged: pr.merged_at.is_some(),
        // Absent reads as open: the endpoints that omit `state` are the ones
        // that only ever return open pull requests, and reading an omission as
        // "closed" would make the triage sweep skip every one of them.
        open: pr
            .state
            .map(|state| matches!(state, octocrab::models::IssueState::Open))
            .unwrap_or(true),
        approvals,
        age_days: days_since(pr.created_at.map(|at| at.timestamp())),
        // `updated_at` counts *any* write, tinysweeper's own labels and
        // comments included, so this is a floor on how quiet the pull request
        // really is and never an overstatement. That asymmetry is the safe one:
        // a quiet guard that reads too low refuses a close it might have
        // allowed, where one that read too high would allow a close on a pull
        // request somebody commented on this morning.
        quiet_days: days_since(pr.updated_at.map(|at| at.timestamp())),
    }
}

/// The type names in an `/orgs/{org}/issue-types` answer.
///
/// Anything that is not a list — a 404 body from a user account, an error
/// object — yields no names, and no names means triage sets no type.
fn type_names_from_json(raw: &serde_json::Value) -> Vec<String> {
    raw.as_array()
        .map(|types| {
            types
                .iter()
                .filter_map(|entry| entry["name"].as_str())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
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

        let approvals = self.approvals(repo, number).await;
        Ok(pull_request_from(pr, repo, approvals))
    }

    async fn branch_head(&self, repo: &RepoId, branch: &str) -> Result<Option<String>> {
        // The branch name goes in the path, so a name containing a slash — a
        // `release/1.2` — is passed through as-is. GitHub resolves it; what it
        // must not do is escape the route, which `RepoId::parse` and GitHub's
        // own ref rules already prevent.
        let route = format!("/repos/{}/{}/commits/{branch}", repo.owner, repo.name);
        match self
            .client
            .get::<serde_json::Value, _, ()>(&route, None)
            .await
        {
            Ok(commit) => Ok(commit["sha"].as_str().map(str::to_string)),
            // A pull request can outlive the branch it targets, and that is an
            // answer rather than a failure.
            Err(octocrab::Error::GitHub { source, .. }) if source.status_code == 404 => Ok(None),
            Err(err) => Err(api(err)),
        }
    }

    async fn open_pull_requests(&self, repo: &RepoId, limit: usize) -> Result<Vec<PullRequest>> {
        let mut out = Vec::new();
        let mut page = 1u32;

        // Paged by hand rather than through `into_stream`, so the request count
        // is bounded by `limit` and visible here. A repository with a thousand
        // open pull requests must not be able to turn one sweep into a thousand
        // API calls.
        while out.len() < limit {
            let batch = self
                .client
                .pulls(&repo.owner, &repo.name)
                .list()
                .state(octocrab::params::State::Open)
                // Ascending by creation, because the port promises oldest
                // first: dedupe calls the older of two near-identical pull
                // requests the original, and truncating the originals away
                // would leave a shortlist that can find no duplicates at all.
                .sort(octocrab::params::pulls::Sort::Created)
                .direction(octocrab::params::Direction::Ascending)
                .per_page(100)
                .page(page)
                .send()
                .await
                .map_err(api)?;

            if batch.items.is_empty() {
                break;
            }
            for pr in batch.items {
                // Zero approvals rather than a per-pull-request call. Counting
                // them properly costs one request each, and a hundred-pull
                // sweep would spend its whole rate-limit budget on a figure
                // triage does not read — only `[automerge]` does, and that path
                // fetches the pull request singly.
                out.push(pull_request_from(pr, repo, 0));
                if out.len() >= limit {
                    break;
                }
            }
            page += 1;
        }

        Ok(out)
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
        // Read to exhaustion, like `review_comments` beside it. Truncating at
        // the first page is not a smaller answer here, it is a wrong one: the
        // callers that look for tinysweeper's own durable comment conclude it
        // does not exist and post a second — so on a pull request with a busy
        // conversation, every sweep would append another one.
        let mut page = self
            .client
            .issues(&repo.owner, &repo.name)
            .list_comments(number)
            .per_page(100)
            .send()
            .await
            .map_err(api)?;

        let mut comments = Vec::new();
        loop {
            for c in &page.items {
                comments.push(IssueComment {
                    id: Some(*c.id),
                    author: c.user.login.clone(),
                    body: c.body.clone().unwrap_or_default(),
                });
            }
            match self.client.get_page(&page.next).await.map_err(api)? {
                Some(next) => page = next,
                None => break,
            }
        }

        Ok(comments)
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

    async fn review_threads(&self, repo: &RepoId, number: u64) -> Result<Vec<ReviewThread>> {
        let mut threads = Vec::new();
        let mut after: Option<String> = None;

        for _ in 0..MAX_THREAD_PAGES {
            let raw: serde_json::Value = self
                .client
                .graphql(&serde_json::json!({
                    "query": REVIEW_THREADS_QUERY.replace("$first", &GRAPHQL_PAGE.to_string()),
                    "variables": {
                        "owner": repo.owner,
                        "name": repo.name,
                        "number": number,
                        "after": after,
                    },
                }))
                .await
                .map_err(api)?;
            graphql_errors(&raw, "the review threads query")?;
            threads.extend(threads_from_graphql(&raw));

            let page = &threads_connection(&raw)["pageInfo"];
            if !page["hasNextPage"].as_bool().unwrap_or(false) {
                return Ok(threads);
            }
            // A missing cursor with more pages claimed would loop forever on
            // the same page; stopping is the honest answer.
            match page["endCursor"].as_str() {
                Some(cursor) => after = Some(cursor.to_string()),
                None => return Ok(threads),
            }
        }

        Err(Error::Forge(
            "a pull request with more review threads than the page bound allows".into(),
        ))
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
        // The raw route rather than octocrab's typed one: its `Issue` model has
        // no `type`, and a second request just to read one field would double
        // the call budget of every triage.
        let route = format!("/repos/{}/{}/issues/{number}", repo.owner, repo.name);
        let raw: serde_json::Value = self.client.get(&route, None::<&()>).await.map_err(api)?;

        Ok(Issue {
            // Trusted over the payload: the caller asked about this number.
            number,
            ..issue_from_json(&raw)
        })
    }

    async fn issue_types(&self, repo: &RepoId) -> Result<Vec<String>> {
        // A repository owned by a user account has no issue types and answers
        // 404, which is an ordinary answer here rather than a failure: it means
        // this repository sets no types, not that the triage should stop.
        let route = format!("/orgs/{}/issue-types", repo.owner);
        Ok(match self.client.get(&route, None::<&()>).await {
            Ok(raw) => type_names_from_json(&raw),
            Err(_) => Vec::new(),
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
                // Not read for the shortlist: the candidates are only ever
                // compared for similarity, and asking for two hundred issues'
                // types would cost a request each.
                issue_type: None,
            })
            .collect())
    }

    async fn search_issues(&self, repo: &RepoId, query: &str) -> Result<Vec<Issue>> {
        // `repo:` is prepended here rather than trusted from the caller, so a
        // query can only ever narrow the search inside one repository and
        // never widen it into somebody else's.
        let scoped = format!("repo:{}/{} {}", repo.owner, repo.name, query.trim());

        let page = self
            .client
            .search()
            .issues_and_pull_requests(&scoped)
            .per_page(SEARCH_PAGE_SIZE)
            .send()
            .await
            .map_err(api)?;

        Ok(page
            .items
            .into_iter()
            .filter(|i| i.pull_request.is_none())
            .map(|i| Issue {
                number: i.number,
                title: i.title,
                body: i.body.unwrap_or_default(),
                author: i.user.login,
                labels: i.labels.into_iter().map(|l| l.name).collect(),
                // The search index reports state, and the dedupe path needs it
                // — a closed tracked issue is still tracked.
                open: matches!(i.state, octocrab::models::IssueState::Open),
                age_days: 0,
                quiet_days: 0,
                comments: i.comments,
                issue_type: None,
            })
            .collect())
    }
}

/// How many search hits one dedupe lookup asks for.
///
/// The marker is unique per Sentry issue, so one hit is the expected answer
/// and anything past a handful means the query matched prose rather than the
/// marker. Small, because GitHub's search rate limit is separate from the REST
/// one and considerably lower.
const SEARCH_PAGE_SIZE: u8 = 20;

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
    async fn publish_check(&self, repo: &RepoId, check: CheckRun) -> Result<u64> {
        let mut body = check_payload(&check);
        // Only on create: `head_sha` is what binds the run to a commit, and the
        // update route rejects it.
        body["head_sha"] = serde_json::json!(check.head_sha);

        let route = format!("/repos/{}/{}/check-runs", repo.owner, repo.name);
        let created: serde_json::Value = self.client.post(route, Some(&body)).await.map_err(api)?;

        // The id is how an in-progress check is concluded later. A create that
        // succeeded but answered with something unreadable is an error rather
        // than a zero: a caller that went on to update check `0` would be
        // writing to whatever run that is, on somebody else's repository.
        created["id"]
            .as_u64()
            .ok_or_else(|| Error::Forge("the check-run create returned no usable id".to_string()))
    }

    async fn update_check(&self, repo: &RepoId, check_id: u64, check: CheckRun) -> Result<()> {
        let route = format!("/repos/{}/{}/check-runs/{check_id}", repo.owner, repo.name);
        let _: serde_json::Value = self
            .client
            .patch(route, Some(&check_payload(&check)))
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
        event: ReviewEvent,
    ) -> Result<()> {
        let mut review = serde_json::json!({
            "body": body,
            "event": event.as_api(),
            "comments": comments
                .iter()
                .map(review_comment_payload)
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

    async fn set_issue_type(&self, repo: &RepoId, number: u64, type_name: &str) -> Result<()> {
        // A PATCH on the issue with the type *name*, which is the documented
        // route and the one confirmed live; the id would force every caller to
        // carry a per-organisation mapping.
        let route = format!("/repos/{}/{}/issues/{number}", repo.owner, repo.name);
        let body = serde_json::json!({"type": type_name});
        let _: serde_json::Value = self
            .client
            .patch(route, Some(&body))
            .await
            .map_err(api)
            .map_err(|error: Error| {
                Error::Forge(format!(
                    "setting the issue type to {type_name} failed: {error}"
                ))
            })?;
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

    async fn close_pull_request(&self, repo: &RepoId, number: u64) -> Result<()> {
        // The pulls endpoint rather than the issues one, even though GitHub
        // would accept either. `PATCH /pulls/{n}` cannot express a merge — the
        // merge button is `PUT /pulls/{n}/merge` — so the request this sends is
        // structurally incapable of landing the branch it is closing.
        self.client
            .pulls(&repo.owner, &repo.name)
            .update(number)
            .state(octocrab::params::pulls::State::Closed)
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

    async fn reply_to_review_thread(
        &self,
        _repo: &RepoId,
        thread_id: &str,
        body: &str,
    ) -> Result<()> {
        let raw: serde_json::Value = self
            .client
            .graphql(&serde_json::json!({
                "query": REPLY_THREAD_MUTATION,
                "variables": {"id": thread_id, "body": body},
            }))
            .await
            .map_err(api)?;
        graphql_errors(&raw, "the reply-to-thread mutation")
    }

    async fn resolve_review_thread(&self, _repo: &RepoId, thread_id: &str) -> Result<()> {
        let raw: serde_json::Value = self
            .client
            .graphql(&serde_json::json!({
                "query": RESOLVE_THREAD_MUTATION,
                "variables": {"id": thread_id},
            }))
            .await
            .map_err(api)?;
        graphql_errors(&raw, "the resolve-thread mutation")
    }

    async fn merge(
        &self,
        repo: &RepoId,
        approval: &crate::automerge::policy::MergeApproved,
        method: &str,
    ) -> Result<()> {
        use octocrab::params::pulls::MergeMethod;

        // From the approval, not from a parameter: the pull request merged is
        // by construction the one the policy passed for.
        let number = approval.number();

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

/// The REST payload for one inline review comment.
///
/// Split out of `create_review` so the wire format is a pure, testable
/// function — the HTTP call itself can't run the offline suite.
fn review_comment_payload(c: &ReviewComment) -> serde_json::Value {
    // Every finding here is anchored to a line the diff actually touches (see
    // `anchored_in_diff`), always on the head revision. GitHub defaults `side`
    // to `RIGHT` when omitted, but naming it keeps that from being an implicit
    // fact one API change away from silently failing every inline comment.
    let mut comment = serde_json::json!({
        "path": c.path,
        "line": c.line,
        "side": "RIGHT",
        "body": c.body,
    });
    // A suggestion that spans more than one line anchors a *range*. The range
    // needs `start_line` and `start_side` on the wire together, or GitHub
    // treats the comment as pinned to `line` alone and a multi-line "Commit
    // suggestion" would replace just that one line, deleting the rest of the
    // block. `apply::inline_comments` refuses to build a range for a
    // single-line replacement, so a `Some` start always means a real range —
    // and `start_side` is `RIGHT` for the same reason `side` is.
    if let Some(start_line) = c.start_line {
        comment["start_line"] = serde_json::json!(start_line);
        comment["start_side"] = serde_json::json!("RIGHT");
    }
    comment
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn check(conclusion: Option<CheckConclusion>) -> CheckRun {
        CheckRun {
            name: "tinysweeper/review".into(),
            head_sha: "abc123".into(),
            conclusion,
            title: "t".into(),
            summary: "s".into(),
        }
    }

    #[test]
    fn an_unfinished_check_is_sent_as_in_progress_with_no_conclusion() {
        let body = check_payload(&check(None));
        assert_eq!(body["status"], "in_progress");
        // GitHub rejects the request outright if a conclusion rides along with
        // a non-completed status, and a rejected request is no check at all.
        assert!(body.get("conclusion").is_none());
    }

    #[test]
    fn a_finished_check_is_sent_as_completed_with_its_conclusion() {
        for (conclusion, wire) in [
            (CheckConclusion::Success, "success"),
            (CheckConclusion::Failure, "failure"),
            (CheckConclusion::ActionRequired, "action_required"),
            (CheckConclusion::Neutral, "neutral"),
            (CheckConclusion::Skipped, "skipped"),
        ] {
            let body = check_payload(&check(Some(conclusion)));
            assert_eq!(body["status"], "completed");
            assert_eq!(body["conclusion"], wire);
        }
    }

    #[test]
    fn the_create_payload_pins_a_commit_and_the_update_payload_does_not() {
        // `head_sha` is what binds a run to a commit; the update route rejects
        // it, so it is added at the create call site rather than in the shared
        // payload. This asserts the shared half stays free of it.
        assert!(check_payload(&check(None)).get("head_sha").is_none());
    }

    #[test]
    fn an_oversized_summary_is_truncated_rather_than_rejected() {
        let mut long = check(Some(CheckConclusion::Success));
        long.summary = "x".repeat(70_000);
        let body = check_payload(&long);
        assert_eq!(
            body["output"]["summary"].as_str().expect("a summary").len(),
            60_000,
            "the API caps a summary at 65535, and a rejected request reads as \
             the bot not having run"
        );
    }

    /// A two-link error chain shaped like octocrab's: an outer value whose own
    /// `Display` says nothing useful, wrapping the one that does.
    #[derive(Debug)]
    struct Outer(Inner);
    #[derive(Debug)]
    struct Inner(&'static str);

    impl std::fmt::Display for Outer {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            // Exactly what octocrab's `Error::GitHub` renders: the variant
            // name, and not one word about what went wrong.
            write!(f, "GitHub")
        }
    }
    impl std::fmt::Display for Inner {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "{}", self.0)
        }
    }
    impl std::error::Error for Outer {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            Some(&self.0)
        }
    }
    impl std::error::Error for Inner {}

    #[test]
    fn a_forge_error_carries_what_github_actually_said() {
        // The bug this pins: `err.to_string()` on octocrab's `GitHub` variant
        // is the literal word "GitHub", because the variant has no display
        // attribute and the status, message and errors array all live in the
        // source. Every 403, 404 and 422 in the product logged identically.
        let err = Outer(Inner("Resource not accessible by integration"));
        assert_eq!(err.to_string(), "GitHub", "the fixture must reproduce it");
        assert_eq!(
            chain(&err),
            "GitHub: Resource not accessible by integration"
        );
    }

    #[test]
    fn a_source_that_merely_repeats_the_outer_message_is_not_appended_twice() {
        let err = Outer(Inner("GitHub"));
        assert_eq!(chain(&err), "GitHub");
    }

    #[test]
    fn an_error_with_no_source_is_rendered_unchanged() {
        assert_eq!(chain(&Inner("plain")), "plain");
    }

    #[test]
    fn an_issue_carrying_a_native_type_reports_its_name() {
        let issue = issue_from_json(&json!({
            "number": 5,
            "title": "Crash on save",
            "body": "It crashes.",
            "user": {"login": "reporter"},
            "labels": [{"name": "priority: p1"}],
            "state": "open",
            "comments": 2,
            "type": {"id": 29989536, "name": "Bug"}
        }));

        assert_eq!(issue.number, 5);
        assert_eq!(issue.author, "reporter");
        assert_eq!(issue.labels, vec!["priority: p1".to_string()]);
        assert!(issue.open);
        assert_eq!(issue.comments, 2);
        assert_eq!(issue.issue_type, Some("Bug".to_string()));
    }

    #[test]
    fn an_issue_with_no_type_reports_none_rather_than_an_empty_name() {
        // The distinction is the whole guard: `None` means "nobody has chosen",
        // and an empty string would read as a choice tinysweeper may overwrite.
        let issue = issue_from_json(&json!({
            "number": 6,
            "title": "Add a dark theme",
            "body": null,
            "user": {"login": "reporter"},
            "labels": [],
            "state": "closed",
            "comments": 0,
            "type": null
        }));

        assert_eq!(issue.issue_type, None);
        assert!(!issue.open);
        assert!(issue.body.is_empty());
    }

    fn comment(
        path: &str,
        line: Option<u64>,
        start_line: Option<u64>,
        body: &str,
    ) -> ReviewComment {
        ReviewComment {
            path: path.to_string(),
            line,
            start_line,
            author: String::new(),
            body: body.to_string(),
        }
    }

    #[test]
    fn a_single_line_comment_carries_no_range_fields_on_the_wire() {
        let payload = review_comment_payload(&comment("src/lib.rs", Some(4), None, "Fix it."));

        assert_eq!(
            payload,
            json!({
                "path": "src/lib.rs",
                "line": 4,
                "side": "RIGHT",
                "body": "Fix it."
            })
        );
    }

    #[test]
    fn a_multi_line_comment_carries_start_line_and_start_side_on_the_wire() {
        let payload = review_comment_payload(&comment("src/lib.rs", Some(4), Some(2), "Fix it."));

        assert_eq!(
            payload,
            json!({
                "path": "src/lib.rs",
                "line": 4,
                "side": "RIGHT",
                "start_line": 2,
                "start_side": "RIGHT",
                "body": "Fix it."
            })
        );
    }

    #[test]
    fn the_type_names_an_owner_defines_are_read_in_the_order_returned() {
        let names = type_names_from_json(&json!([
            {"id": 29989535, "name": "Task", "description": "A specific piece of work"},
            {"id": 29989536, "name": "Bug"},
            {"id": 29989537, "name": "Feature"},
        ]));
        assert_eq!(
            names,
            vec!["Task".to_string(), "Bug".to_string(), "Feature".to_string()]
        );
    }

    #[test]
    fn an_answer_that_is_not_a_list_of_types_yields_no_types() {
        // What a user-account repository, or an organisation that never enabled
        // issue types, effectively returns. Not an error: it disables the
        // feature for that repository and triage carries on.
        assert!(type_names_from_json(&json!({"message": "Not Found"})).is_empty());
    }

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
    fn review_threads_map_their_resolved_state_and_bot_authors() {
        let raw = json!({
            "data": {"repository": {"pullRequest": {"reviewThreads": {
                "pageInfo": {"hasNextPage": false, "endCursor": null},
                "nodes": [
                    {
                        "id": "PRRT_1",
                        "isResolved": false,
                        "isOutdated": true,
                        "comments": {"nodes": [
                            {"author": {"login": "tinysweeper", "__typename": "Bot"},
                             "body": "finding"},
                            {"author": {"login": "author", "__typename": "User"},
                             "body": "fixed"}
                        ]}
                    },
                    {
                        "id": "PRRT_2",
                        "isResolved": true,
                        "isOutdated": false,
                        "comments": {"nodes": [
                            // A deleted account has no author at all; the
                            // comment must still map rather than drop the
                            // whole thread.
                            {"author": null, "body": "gone"}
                        ]}
                    }
                ]
            }}}}
        });

        let threads = threads_from_graphql(&raw);

        assert_eq!(threads.len(), 2);
        assert_eq!(threads[0].id, "PRRT_1");
        assert!(!threads[0].is_resolved);
        assert!(threads[0].is_outdated);
        assert!(threads[0].comments[0].bot, "a Bot author is a bot");
        assert!(!threads[0].comments[1].bot);
        assert!(threads[1].is_resolved);
        assert_eq!(threads[1].comments[0].author, "");
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
