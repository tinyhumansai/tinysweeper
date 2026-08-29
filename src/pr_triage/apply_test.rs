//! Tests for the write half.
//!
//! Every one asserts on the exact writes `MockForge` recorded, in order,
//! because the orderings this module promises — label before unlabel, comment
//! before close — are the whole of its behaviour.

use super::*;
use crate::config::types::{Config, PrClose, PrTriage};
use crate::forge::mock::{MockForge, Write};
use crate::forge::types::PullRequest;
use crate::pr_triage::comment::MARKER;
use crate::pr_triage::types::{ClosePlan, TriagePlan, Verdict};

/// A deployment with closing fully on, so a refusal in these tests is always
/// the guard under test and never the policy being off.
fn config() -> Config {
    let mut config = Config {
        pr_triage: PrTriage {
            enabled: true,
            close: PrClose {
                enabled: true,
                min_age_days: 1,
                ..PrClose::default()
            },
            ..PrTriage::default()
        },
        ..Config::default()
    };
    config.issues.block_labels = vec!["tinysweeper:human-review".into()];
    config
}

/// A pull request that clears every guard.
fn closeable() -> PullRequest {
    PullRequest {
        number: 5798,
        author: "contributor".into(),
        age_days: 30,
        quiet_days: 30,
        ..PullRequest::default()
    }
}

fn repo() -> RepoId {
    RepoId {
        owner: "acme".into(),
        name: "widget".into(),
    }
}

fn duplicate_plan() -> TriagePlan {
    let mut plan = TriagePlan::new(
        5798,
        Verdict::Duplicate {
            of: 5789,
            path_overlap: 1.0,
            line_overlap: 1.0,
        },
    );
    plan.add_labels = vec!["triage: duplicate".into()];
    plan.comment = Some(format!("{MARKER}\nsame as #5789"));
    plan
}

#[tokio::test]
async fn a_label_and_a_comment_are_written_and_nothing_is_closed() {
    let forge = MockForge::new();
    apply_plan(&forge, &repo(), &duplicate_plan())
        .await
        .expect("applies");

    assert_eq!(
        forge.writes(),
        vec![
            Write::Labels {
                number: 5798,
                labels: vec!["triage: duplicate".into()],
            },
            Write::Comment {
                number: 5798,
                body: format!("{MARKER}\nsame as #5789"),
            },
        ]
    );
}

#[tokio::test]
async fn the_comment_goes_up_before_the_close() {
    let mut plan = duplicate_plan();
    plan.close = Some(ClosePlan {
        number: 5798,
        dry_run: false,
    });

    let forge = MockForge::new();
    apply_plan(&forge, &repo(), &plan).await.expect("applies");

    let writes = forge.writes();
    let comment = writes
        .iter()
        .position(|write| matches!(write, Write::Comment { .. }))
        .expect("a comment");
    let close = writes
        .iter()
        .position(|write| matches!(write, Write::PullRequestClosed { .. }))
        .expect("a close");
    assert!(comment < close, "{writes:?}");
}

#[tokio::test]
async fn a_dry_run_says_everything_and_closes_nothing() {
    let mut plan = duplicate_plan();
    plan.close = Some(ClosePlan {
        number: 5798,
        dry_run: true,
    });

    let forge = MockForge::new();
    apply_plan(&forge, &repo(), &plan).await.expect("applies");

    assert!(
        !forge
            .writes()
            .iter()
            .any(|write| matches!(write, Write::PullRequestClosed { .. })),
        "a dry run closed a pull request"
    );
    assert!(
        forge
            .writes()
            .iter()
            .any(|write| matches!(write, Write::Comment { .. }))
    );
}

#[tokio::test]
async fn a_second_sweep_edits_its_own_comment_rather_than_adding_one() {
    let mut plan = duplicate_plan();
    plan.comment_id = Some(99);

    let forge = MockForge::new();
    apply_plan(&forge, &repo(), &plan).await.expect("applies");

    assert!(
        forge
            .writes()
            .iter()
            .any(|write| matches!(write, Write::CommentUpdate { comment_id: 99, .. })),
        "{:?}",
        forge.writes()
    );
    assert!(
        !forge
            .writes()
            .iter()
            .any(|write| matches!(write, Write::Comment { .. }))
    );
}

#[tokio::test]
async fn the_new_label_goes_on_before_the_old_one_comes_off() {
    let mut plan = duplicate_plan();
    plan.remove_labels = vec!["triage: review".into()];

    let forge = MockForge::new();
    apply_plan(&forge, &repo(), &plan).await.expect("applies");

    let writes = forge.writes();
    let add = writes
        .iter()
        .position(|write| matches!(write, Write::Labels { .. }))
        .expect("an add");
    let remove = writes
        .iter()
        .position(|write| matches!(write, Write::LabelRemoved { .. }))
        .expect("a removal");
    assert!(add < remove, "{writes:?}");
}

