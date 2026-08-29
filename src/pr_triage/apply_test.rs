//! Tests for the write half.
//!
//! Every one asserts on the exact writes `MockForge` recorded, in order,
//! because the orderings this module promises — label before unlabel, comment
//! before close — are the whole of its behaviour.

use super::*;
use crate::forge::mock::{MockForge, Write};
use crate::pr_triage::comment::MARKER;
use crate::pr_triage::types::{ClosePlan, TriagePlan, Verdict};

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
    // A read-only forge refuses nothing, so the failure is manufactured by
    // asking to close a pull request that is not there — which is exactly what
    // a sweep meets when somebody closes one by hand mid-run.
    let plans = vec![duplicate_plan(), {
        let mut second = duplicate_plan();
        second.number = 5799;
        second
    }];

    let forge = MockForge::new();
    let reports = apply_all(&forge, &repo(), &plans).await;

    assert_eq!(reports.len(), 2);
    assert!(
        reports
            .iter()
            .all(|report| report.outcome == Outcome::Labelled)
    );
    assert_eq!(reports[1].number, 5799);
    assert_eq!(reports[0].verdict, "triage: duplicate");
}

#[tokio::test]
async fn a_plan_with_nothing_to_write_reports_unchanged() {
    let plan = TriagePlan::new(1, Verdict::Review { because: "-" });
    let forge = MockForge::new();
    let reports = apply_all(&forge, &repo(), &[plan]).await;

    assert_eq!(reports[0].outcome, Outcome::Unchanged);
    assert!(forge.writes().is_empty());
}
