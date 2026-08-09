//! Review-thread resolution: deciding which of tinysweeper's own conversations
//! have been dealt with, so nobody has to close each one by hand.
//!
//! ## A model does not decide this
//!
//! Resolving a thread is a mutation, and `AGENTS.md` is explicit that a model
//! verdict is advisory. So the decision is deterministic and costs nothing: a
//! finding carries a fingerprint, `apply` writes it into the comment that opens
//! the thread, and a fingerprint absent from the current run's findings is a
//! finding that stopped reproducing. Paired with GitHub's own `isOutdated` —
//! the code the thread anchors to has changed — that is enough to close the
//! conversation without asking anybody anything.
//!
//! The model is reached for in one case only: the code did not change and a
//! human replied, which is the "you have misunderstood, this is fine" case no
//! fingerprint can settle. Its verdict is advisory, it is gated behind
//! `threads.ask_model` (**default off**), and even with the flag on the
//! mutation is performed by [`apply_plan`] from a plan this module built.
//!
//! ## What is never touched
//!
//! - A thread a human opened. Only threads whose first comment is ours, matched
//!   by exact login via [`crate::findings::prior::is_own_login`] — a prefix
//!   match would count `tinysweeper-anything` as ourselves.
//! - A thread whose finding still reproduces.
//! - A thread whose only replies come from bots: two bots replying to each
//!   other is a loop nobody is watching.
//! - An already-resolved thread, which would otherwise be resolved forever.

pub mod advise;
pub mod types;

use std::collections::BTreeSet;

use crate::config::types::Config;
use crate::error::Result;
use crate::findings::prior::{fingerprint_in, is_own_login};
use crate::forge::types::{RepoId, ReviewThread};
use crate::ports::forge::{ForgeRead, ForgeWrite};
use crate::ports::model::{Model, Spend};

pub use crate::threads::types::{Decision, PlannedResolve, ThreadPlan};

/// Decide what to do with one thread, from what is already known.
///
/// `current` is the set of fingerprints this run's findings produced. No forge
/// call, no model call, no clock: the decision is a pure function of the thread
/// and the findings, which is what makes it auditable.
pub fn decide(thread: &ReviewThread, current: &BTreeSet<String>) -> Decision {
    if thread.is_resolved {
        return Decision::Leave("already resolved");
    }

    let Some(opener) = thread.comments.first() else {
        return Decision::Leave("an empty thread");
    };
    if !is_own_login(&opener.author) {
        return Decision::Leave("a thread tinysweeper did not open");
    }

    let Some(fingerprint) = fingerprint_in(&opener.body) else {
        return Decision::Leave("no fingerprint to check against");
    };

    // Somebody has to have replied, and it has to be a person. The trigger for
    // re-evaluating a thread is a human comment; without this, a bot's reply —
    // or tinysweeper's own follow-up — starts the cycle again.
    let human_replied = thread
        .comments
        .iter()
        .skip(1)
        .any(|comment| !comment.bot && !is_own_login(&comment.author));
    if !human_replied {
        return Decision::Leave("no human has replied");
    }

    match (thread.is_outdated, current.contains(&fingerprint)) {
        // The code under the comment changed and the finding is gone. That is
        // the entire deterministic rule.
        (true, false) => Decision::Resolve("the finding no longer reproduces on the new code"),
        (true, true) => Decision::Leave("the finding still reproduces"),
        // The code did not change, so nothing deterministic settles it: only
        // the reply itself could, and reading a reply is a model's job.
        (false, _) => Decision::Ask,
    }
}

/// Build the plan of threads to resolve for one pull request.
///
/// Reads only — the plan is executed later by [`apply_plan`], the one function
/// here that holds a write handle.
///
/// Returns the spend alongside the plan so the caller can fold it into the
/// run's total. An advisory call whose cost is not merged is invisible money.
pub async fn plan(
    read: &dyn ForgeRead,
    model: Option<&dyn Model>,
    config: &Config,
    repo: &RepoId,
    number: u64,
    current: &BTreeSet<String>,
) -> Result<(ThreadPlan, Spend)> {
    let mut plan = ThreadPlan::default();
    let mut spend = Spend::default();

    if !config.threads.resolve_fixed {
        return Ok((plan, spend));
    }

    for thread in read.review_threads(repo, number).await? {
        match decide(&thread, current) {
            Decision::Resolve(reason) => plan.resolve.push(PlannedResolve {
                id: thread.id.clone(),
                reason: reason.to_string(),
            }),
            Decision::Leave(_) => {}
            Decision::Ask => {
                // Advisory, and off unless an operator turned it on. With the
                // flag off the thread is left for a human, which is exactly the
                // behaviour that existed before this module.
                let (Some(model), true) = (model, config.threads.ask_model) else {
                    continue;
                };
                let (resolve, call) = advise::ask(model, config, &thread).await?;
                spend.merge(call);
                if resolve {
                    plan.resolve.push(PlannedResolve {
                        id: thread.id.clone(),
                        reason: "the reply explains why it is not a problem (advisory)".into(),
                    });
                }
            }
        }
    }

    Ok((plan, spend))
}

/// Execute a plan. The only mutation in this module.
///
/// Returns how many threads were resolved. A thread that fails is logged and
/// the rest still run: one stale node id must not cost a pull request the whole
/// of its housekeeping.
pub async fn apply_plan(write: &dyn ForgeWrite, repo: &RepoId, plan: &ThreadPlan) -> Result<usize> {
    let mut resolved = 0;
    for entry in &plan.resolve {
        match write.resolve_review_thread(repo, &entry.id).await {
            Ok(()) => resolved += 1,
            Err(err) => tracing::warn!(%err, thread = %entry.id, "could not resolve a thread"),
        }
    }
    Ok(resolved)
}

#[cfg(test)]
mod tests;
