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
//!
//! ## Explained once
//!
//! A thread that already carries our resolution note is still resolved when
//! the policy says so, but never explained a second time. Notes from before the
//! resolve-first ordering sit under threads whose resolve GitHub refused; once
//! the installation has the permission, those must close, silently.

pub mod advise;
pub mod types;

use std::collections::BTreeSet;

use crate::config::types::Config;
use crate::error::{Error, Result};
use crate::findings::prior::{is_own_login, title_in};
use crate::forge::types::{RepoId, ReviewThread, ThreadComment};
use crate::ports::forge::{ForgeRead, ForgeWrite};
use crate::ports::model::{Model, Spend};

pub use crate::threads::types::{ApplyReport, Decision, PlannedResolve, ThreadPlan};

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
        let noted = thread.comments.iter().any(is_own_resolution_note);
        match decide(&thread, resolved) {
            Decision::Resolve(reason) => plan.resolve.push(PlannedResolve {
                id: thread.id.clone(),
                reason: reason.to_string(),
                noted,
            }),
            Decision::Leave(_) => {}
            Decision::Ask => {
                // Advisory and enabled by default. An operator can turn it off
                // to leave unchanged-code conversations for a human; either
                // way, deterministic policy still owns the eventual write.
                let (Some(model), true) = (model, config.threads.ask_model) else {
                    continue;
                };
                let (resolve, call) = advise::ask(model, config, &thread).await?;
                spend.merge(call);
                if resolve {
                    plan.resolve.push(PlannedResolve {
                        id: thread.id.clone(),
                        reason: "the reply explains why it is not a problem (advisory)".into(),
                        noted,
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

/// The hidden marker every resolution note carries.
///
/// What lets the next run see that this thread was already explained. The
/// marker alone is not trusted — see [`is_own_resolution_note`].
pub const RESOLVED_NOTE_MARKER: &str = "<!-- tinysweeper:resolved-note -->";

/// How a resolution note began before it carried [`RESOLVED_NOTE_MARKER`].
///
/// Those notes are still on GitHub — hundreds of them, one per push, under
/// threads whose resolve was refused — and must count as already explained.
const LEGACY_NOTE_PREFIX: &str = "**Resolved** — ";

/// Whether `comment` is a resolution note tinysweeper itself posted.
///
/// Ours by author *and* by text, like every other marker check: anyone can
/// paste the marker into a reply, and doing so must not pin a thread open.
fn is_own_resolution_note(comment: &ThreadComment) -> bool {
    is_own_login(&comment.author)
        && (comment.body.contains(RESOLVED_NOTE_MARKER)
            || comment.body.trim_start().starts_with(LEGACY_NOTE_PREFIX))
}

/// Whether a forge error is GitHub refusing for want of permission.
///
/// Matched on the rendered message because both shapes arrive as
/// [`Error::Forge`] text: REST's `403 Resource not accessible by integration`
/// and GraphQL's `"type":"FORBIDDEN"` entry in an otherwise-200 response.
/// A status code alone is not matched — `403` can appear inside a node id.
pub fn is_permission_denied(err: &Error) -> bool {
    let message = err.to_string().to_ascii_lowercase();
    ["not accessible by integration", "forbidden", "permission"]
        .iter()
        .any(|needle| message.contains(needle))
}

/// The note posted in a thread once it has been resolved.
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
         the finding will be re-raised on the next push if it still reproduces.</sub>\n\n\
         {RESOLVED_NOTE_MARKER}"
    )
}

/// Execute a plan. The only mutation in this module.
///
/// `head_sha` is the commit the run reviewed, and it is what the note claims
/// the fix landed in — the caller has already checked it against live state,
/// so a note posted here cannot credit a commit nobody is looking at.
///
/// The resolve comes **first**, and the note is posted only once it succeeded.
/// The other order announced resolves GitHub then refused, and since nothing
/// closed, the next push planned the same thread and announced it again — one
/// more note per push, forever. This order can lose the explanation for a
/// thread that did close, if the reply fails; that is the cheaper loss.
///
/// A thread the plan marks `noted` already carries our explanation, so it is
/// resolved without a second one.
///
/// A thread that fails is logged and the rest still run: one stale node id
/// must not cost a pull request the whole of its housekeeping. A *permission*
/// refusal is different — every later resolve would be refused the same way —
/// so it is logged once and the rest of the plan is skipped.
pub async fn apply_plan(
    write: &dyn ForgeWrite,
    config: &Config,
    repo: &RepoId,
    plan: &ThreadPlan,
    head_sha: &str,
) -> Result<ApplyReport> {
    let mut report = ApplyReport::default();
    for (index, entry) in plan.resolve.iter().enumerate() {
        if let Err(err) = write.resolve_review_thread(repo, &entry.id).await {
            report.failed += 1;
            if is_permission_denied(&err) {
                report.skipped = plan.resolve.len() - index - 1;
                // A stable message an operator can alert on: the fix is the
                // installation's `Pull requests: write` permission, not code.
                tracing::warn!(
                    %err,
                    thread = %entry.id,
                    skipped = report.skipped,
                    "review thread resolve refused for want of permission; \
                     skipping the rest of this run"
                );
                break;
            }
            tracing::warn!(%err, thread = %entry.id, "could not resolve a thread");
            continue;
        }
        report.resolved += 1;
        if config.threads.comment_on_resolve && !entry.noted {
            let note = resolution_note(&entry.reason, head_sha);
            if let Err(err) = write.reply_to_review_thread(repo, &entry.id, &note).await {
                tracing::warn!(%err, thread = %entry.id, "could not explain a resolve");
            }
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests;