#[tokio::test]
async fn one_failure_does_not_abandon_the_rest_of_the_sweep() {
    // A real failure, not an assumed one. `update_comment` against an id the
    // mock has never issued is refused, which is exactly what happens when a
    // maintainer deletes the bot's comment between the sweep and the write.
    let mut failing = duplicate_plan();
    failing.comment_id = Some(999_999);

    let mut second = duplicate_plan();
    second.number = 5799;

    let forge = MockForge::new().refusing_unknown_comments();
    let reports = apply_all(&forge, &forge, &config(), &repo(), &[failing, second], &[]).await;

    assert_eq!(reports.len(), 2, "the sweep abandoned the rest");
    assert!(
        matches!(reports[0].outcome, Outcome::Failed(_)),
        "{:?}",
        reports[0]
    );
    // The point: the second one still ran.
    assert_eq!(reports[1].number, 5799);
    assert_eq!(reports[1].outcome, Outcome::Labelled);
    assert_eq!(reports[1].verdict, "triage: duplicate");
}

#[tokio::test]
async fn a_plan_with_nothing_to_write_reports_unchanged() {
    let plan = TriagePlan::new(1, Verdict::Review { because: "-" });
    let forge = MockForge::new();
    let reports = apply_all(&forge, &forge, &config(), &repo(), &[plan], &[]).await;

    assert_eq!(reports[0].outcome, Outcome::Unchanged);
    assert!(forge.writes().is_empty());
}

// --- revalidation ---------------------------------------------------------

fn closing_plan() -> TriagePlan {
    let mut plan = duplicate_plan();
    plan.close = Some(ClosePlan {
        number: 5798,
        dry_run: false,
    });
    plan
}

#[tokio::test]
async fn a_close_survives_a_pull_request_that_has_not_changed() {
    let forge = MockForge::new().with_pull_request(closeable(), vec![], vec![]);
    let mut plan = closing_plan();
    revalidate(&forge, &config(), &repo(), &mut plan, &[]).await;
    assert!(plan.close.is_some(), "{plan:?}");
}

#[tokio::test]
async fn a_kill_switch_added_during_the_sweep_stops_the_close() {
    let subject = PullRequest {
        labels: vec!["tinysweeper:human-review".into()],
        ..closeable()
    };
    let forge = MockForge::new().with_pull_request(subject, vec![], vec![]);

    let mut plan = closing_plan();
    revalidate(&forge, &config(), &repo(), &mut plan, &[]).await;

    assert!(plan.close.is_none(), "{plan:?}");
    assert_eq!(
        plan.close_refusal,
        Some("it carries a label that switches the bot off")
    );
}

#[tokio::test]
async fn a_pull_request_marked_draft_during_the_sweep_stops_the_close() {
    let forge = MockForge::new().with_pull_request(
        PullRequest {
            draft: true,
            ..closeable()
        },
        vec![],
        vec![],
    );

    let mut plan = closing_plan();
    revalidate(&forge, &config(), &repo(), &mut plan, &[]).await;

    assert!(plan.close.is_none(), "{plan:?}");
    assert_eq!(plan.close_refusal, Some("it is a draft"));
}

#[tokio::test]
async fn a_pull_request_that_cannot_be_re_read_is_not_closed() {
    // "We could not check" and "it is no longer allowed" are the same answer
    // when the action cannot be undone. The mock has no such pull request.
    let forge = MockForge::new();
    let mut plan = closing_plan();
    revalidate(&forge, &config(), &repo(), &mut plan, &[]).await;

    assert!(plan.close.is_none(), "{plan:?}");
    assert_eq!(
        plan.close_refusal,
        Some("its current state could not be re-checked before closing")
    );
}

#[tokio::test]
async fn a_dropped_close_takes_its_comment_claim_with_it() {
    // The comment says what was decided, so a decision reversed between the
    // sweep and the write has to reach the comment too.
    let forge = MockForge::new().with_pull_request(
        PullRequest {
            draft: true,
            ..closeable()
        },
        vec![],
        vec![],
    );

    let reports = apply_all(&forge, &forge, &config(), &repo(), &[closing_plan()], &[]).await;

    assert_eq!(reports[0].outcome, Outcome::Labelled);
    let posted = forge
        .writes()
        .into_iter()
        .find_map(|write| match write {
            Write::Comment { body, .. } => Some(body),
            _ => None,
        })
        .expect("a comment");
    assert!(!posted.contains("Closing it on that basis"), "{posted}");
    assert!(posted.contains("Left open: it is a draft"), "{posted}");
    assert!(
        !forge
            .writes()
            .iter()
            .any(|write| matches!(write, Write::PullRequestClosed { .. }))
    );
}
