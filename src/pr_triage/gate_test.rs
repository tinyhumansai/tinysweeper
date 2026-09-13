//! Tests for the pull request close gate.
//!
//! One test per guard, and each one starts from a pull request that *would*
//! close — so a test that stops failing because the gate got looser elsewhere
//! is not possible.

use super::*;
use crate::config::types::PrClose;
use crate::forge::types::PullRequest;

/// The head the fixture pull request is at. A close names it, because the
/// verdict is only about the diff at that commit.
const HEAD: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

fn policy() -> PrClose {
    PrClose {
        enabled: true,
        min_age_days: 1,
        quiet_days: 0,
        protected_labels: vec!["pinned".into()],
        protected_authors: vec![],
        dry_run: false,
    }
}

fn subject() -> PullRequest {
    PullRequest {
        number: 5798,
        title: "docs: fix the Rust version".into(),
        author: "contributor".into(),
        head_sha: HEAD.into(),
        age_days: 30,
        quiet_days: 30,
        ..PullRequest::default()
    }
}

fn duplicate() -> Verdict {
    Verdict::Duplicate {
        of: 5789,
        of_head_sha: "cccccccccccccccccccccccccccccccccccccccc".into(),
        path_overlap: 1.0,
        line_overlap: 1.0,
    }
}

fn decide_with(subject: &PullRequest, verdict: &Verdict, policy: &PrClose) -> Outcome {
    decide(Inputs {
        subject,
        verdict,
        maintainers: &["maintainer".to_string()],
        policy,
    })
}

#[test]
fn a_duplicate_that_clears_every_guard_closes() {
    assert_eq!(
        decide_with(&subject(), &duplicate(), &policy()),
        Outcome::Close(ClosePlan {
            number: 5798,
            head_sha: HEAD.into(),
            dry_run: false
        })
    );
}

#[test]
fn dry_run_reaches_the_plan_rather_than_being_a_refusal() {
    // The distinction is load-bearing: a dry run still labels and still
    // comments, and only `apply` stops short of the close itself.
    let policy = PrClose {
        dry_run: true,
        ..policy()
    };
    assert_eq!(
        decide_with(&subject(), &duplicate(), &policy),
        Outcome::Close(ClosePlan {
            number: 5798,
            head_sha: HEAD.into(),
            dry_run: true
        })
    );
}

#[test]
fn closing_is_off_until_asked_for() {
    let policy = PrClose {
        enabled: false,
        ..policy()
    };
    assert_eq!(
        decide_with(&subject(), &duplicate(), &policy),
        Outcome::Refuse("pr_triage.close.enabled is off")
    );
}

#[test]
fn a_pull_request_worth_reading_is_never_closed() {
    assert_eq!(
        decide_with(&subject(), &Verdict::Review { because: "-" }, &policy()),
        Outcome::Refuse("the sweep found nothing that justifies a close")
    );
}

#[test]
fn a_draft_is_left_to_its_author() {
    let subject = PullRequest {
        draft: true,
        ..subject()
    };
    assert_eq!(
        decide_with(&subject, &duplicate(), &policy()),
        Outcome::Refuse("it is a draft")
    );
}

#[test]
fn a_merged_pull_request_is_not_closed_again() {
    let subject = PullRequest {
        merged: true,
        ..subject()
    };
    assert_eq!(
        decide_with(&subject, &duplicate(), &policy()),
        Outcome::Refuse("it is already merged")
    );
}

#[test]
fn an_already_closed_pull_request_is_not_closed_again() {
    let subject = PullRequest {
        open: false,
        ..subject()
    };
    assert_eq!(
        decide_with(&subject, &duplicate(), &policy()),
        Outcome::Refuse("it is already closed")
    );
}

#[test]
fn a_young_pull_request_is_left_alone() {
    let subject = PullRequest {
        age_days: 0,
        ..subject()
    };
    assert_eq!(
        decide_with(&subject, &duplicate(), &policy()),
        Outcome::Refuse("younger than pr_triage.close.min_age_days")
    );
}

#[test]
fn recent_activity_refuses_the_close() {
    let policy = PrClose {
        quiet_days: 7,
        ..policy()
    };
    let subject = PullRequest {
        quiet_days: 2,
        ..subject()
    };
    assert_eq!(
        decide_with(&subject, &duplicate(), &policy),
        Outcome::Refuse("active within pr_triage.close.quiet_days")
    );
}

#[test]
fn a_protected_label_refuses_the_close_whatever_its_case() {
    let subject = PullRequest {
        labels: vec!["Pinned".into()],
        ..subject()
    };
    assert_eq!(
        decide_with(&subject, &duplicate(), &policy()),
        Outcome::Refuse("carries a protected label")
    );
}

#[test]
fn a_maintainers_own_pull_request_stays_open() {
    let subject = PullRequest {
        author: "MAINTAINER".into(),
        ..subject()
    };
    assert_eq!(
        decide_with(&subject, &duplicate(), &policy()),
        Outcome::Refuse("opened by a maintainer or a protected author")
    );
}

#[test]
fn a_protected_author_stays_open() {
    let policy = PrClose {
        protected_authors: vec!["contributor".into()],
        ..policy()
    };
    assert_eq!(
        decide_with(&subject(), &duplicate(), &policy),
        Outcome::Refuse("opened by a maintainer or a protected author")
    );
}

#[test]
fn the_original_must_be_older_even_if_the_scorer_says_otherwise() {
    let verdict = Verdict::Duplicate {
        of: 9999,
        of_head_sha: "cccccccccccccccccccccccccccccccccccccccc".into(),
        path_overlap: 1.0,
        line_overlap: 1.0,
    };
    assert_eq!(
        decide_with(&subject(), &verdict, &policy()),
        Outcome::Refuse("the named original is not older than this pull request")
    );
}

#[test]
fn a_superseded_pull_request_closes_on_the_same_guards() {
    let verdict = Verdict::Superseded {
        base_ref: "main".into(),
        base_sha: "abc1234".into(),
        lines_checked: 12,
    };
    assert_eq!(
        decide_with(&subject(), &verdict, &policy()),
        Outcome::Close(ClosePlan {
            number: 5798,
            head_sha: HEAD.into(),
            dry_run: false
        })
    );
}
