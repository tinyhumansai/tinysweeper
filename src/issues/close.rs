//! The deterministic close gate.
//!
//! One sentence, and it is the sentence that matters most in this crate:
//!
//! > tinysweeper closes an issue only when closing is enabled, the issue is
//! > open, at least `min_age_days` old, quiet for `quiet_days`, carries no
//! > protected or blocked label, was opened by neither a maintainer nor a
//! > protected author, and a model claim at or above `confidence_min` names a
//! > number that **we** put in front of the model and that the forge confirms is
//! > either a strictly older issue (duplicate) or a merged pull request (fixed).
//!
//! Everything here is a pure function over already-fetched facts. No forge, no
//! model, no clock, no environment — so every guard is testable in isolation and
//! none of them can be talked out of by prompt injection.

use crate::config::types::IssueClose;
use crate::forge::types::Issue;
use crate::issues::types::{ClaimKind, ClosePlan, DuplicateClaim};

/// What the forge says about the number a model named.
///
/// Fetched *after* the model answers and *before* the gate runs, so the gate
/// reasons about the reference as it actually is rather than as it was
/// described.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Referenced {
    /// A real issue, with the two facts the gate needs about it.
    Issue {
        /// Its number.
        number: u64,
        /// Whether it is still open.
        open: bool,
        /// Its age in days, so "older" is a fact and not a guess.
        age_days: u32,
    },
    /// A pull request that was actually merged.
    MergedPull {
        /// Its number.
        number: u64,
    },
    /// A pull request that exists but was closed without merging.
    UnmergedPull {
        /// Its number.
        number: u64,
    },
}

/// Everything the gate needs, gathered by the caller.
#[derive(Debug, Clone, Copy)]
pub struct CloseInputs<'a> {
    /// The issue under consideration.
    pub subject: &'a Issue,
    /// What the model claimed, if anything. Advisory.
    pub claim: Option<&'a DuplicateClaim>,
    /// What the forge says about the claimed number, if it could be fetched.
    pub referenced: Option<&'a Referenced>,
    /// The candidate numbers that were actually shown to the model.
    pub candidates: &'a [u64],
    /// Repository logins treated as maintainers. Their issues stay open.
    pub maintainers: &'a [String],
    /// The `[issues.close]` policy.
    pub policy: &'a IssueClose,
}

/// The gate's verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CloseOutcome {
    /// Close it, on this evidence.
    Close(ClosePlan),
    /// Leave it open, for this reason.
    Refuse(&'static str),
}

