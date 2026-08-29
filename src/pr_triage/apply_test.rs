//! Tests for the write half.
//!
//! Every one asserts on the exact writes `MockForge` recorded, in order,
//! because the orderings this module promises — label before unlabel, comment
//! before close — are the whole of its behaviour.

use super::*;
use crate::config::types::{Config, PrClose, PrTriage};
use crate::forge::mock::{MockForge, MockState, Write};
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

/// The head the original of a duplicate was read at.
const ORIGINAL: &str = "cccccccccccccccccccccccccccccccccccccccc";

/// A forge holding `subject` and the original it is said to duplicate.
///
/// A duplicate verdict rests on both, and `revalidate` re-reads both, so a
/// fixture with only the subject in it would fail for the wrong reason.
fn forge_with(subject: PullRequest) -> MockForge {
    MockForge::new()
        .with_pull_request(subject, vec![], vec![])
        .with_pull_request(
            PullRequest {
                number: 5789,
                head_sha: ORIGINAL.into(),
                ..closeable()
            },
            vec![],
            vec![],
        )
}

/// A pull request that clears every guard.
fn closeable() -> PullRequest {
    PullRequest {
        number: 5798,
        author: "contributor".into(),
        head_sha: HEAD.into(),
        age_days: 30,
        quiet_days: 30,
        ..PullRequest::default()
    }
}

/// The head the sweep read. A close is tied to it, so the fixtures have to
/// agree on one.
const HEAD: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

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
            of_head_sha: "cccccccccccccccccccccccccccccccccccccccc".into(),
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
        head_sha: HEAD.into(),
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
        head_sha: HEAD.into(),
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

    // Both pull requests are registered: `apply_all` re-reads live state before
    // writing, and fails closed when it cannot.
    let forge = forge_with(closeable())
        .with_pull_request(
            PullRequest {
                number: 5799,
                ..closeable()
            },
            vec![],
            vec![],
        )
        .refusing_unknown_comments();
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
    let forge = MockForge::new().with_pull_request(
        PullRequest {
            number: 1,
            ..closeable()
        },
        vec![],
        vec![],
    );
    let reports = apply_all(&forge, &forge, &config(), &repo(), &[plan], &[]).await;

    assert_eq!(reports[0].outcome, Outcome::Unchanged);
    assert!(forge.writes().is_empty());
}

// --- revalidation ---------------------------------------------------------

fn closing_plan() -> TriagePlan {
    let mut plan = duplicate_plan();
    plan.close = Some(ClosePlan {
        number: 5798,
        head_sha: HEAD.into(),
        dry_run: false,
    });
    plan
}

#[tokio::test]
async fn a_close_survives_a_pull_request_that_has_not_changed() {
    let forge = forge_with(closeable());
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
    let forge = forge_with(subject);

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
    let forge = forge_with(PullRequest {
        draft: true,
        ..closeable()
    });

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
        Some("its current state could not be re-checked before writing")
    );
    // And nothing else is written either: an unreadable state might be hiding
    // a kill switch a maintainer applied a moment ago.
    let forge = MockForge::new();
    let reports = apply_all(&forge, &forge, &config(), &repo(), &[closing_plan()], &[]).await;
    assert_eq!(reports[0].outcome, Outcome::LeftAlone);
    assert!(forge.writes().is_empty(), "{:?}", forge.writes());
}

