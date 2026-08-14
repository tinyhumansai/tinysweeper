//! Review-thread resolution: deciding which of tinysweeper's own conversations
//! have been dealt with, so nobody has to close each one by hand.
//!
//! ## A model advises; policy applies
//!
//! Resolving a thread is a mutation, and `AGENTS.md` is explicit that a model
//! verdict is advisory. The review agent sees the prior finding alongside the
//! newly pushed diff and explicitly reports which findings the new code fixed.
//! Paired with GitHub's `isOutdated` signal, that advisory result is enough to
//! plan a close; [`apply_plan`] performs the eventual mutation without giving
//! the model a write handle.
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
//! - An unchanged-code thread whose only replies come from bots: two bots
//!   replying to each other is a loop nobody is watching.
//! - An already-resolved thread, which would otherwise be resolved forever.

pub mod advise;
pub mod types;

use std::collections::BTreeSet;

use crate::config::types::Config;
use crate::error::Result;
use crate::findings::prior::{is_own_login, title_in};
use crate::forge::types::{RepoId, ReviewThread};
use crate::ports::forge::{ForgeRead, ForgeWrite};
use crate::ports::model::{Model, Spend};

pub use crate::threads::types::{Decision, PlannedResolve, ThreadPlan};

/// Decide what to do with one thread, from what is already known.
///
/// `resolved` is the set of prior titles that the review agents explicitly
/// found fixed in the new diff. No forge call, no model call, no clock: this is
/// a pure policy check over that advisory evidence and the thread.
pub fn decide(thread: &ReviewThread, resolved: &BTreeSet<String>) -> Decision {
    if thread.is_resolved {
        return Decision::Leave("already resolved");
    }

    let Some(opener) = thread.comments.first() else {
        return Decision::Leave("an empty thread");
    };
    if !is_own_login(&opener.author) {
        return Decision::Leave("a thread tinysweeper did not open");
    }

    let Some(title) = title_in(&opener.body) else {
        return Decision::Leave("no finding title to check against");
    };

    match (thread.is_outdated, resolved.contains(&title)) {
        // The review agent received this earlier finding and the new diff, then
        // explicitly declared it fixed. A `synchronize` delivery caused that
        // review, while GitHub's outdated flag proves this thread's code moved.
        (true, true) => {
            Decision::Resolve("the review agent found this finding fixed in the new code")
        }
        (true, false) => Decision::Leave("the review agent did not confirm the finding is fixed"),
        // The code did not change, so nothing deterministic settles it: only
        // the reply itself could, and reading a reply is a model's job.
        (false, _) => {
            let human_replied = thread
                .comments
                .iter()
                .skip(1)
                .any(|comment| !comment.bot && !is_own_login(&comment.author));
            if human_replied {
                Decision::Ask
            } else {
                Decision::Leave("no human has replied")
            }
        }
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
    resolved: &BTreeSet<String>,
) -> Result<(ThreadPlan, Spend)> {
    let mut plan = ThreadPlan::default();
    let mut spend = Spend::default();

    if !config.threads.resolve_fixed {
        return Ok((plan, spend));
    }

    for thread in read.review_threads(repo, number).await? {
        match decide(&thread, resolved) {
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

/// How much of a SHA a resolution note shows.
///
/// GitHub's own abbreviation, and long enough to stay unambiguous in any
/// repository this will plausibly run on.
const SHORT_SHA: usize = 7;

/// The note posted in a thread just before it is resolved.
///
/// Written here, from a `&'static str` reason and a SHA, so no part of it can
/// come from a model or from a pull request. `reason` originates in
/// [`Decision`] — every one of its strings is a literal in this crate — and
/// `head_sha` is read off the forge, so the worst input this can render is a
/// malformed commit id.
pub fn resolution_note(reason: &str, head_sha: &str) -> String {
    let short: String = head_sha.chars().take(SHORT_SHA).collect();
    format!(
        "**Resolved** — {reason}, as of `{short}`.\n\n\
         <sub>If this is wrong, reopen the conversation and say so; \
         the finding will be re-raised on the next push if it still reproduces.</sub>"
    )
}

/// Execute a plan. The only mutation in this module.
///
/// `head_sha` is the commit the run reviewed, and it is what the note claims
/// the fix landed in — the caller has already checked it against live state,
/// so a note posted here cannot credit a commit nobody is looking at.
///
/// Returns how many threads were resolved. A thread that fails is logged and
/// the rest still run: one stale node id must not cost a pull request the whole
/// of its housekeeping.
///
/// The note is posted *before* the resolve, and its failure does not stop one.
/// Both orderings lose something when the second call fails; this one loses the
/// explanation for a thread that did close, rather than leaving a thread open
/// under a comment announcing it was resolved.
pub async fn apply_plan(
    write: &dyn ForgeWrite,
    config: &Config,
    repo: &RepoId,
    plan: &ThreadPlan,
    head_sha: &str,
) -> Result<usize> {
    let mut resolved = 0;
    for entry in &plan.resolve {
        if config.threads.comment_on_resolve {
            let note = resolution_note(&entry.reason, head_sha);
            if let Err(err) = write.reply_to_review_thread(repo, &entry.id, &note).await {
                tracing::warn!(%err, thread = %entry.id, "could not explain a resolve");
            }
        }
        match write.resolve_review_thread(repo, &entry.id).await {
            Ok(()) => resolved += 1,
            Err(err) => tracing::warn!(%err, thread = %entry.id, "could not resolve a thread"),
        }
    }
    Ok(resolved)
}

#[cfg(test)]
mod tests;