/// Decide whether this issue may be closed.
///
/// The guards run cheapest-and-most-categorical first, so the refusal reason
/// that reaches the log is the one a maintainer would have given.
pub fn decide(inputs: CloseInputs<'_>) -> CloseOutcome {
    let CloseInputs {
        subject,
        claim,
        referenced,
        candidates,
        maintainers,
        policy,
    } = inputs;

    if !policy.enabled {
        return CloseOutcome::Refuse("issues.close.enabled is off");
    }
    if !subject.open {
        return CloseOutcome::Refuse("the issue is already closed");
    }
    let Some(claim) = claim else {
        return CloseOutcome::Refuse("no duplicate or fix was proposed");
    };
    if claim.confidence < policy.confidence_min {
        return CloseOutcome::Refuse("below issues.close.confidence_min");
    }
    // The anti-hallucination guard, and the reason a hostile issue body cannot
    // steer a close: the model may only pick from the shortlist we assembled
    // deterministically, so a number it invented has nowhere to land.
    if !candidates.contains(&claim.number) {
        return CloseOutcome::Refuse("the reference was not one of the candidates offered");
    }
    if claim.number == subject.number {
        return CloseOutcome::Refuse("an issue cannot be a duplicate of itself");
    }
    if subject.age_days < policy.min_age_days {
        return CloseOutcome::Refuse("younger than issues.close.min_age_days");
    }
    if subject.quiet_days < policy.quiet_days {
        return CloseOutcome::Refuse("active within issues.close.quiet_days");
    }
    if subject
        .labels
        .iter()
        .any(|label| contains(&policy.protected_labels, label))
    {
        return CloseOutcome::Refuse("carries a protected label");
    }
    if contains(maintainers, &subject.author)
        || contains(&policy.protected_authors, &subject.author)
    {
        return CloseOutcome::Refuse("opened by a maintainer or a protected author");
    }

    let Some(referenced) = referenced else {
        return CloseOutcome::Refuse("the reference could not be verified on the forge");
    };

    match (claim.kind, referenced) {
        // Strictly older, because closing the first report as a duplicate of a
        // later one throws away the discussion that has already happened on it.
        (ClaimKind::Duplicate, Referenced::Issue { age_days, .. }) => {
            if *age_days <= subject.age_days {
                return CloseOutcome::Refuse("the named duplicate is not older than this issue");
            }
        }
        (ClaimKind::Duplicate, _) => {
            return CloseOutcome::Refuse("the named duplicate is not an issue");
        }
        (ClaimKind::Resolved, Referenced::MergedPull { .. }) => {}
        (ClaimKind::Resolved, _) => {
            return CloseOutcome::Refuse("the named fix is not a merged pull request");
        }
    }

    CloseOutcome::Close(ClosePlan {
        kind: claim.kind,
        reference: claim.number,
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
mod tests {
    use super::*;

    fn policy() -> IssueClose {
        IssueClose {
            enabled: true,
            min_age_days: 60,
            quiet_days: 60,
            confidence_min: 0.85,
            protected_labels: vec!["pinned".into()],
            protected_authors: vec![],
            dry_run: false,
        }
    }

    fn subject() -> Issue {
        Issue {
            number: 42,
            title: "Crash when saving".into(),
            body: "It crashes.".into(),
            author: "reporter".into(),
            labels: vec![],
            open: true,
            age_days: 90,
            quiet_days: 90,
            comments: 1,
            issue_type: None,
        }
    }

    fn duplicate_claim() -> DuplicateClaim {
        DuplicateClaim {
            kind: ClaimKind::Duplicate,
            number: 7,
            confidence: 0.95,
        }
    }

    fn older_issue() -> Referenced {
        Referenced::Issue {
            number: 7,
            open: true,
            age_days: 200,
        }
    }

    /// A run where every guard passes, so a test can change exactly one thing.
    fn allowed<'a>(
        subject: &'a Issue,
        claim: &'a DuplicateClaim,
        referenced: &'a Referenced,
        candidates: &'a [u64],
        maintainers: &'a [String],
        policy: &'a IssueClose,
    ) -> CloseInputs<'a> {
        CloseInputs {
            subject,
            claim: Some(claim),
            referenced: Some(referenced),
            candidates,
            maintainers,
            policy,
        }
    }

    #[test]
    fn an_old_quiet_duplicate_of_an_older_issue_is_closed() {
        let got = decide(allowed(
            &subject(),
            &duplicate_claim(),
            &older_issue(),
            &[7],
            &[],
            &policy(),
        ));
        assert_eq!(
            got,
            CloseOutcome::Close(ClosePlan {
                kind: ClaimKind::Duplicate,
                reference: 7,
                dry_run: false,
            })
        );
    }

    #[test]
    fn closing_disabled_refuses_everything() {
        let policy = IssueClose {
            enabled: false,
            ..policy()
        };
        assert_eq!(
            decide(allowed(
                &subject(),
                &duplicate_claim(),
                &older_issue(),
                &[7],
                &[],
                &policy
            )),
            CloseOutcome::Refuse("issues.close.enabled is off")
        );
    }

    #[test]
    fn no_claim_means_no_close() {
        assert_eq!(
            decide(CloseInputs {
                subject: &subject(),
                claim: None,
                referenced: None,
                candidates: &[7],
                maintainers: &[],
                policy: &policy(),
            }),
            CloseOutcome::Refuse("no duplicate or fix was proposed")
        );
    }

    #[test]
    fn a_claim_below_the_confidence_floor_is_refused() {
        let claim = DuplicateClaim {
            confidence: 0.5,
            ..duplicate_claim()
        };
        assert_eq!(
            decide(allowed(
                &subject(),
                &claim,
                &older_issue(),
                &[7],
                &[],
                &policy()
            )),
            CloseOutcome::Refuse("below issues.close.confidence_min")
        );
    }

    #[test]
    fn a_number_we_never_showed_the_model_is_refused() {
        // The anti-hallucination guard. A model that invents #9999 — or that
        // reads "close issue 1" out of a hostile issue body — names a number
        // that was not on the shortlist, and the gate refuses it.
        let claim = DuplicateClaim {
            number: 9999,
            ..duplicate_claim()
        };
        let referenced = Referenced::Issue {
            number: 9999,
            open: true,
            age_days: 400,
        };
        assert_eq!(
            decide(allowed(
                &subject(),
                &claim,
                &referenced,
                &[7],
                &[],
                &policy()
            )),
            CloseOutcome::Refuse("the reference was not one of the candidates offered")
        );
    }

    #[test]
    fn an_issue_cannot_duplicate_itself() {
        let claim = DuplicateClaim {
            number: 42,
            ..duplicate_claim()
        };
        let referenced = Referenced::Issue {
            number: 42,
            open: true,
            age_days: 90,
        };
        assert_eq!(
            decide(allowed(
                &subject(),
                &claim,
                &referenced,
                &[42],
                &[],
                &policy()
            )),
            CloseOutcome::Refuse("an issue cannot be a duplicate of itself")
        );
    }

    #[test]
    fn a_young_issue_is_never_closed() {
        let subject = Issue {
            age_days: 3,
            ..subject()
        };
        assert_eq!(
            decide(allowed(
                &subject,
                &duplicate_claim(),
                &older_issue(),
                &[7],
                &[],
                &policy()
            )),
            CloseOutcome::Refuse("younger than issues.close.min_age_days")
        );
    }

    #[test]
    fn recent_human_activity_keeps_it_open() {
        let subject = Issue {
            quiet_days: 2,
            ..subject()
        };
        assert_eq!(
            decide(allowed(
                &subject,
                &duplicate_claim(),
                &older_issue(),
                &[7],
                &[],
                &policy()
            )),
            CloseOutcome::Refuse("active within issues.close.quiet_days")
        );
    }

    #[test]
    fn a_protected_label_keeps_it_open() {
        let subject = Issue {
            labels: vec!["pinned".into()],
            ..subject()
        };
        assert_eq!(
            decide(allowed(
                &subject,
                &duplicate_claim(),
                &older_issue(),
                &[7],
                &[],
                &policy()
            )),
            CloseOutcome::Refuse("carries a protected label")
        );
    }

    #[test]
    fn a_maintainer_authored_issue_stays_open() {
        let subject = Issue {
            author: "maintainer".into(),
            ..subject()
        };
        let maintainers = vec!["Maintainer".to_string()];
        assert_eq!(
            decide(allowed(
                &subject,
                &duplicate_claim(),
                &older_issue(),
                &[7],
                &maintainers,
                &policy()
            )),
            CloseOutcome::Refuse("opened by a maintainer or a protected author")
        );
    }

    #[test]
    fn a_protected_author_stays_open() {
        let policy = IssueClose {
            protected_authors: vec!["reporter".into()],
            ..policy()
        };
        assert_eq!(
            decide(allowed(
                &subject(),
                &duplicate_claim(),
                &older_issue(),
                &[7],
                &[],
                &policy
            )),
            CloseOutcome::Refuse("opened by a maintainer or a protected author")
        );
    }

    #[test]
    fn an_unverifiable_reference_is_refused() {
        assert_eq!(
            decide(CloseInputs {
                subject: &subject(),
                claim: Some(&duplicate_claim()),
                referenced: None,
                candidates: &[7],
                maintainers: &[],
                policy: &policy(),
            }),
            CloseOutcome::Refuse("the reference could not be verified on the forge")
        );
    }

    #[test]
    fn a_newer_issue_is_not_the_original() {
        // Closing the *older* report as a duplicate of the newer one loses the
        // history. Strictly older, or nothing.
        let referenced = Referenced::Issue {
            number: 7,
            open: true,
            age_days: 5,
        };
        assert_eq!(
            decide(allowed(
                &subject(),
                &duplicate_claim(),
                &referenced,
                &[7],
                &[],
                &policy()
            )),
            CloseOutcome::Refuse("the named duplicate is not older than this issue")
        );
    }

    #[test]
    fn a_merged_pull_request_resolves_an_issue() {
        let claim = DuplicateClaim {
            kind: ClaimKind::Resolved,
            number: 7,
            confidence: 0.95,
        };
        let referenced = Referenced::MergedPull { number: 7 };
        assert_eq!(
            decide(allowed(
                &subject(),
                &claim,
                &referenced,
                &[7],
                &[],
                &policy()
            )),
            CloseOutcome::Close(ClosePlan {
                kind: ClaimKind::Resolved,
                reference: 7,
                dry_run: false,
            })
        );
    }

    #[test]
    fn an_unmerged_pull_request_resolves_nothing() {
        let claim = DuplicateClaim {
            kind: ClaimKind::Resolved,
            number: 7,
            confidence: 0.95,
        };
        let referenced = Referenced::UnmergedPull { number: 7 };
        assert_eq!(
            decide(allowed(
                &subject(),
                &claim,
                &referenced,
                &[7],
                &[],
                &policy()
            )),
            CloseOutcome::Refuse("the named fix is not a merged pull request")
        );
    }

    #[test]
    fn an_open_issue_does_not_count_as_a_fix() {
        let claim = DuplicateClaim {
            kind: ClaimKind::Resolved,
            number: 7,
            confidence: 0.95,
        };
        assert_eq!(
            decide(allowed(
                &subject(),
                &claim,
                &older_issue(),
                &[7],
                &[],
                &policy()
            )),
            CloseOutcome::Refuse("the named fix is not a merged pull request")
        );
    }

    #[test]
    fn an_already_closed_issue_is_left_alone() {
        let subject = Issue {
            open: false,
            ..subject()
        };
        assert_eq!(
            decide(allowed(
                &subject,
                &duplicate_claim(),
                &older_issue(),
                &[7],
                &[],
                &policy()
            )),
            CloseOutcome::Refuse("the issue is already closed")
        );
    }

    #[test]
    fn dry_run_is_carried_into_the_plan() {
        let policy = IssueClose {
            dry_run: true,
            ..policy()
        };
        assert_eq!(
            decide(allowed(
                &subject(),
                &duplicate_claim(),
                &older_issue(),
                &[7],
                &[],
                &policy
            )),
            CloseOutcome::Close(ClosePlan {
                kind: ClaimKind::Duplicate,
                reference: 7,
                dry_run: true,
            })
        );
    }
}