#[tokio::test]
async fn a_dropped_close_takes_its_comment_claim_with_it() {
    // The comment says what was decided, so a decision reversed between the
    // sweep and the write has to reach the comment too.
    let forge = forge_with(PullRequest {
        draft: true,
        ..closeable()
    });

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

#[tokio::test]
async fn a_push_during_the_sweep_stops_the_close() {
    // The verdict was read off a diff. A new head is a new diff, so the
    // evidence no longer describes the pull request being closed.
    let forge = forge_with(PullRequest {
        head_sha: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".into(),
        ..closeable()
    });

    let mut plan = closing_plan();
    revalidate(&forge, &config(), &repo(), &mut plan, &[]).await;

    assert!(plan.close.is_none(), "{plan:?}");
    assert_eq!(
        plan.close_refusal,
        Some("it was pushed to after the sweep read its diff")
    );
}

#[tokio::test]
async fn a_plan_that_only_retires_a_label_is_not_reported_unchanged() {
    let mut plan = TriagePlan::new(1, Verdict::Review { because: "-" });
    plan.remove_labels = vec!["triage: duplicate".into()];

    let forge = MockForge::new().with_pull_request(
        PullRequest {
            number: 1,
            ..closeable()
        },
        vec![],
        vec![],
    );
    let reports = apply_all(&forge, &forge, &config(), &repo(), &[plan], &[]).await;

    assert_eq!(reports[0].outcome, Outcome::Labelled);
}

#[tokio::test]
async fn a_kill_switch_found_at_write_time_cancels_every_write() {
    // Not just the close. `tinysweeper:human-review` means leave this alone,
    // and a run that noticed it and still applied the label and the comment
    // would honour the letter of the setting and none of its meaning.
    let subject = PullRequest {
        labels: vec!["tinysweeper:human-review".into()],
        ..closeable()
    };
    let forge = forge_with(subject);

    let reports = apply_all(&forge, &forge, &config(), &repo(), &[closing_plan()], &[]).await;

    assert_eq!(reports[0].outcome, Outcome::LeftAlone);
    assert!(forge.writes().is_empty(), "{:?}", forge.writes());
}

#[tokio::test]
async fn the_second_gate_is_what_catches_a_push_during_the_writes() {
    // The first gate cannot see this: the head moves *after* it has run and the
    // label and comment have gone out. Driven directly, because the whole point
    // is the window between the two calls — a test that pushed before
    // `apply_all` would pass on the first gate alone and would still pass if
    // the second were deleted.
    let forge = forge_with(closeable());
    let mut plan = closing_plan();

    // First gate: clean.
    assert_eq!(
        revalidate(&forge, &config(), &repo(), &mut plan, &[]).await,
        Recheck::Unchanged
    );
    assert!(plan.close.is_some());

    // The writes happen, and the contributor pushes inside them.
    let mut writes = plan.clone();
    let close = writes.close.take();
    apply_plan(&forge, &repo(), &writes).await.expect("writes");
    forge.push(5798, "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb", vec![]);

    // Second gate: refuses.
    plan.close = close;
    revalidate_at(
        &forge,
        &config(),
        &repo(),
        &mut plan,
        &[],
        Freshness::SinceOurOwnWrites,
    )
    .await;

    assert!(plan.close.is_none(), "{plan:?}");
    assert_eq!(
        plan.close_refusal,
        Some("it was pushed to after the sweep read its diff")
    );
    assert!(
        !forge
            .writes()
            .iter()
            .any(|write| matches!(write, Write::PullRequestClosed { .. }))
    );
}

#[tokio::test]
async fn the_second_gate_does_not_trip_over_our_own_comment() {
    // `quiet_days` reads `updated_at`, which our own comment just bumped. A
    // second gate that re-applied it would drop every close and repeat the
    // cycle forever — the bot resetting the clock it then waits on.
    let policy = Config {
        pr_triage: PrTriage {
            close: PrClose {
                quiet_days: 30,
                ..config().pr_triage.close
            },
            ..config().pr_triage
        },
        ..config()
    };
    let forge = forge_with(PullRequest {
        // Touched today, exactly as our own comment would leave it.
        quiet_days: 0,
        ..closeable()
    });

    let mut plan = closing_plan();
    revalidate_at(
        &forge,
        &policy,
        &repo(),
        &mut plan,
        &[],
        Freshness::SinceOurOwnWrites,
    )
    .await;
    assert!(plan.close.is_some(), "{plan:?}");

    // The full gate, which runs before any of our writes, still applies it.
    let mut plan = closing_plan();
    revalidate(&forge, &policy, &repo(), &mut plan, &[]).await;
    assert_eq!(
        plan.close_refusal,
        Some("active within pr_triage.close.quiet_days")
    );
}

#[tokio::test]
async fn a_push_to_the_original_also_stops_the_close() {
    // A duplicate verdict rests on two diffs. Pinning only the subject's would
    // leave half the evidence free to move.
    let forge = forge_with(closeable());

    // Unmoved: the close stands.
    let mut plan = closing_plan();
    revalidate(&forge, &config(), &repo(), &mut plan, &[]).await;
    assert!(plan.close.is_some(), "{plan:?}");

    // Moved: it does not.
    forge.push(5789, "dddddddddddddddddddddddddddddddddddddddd", vec![]);
    let mut plan = closing_plan();
    revalidate(&forge, &config(), &repo(), &mut plan, &[]).await;
    assert!(plan.close.is_none(), "{plan:?}");
    assert_eq!(
        plan.close_refusal,
        Some("the pull request it duplicates changed after the sweep read it")
    );
}

#[tokio::test]
async fn a_duplicate_of_something_itself_closed_unmerged_is_not_closed() {
    // Closing a contribution as a duplicate of a pull request that has itself
    // been abandoned loses both, which is the one outcome nobody wants: the
    // change simply disappears.
    let forge = MockForge::new()
        .with_pull_request(closeable(), vec![], vec![])
        .with_pull_request(
            PullRequest {
                number: 5789,
                head_sha: ORIGINAL.into(),
                open: false,
                merged: false,
                ..closeable()
            },
            vec![],
            vec![],
        );

    let mut plan = closing_plan();
    revalidate(&forge, &config(), &repo(), &mut plan, &[]).await;

    assert!(plan.close.is_none(), "{plan:?}");
    assert_eq!(
        plan.close_refusal,
        Some("the pull request it duplicates was itself closed unmerged")
    );
}

#[tokio::test]
async fn a_superseded_close_is_dropped_when_the_base_branch_moves() {
    // A force-push or a revert between the sweep and the write takes the lines
    // back off the branch, and the whole finding with them.
    let mut plan = TriagePlan::new(
        5798,
        Verdict::Superseded {
            base_ref: "main".into(),
            base_sha: "1111111111111111111111111111111111111111".into(),
            lines_checked: 12,
        },
    );
    plan.close = Some(ClosePlan {
        number: 5798,
        head_sha: HEAD.into(),
        dry_run: false,
    });

    let mut moved = MockState::default();
    moved.pull_requests.insert(5798, closeable());
    moved.branches.insert(
        "main".into(),
        "2222222222222222222222222222222222222222".into(),
    );
    let forge = MockForge::with_state(moved);

    revalidate(&forge, &config(), &repo(), &mut plan, &[]).await;

    assert!(plan.close.is_none(), "{plan:?}");
    assert_eq!(
        plan.close_refusal,
        Some("its base branch moved after the sweep compared against it")
    );
}

#[tokio::test]
async fn a_kill_switch_stops_a_plan_that_was_not_going_to_close_anything() {
    // The label means "leave this alone", and a maintainer who applies it is
    // owed that whether or not a close happened to be on the table.
    let forge = MockForge::new().with_pull_request(
        PullRequest {
            labels: vec!["tinysweeper:human-review".into()],
            ..closeable()
        },
        vec![],
        vec![],
    );

    let mut plan = duplicate_plan();
    assert!(plan.close.is_none());

    let reports = apply_all(&forge, &forge, &config(), &repo(), &[plan.clone()], &[]).await;

    assert_eq!(reports[0].outcome, Outcome::LeftAlone);
    assert!(forge.writes().is_empty(), "{:?}", forge.writes());

    let recheck = revalidate(&forge, &config(), &repo(), &mut plan, &[]).await;
    assert_eq!(recheck, Recheck::LeaveAlone);
}
