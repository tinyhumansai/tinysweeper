//! `local-review`: the review engine over a local git range.
//!
//! Always compiled. Every input comes from `git` and the filesystem, so this
//! path needs no GitHub token, makes no forge call, and leaves nothing behind.
//! The only thing it spends is model tokens.
//!
//! # Why it exists
//!
//! A prompt change used to be validated by opening a pull request and paying
//! for a real review, which is slow, costs money every iteration, and leaves
//! comments on somebody's branch. This runs the *same* engine — the same
//! scanners, the same lanes, the same filtering, dedupe and capping — against a
//! range in the checkout you already have.
//!
//! # It is always a first review
//!
//! There is no forge to read prior findings back off, and no durable store, so
//! `review.incremental` has nothing to work with: every run is a cold review
//! with an empty prompt cache. That is a limitation worth stating rather than
//! hiding, because it means the cache-hit line in the output is always zero and
//! must not be read as a regression against the numbers a real review posts.
//!
//! # The description is synthesised
//!
//! A git range has no title and no body, and the `description` lane's entire
//! subject is whether those match the diff. Rather than invent a description
//! and let a lane grade the invention, the title defaults to the newest
//! commit's subject and the body to empty — and `--title` / `--body` exist so
//! the lane can be exercised against something real.

use std::path::Path;
use std::sync::Arc;

use crate::app::review::{Proposal, review_with_state};
use crate::config::types::Config;
use crate::error::Result;
use crate::evidence::git::{self, Range, ResolvedRange};
use crate::forge::mock::{MockForge, MockState};
use crate::forge::types::{PullRequest, RepoId};
use crate::ports::model::Model;

/// The pull request number a local range is reviewed under.
///
/// Zero can never collide with a real pull request, which matters because the
/// number reaches the state key and the proposal on disk. A local run must not
/// be mistakable for a review of `#1`.
pub const LOCAL_NUMBER: u64 = 0;

/// What to review, and how to describe it.
#[derive(Debug, Clone)]
pub struct LocalInput {
    /// The revisions to diff.
    pub range: Range,
    /// Title shown to the `description` lane. Defaults to the newest commit's
    /// subject.
    pub title: Option<String>,
    /// Body shown to the `description` lane. Defaults to empty, which that lane
    /// treats as a finding — accurately, for a range nobody has written up.
    pub body: Option<String>,
}

/// Everything a local review resolved before it spent a token.
///
/// Returned alongside the proposal so a caller can report what was actually
/// reviewed: which merge base, which head, and whether uncommitted work was
/// included.
#[derive(Debug, Clone)]
pub struct LocalContext {
    /// The repository, from `origin` when it names one.
    pub repo: RepoId,
    /// The resolved range.
    pub range: ResolvedRange,
}

/// Review a local git range and return the proposal it would have published.
///
/// Nothing is written anywhere: there is no `ForgeWrite` on this path at all,
/// which is the same type-level guarantee a lane has.
pub async fn local_review(
    dir: &Path,
    input: &LocalInput,
    model: Arc<dyn Model>,
    config: &Config,
) -> Result<(Proposal, LocalContext)> {
    let range = git::resolve(dir, &input.range).await?;
    let repo = match git::origin_repo(dir).await {
        Some(repo) => repo,
        None => git::local_repo_id(dir),
    };

    let pull_request = PullRequest {
        number: LOCAL_NUMBER,
        title: input.title.clone().unwrap_or_else(|| default_title(&range)),
        body: input.body.clone().unwrap_or_default(),
        author: "local".to_string(),
        base_ref: input.range.base.clone(),
        base_sha: range.base_sha.clone(),
        head_ref: input.range.head.clone().unwrap_or_else(|| "HEAD".into()),
        head_sha: range.head_sha.clone(),
        ..PullRequest::default()
    };

    let mut state = MockState::default();
    // The instruction files the knowledge pass reads, served at the head the
    // diff was actually taken from — the working tree when it is dirty. A
    // sandboxed extraction that read the committed `AGENTS.md` while the review
    // read the uncommitted diff would silently disagree with itself.
    for name in &config.knowledge.files {
        if let Some(content) = git::file_at(dir, &range, name).await? {
            state.set_file(&range.head_sha, name, &content);
        }
    }

    let forge = MockForge::with_state(state).with_pull_request(
        pull_request,
        range.files.clone(),
        range.commits.clone(),
    );

    // No store: there is no earlier local run to replay, and pretending
    // otherwise would report a cache hit that never happened.
    let proposal = review_with_state(&forge, model, config, &repo, LOCAL_NUMBER, None).await?;

    Ok((proposal, LocalContext { repo, range }))
}

/// The subject line of the newest commit in the range.
fn default_title(range: &ResolvedRange) -> String {
    range
        .commits
        .last()
        .and_then(|commit| commit.message.lines().next())
        .map(str::to_string)
        .unwrap_or_else(|| "Uncommitted changes".to_string())
}

#[cfg(test)]
#[path = "local_test.rs"]
mod tests;
