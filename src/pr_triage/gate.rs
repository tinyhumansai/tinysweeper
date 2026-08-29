//! The deterministic close gate for pull requests.
//!
//! One sentence, deliberately shaped like the issue gate's:
//!
//! > tinysweeper closes a pull request only when `pr_triage.close.enabled` is
//! > on, the pull request is open, not merged, not a draft, at least
//! > `min_age_days` old, quiet for `quiet_days`, carries no protected label,
//! > was opened by neither a maintainer nor a `protected_author`, and the sweep
//! > reached a verdict of duplicate or superseded — which it can only do from
//! > the diff itself.
//!
//! A pure function over already-gathered facts: no forge, no model, no clock,
//! no environment. Every guard is testable on its own, and none of them can be
//! talked out of, because there is nothing here for prose to talk to.
//!
//! The one field [`crate::config::types::IssueClose`] has that
//! [`PrClose`] does not is `confidence_min`, and its absence is the design.
//! Nothing on this path produces a confidence, because nothing on this path
//! produces an opinion.

use crate::config::types::PrClose;
use crate::forge::types::PullRequest;
use crate::pr_triage::types::{ClosePlan, Verdict};

/// Everything the gate needs, gathered by the caller.
#[derive(Debug, Clone, Copy)]
pub struct Inputs<'a> {
    /// The pull request under consideration.
    pub subject: &'a PullRequest,
    /// What the sweep concluded from the diff.
    pub verdict: &'a Verdict,
    /// Repository logins treated as maintainers. Their pull requests stay open.
    pub maintainers: &'a [String],
    /// The `[pr_triage.close]` policy.
    pub policy: &'a PrClose,
}

/// The gate's verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Close it, on these terms.
    Close(ClosePlan),
    /// Leave it open, for this reason.
    Refuse(&'static str),
}

/// Decide whether this pull request may be closed.
///
/// The guards run cheapest-and-most-categorical first, so the refusal that
/// reaches the log is the one a maintainer would have given.
pub fn decide(inputs: Inputs<'_>) -> Outcome {
    let Inputs {
        subject,
        verdict,
        maintainers,
        policy,
    } = inputs;

    if !policy.enabled {
        return Outcome::Refuse("pr_triage.close.enabled is off");
    }
    if !verdict.is_closeable() {
        return Outcome::Refuse("the sweep found nothing that justifies a close");
    }
    if subject.merged {
        return Outcome::Refuse("it is already merged");
    }
    // A draft is the author saying the work is not finished. Judging an
    // unfinished change against the base branch and closing it is the rudest
    // thing this bot could do, and the author will open it for review when they
    // are ready — at which point the sweep sees it again.
    if subject.draft {
        return Outcome::Refuse("it is a draft");
    }
    if subject.age_days < policy.min_age_days {
        return Outcome::Refuse("younger than pr_triage.close.min_age_days");
    }
    if subject.quiet_days < policy.quiet_days {
        return Outcome::Refuse("active within pr_triage.close.quiet_days");
    }
    if subject
        .labels
        .iter()
        .any(|label| contains(&policy.protected_labels, label))
    {
        return Outcome::Refuse("carries a protected label");
    }
    if contains(maintainers, &subject.author)
        || contains(&policy.protected_authors, &subject.author)
    {
        return Outcome::Refuse("opened by a maintainer or a protected author");
    }
    // Last, because it is the one guard that is about the *evidence* rather
    // than about the pull request, and a reader of the log wants the specific
    // reason before the generic one.
    if let Verdict::Duplicate { of, .. } = verdict
        && *of >= subject.number
    {
        return Outcome::Refuse("the named original is not older than this pull request");
    }

    Outcome::Close(ClosePlan {
        number: subject.number,
        head_sha: subject.head_sha.clone(),
        dry_run: policy.dry_run,
    })
}

/// Login and label comparison, case-insensitively: GitHub is not case
/// sensitive here and a guard that is would be trivially side-stepped.
fn contains(haystack: &[String], needle: &str) -> bool {
    haystack
        .iter()
        .any(|item| item.trim().eq_ignore_ascii_case(needle.trim()))
}

#[cfg(test)]
#[path = "gate_test.rs"]
mod tests;
