//! Auto-merge: the deterministic gate that may put code on the default branch.
//!
//! One of only two modules permitted to mutate GitHub, and the only one that
//! can do so without a human ever looking. Every criterion is arithmetic over
//! observable state — check conclusions, review states, file counts, paths,
//! SHAs — and no model output reaches any of it. "Does this look safe to
//! merge?" is not a question asked here, because it is precisely the question
//! that gets answered plausibly and wrongly.
//!
//! Everything fails closed. An unreadable threshold, a diff the forge would
//! not render, a `mergeable` state GitHub has not computed yet: each is a
//! refusal, never an assumption. A wrongly-merged pull request cannot be
//! un-merged; one left alone merely waits for a person.

pub mod complexity;
pub mod paths;
pub mod policy;
pub mod types;

use crate::automerge::policy::Snapshot;
pub use crate::automerge::policy::evaluate;
use crate::automerge::types::{Decision, Outcome, Refusal};
use crate::config::types::AutoMerge;
use crate::error::Result;
use crate::forge::types::RepoId;
use crate::ports::forge::{ForgeRead, ForgeWrite};

#[cfg(test)]
mod test;

/// Read everything the policy judges, at one instant.
///
/// The checks are read against the head SHA the pull request reports rather
/// than against the pull request number, so a push mid-read cannot produce a
/// snapshot whose checks describe a different commit from its files.
pub async fn snapshot(read: &dyn ForgeRead, repo: &RepoId, number: u64) -> Result<Snapshot> {
    let pull_request = read.pull_request(repo, number).await?;
    let checks = read.check_runs(repo, &pull_request.head_sha).await?;
    Ok(Snapshot {
        files: read.changed_files(repo, number).await?,
        checks,
        reviews: read.reviews(repo, number).await?,
        pull_request,
    })
}

/// Evaluate a pull request and, if it qualifies, merge it.
///
/// The whole job: read, decide, re-read, decide again, merge. Nothing is
/// written on any path but the last one.
pub async fn merge_if_qualified(
    read: &dyn ForgeRead,
    write: &dyn ForgeWrite,
    config: &AutoMerge,
    repo: &RepoId,
    number: u64,
) -> Result<Outcome> {
    // Cheap and total: with the feature off there is nothing to read, and a
    // disabled job should not spend an API call proving it.
    if !config.enabled {
        return Ok(Outcome::Refused(Refusal::Disabled));
    }

    let taken = snapshot(read, repo, number).await?;
    if let Decision::Refuse(refusal) = evaluate(config, &taken) {
        tracing::info!(number, reason = %refusal, "not auto-merging");
        return Ok(Outcome::Refused(refusal));
    }

    merge_snapshot(read, write, config, repo, &taken).await
}

/// Re-validate an already-taken snapshot against live state, then merge.
///
/// The re-validation is the point of the split, and it mirrors
/// `app::apply`: the decision was reached against one commit, and between
/// then and now a push may have replaced it. Merging on the strength of checks
/// that went green on a commit nobody is looking at any more is the exact
/// failure this module cannot be allowed to have, so the whole policy — not
/// merely the SHA — is evaluated a second time against freshly read state.
pub async fn merge_snapshot(
    read: &dyn ForgeRead,
    write: &dyn ForgeWrite,
    config: &AutoMerge,
    repo: &RepoId,
    taken: &Snapshot,
) -> Result<Outcome> {
    let live = snapshot(read, repo, taken.pull_request.number).await?;

    if live.pull_request.head_sha != taken.pull_request.head_sha {
        let refusal = Refusal::HeadMoved {
            evaluated: taken.pull_request.head_sha.clone(),
            live: live.pull_request.head_sha.clone(),
        };
        tracing::info!(number = taken.pull_request.number, reason = %refusal, "not auto-merging");
        return Ok(Outcome::Refused(refusal));
    }

    // The approval this produces is the only way to reach `write.merge` below,
    // so the second evaluation is now load-bearing in the type system as well
    // as in the control flow: deleting it would not merely skip a check, it
    // would leave nothing to pass to the merge.
    let approval = match evaluate(config, &live) {
        Decision::Refuse(refusal) => {
            tracing::info!(number = live.pull_request.number, reason = %refusal, "not auto-merging");
            return Ok(Outcome::Refused(refusal));
        }
        Decision::Allow(approval) => approval,
    };

    // The method comes from config because repositories disable merge methods:
    // squash is off on this one, and a hardcoded method would mean either
    // never merging or merging in a shape the repository has ruled out.
    let method = config.method.clone();
    match write.merge(repo, &approval, &method).await {
        Ok(()) => {
            tracing::info!(number = live.pull_request.number, %method, "auto-merged");
            Ok(Outcome::Merged { method })
        }
        // The forge refusing is not an error to propagate. A disabled merge
        // method, a branch-protection rule, a human merging first: in every
        // case the pull request is exactly where it was, which is the safe
        // state, and the next run will try again.
        Err(err) => {
            let reason = err.to_string();
            tracing::warn!(number = live.pull_request.number, %method, %reason, "the forge refused the merge");
            Ok(Outcome::Rejected { method, reason })
        }
    }
}
