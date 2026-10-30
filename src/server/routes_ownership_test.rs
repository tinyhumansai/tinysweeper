//! Worker ownership under host stalls: heartbeat tolerance and logging, the
//! paid budget across ownership-fenced attempts, and how an exhausted head is
//! reported and re-requested.
//!
//! Every case here comes from the 2026-10-30 production logs: a swap-thrashing
//! host stalled workers past their owner lease, each stall spent a paid
//! attempt, and the head ended "Not reviewed" with no way to ask again.

use super::*;
use crate::ports::review_job_mock::MockReviewJobStore;
use std::time::Duration;

fn stalled_review(number: u64) -> ReviewRequest {
    ReviewRequest {
        id: crate::ports::review_job::ReviewJobId(format!("stall-{number}")),
        repo: "owner/repo".into(),
        number,
        author: "contributor".into(),
        installation: 1,
        head_sha: Some(format!("sha-{number}")),
        delivery: Some(format!("stall-{number}")),
        event: "pull_request".into(),
        mode: ReviewJobMode::Incremental,
        accepted_at_ms: unix_time_ms(),
    }
}

/// Acknowledge every queued check, as the status drainer would, so the
/// accepted job becomes claimable.
async fn acknowledge_queued_checks(store: &dyn ReviewJobStore, now_ms: i64) {
    static NEXT_CHECK_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(70_000);
    for check in store.claim_pending_checks(now_ms, 64).await.unwrap() {
        if check.retired {
            continue;
        }
        assert!(
            store
                .mark_queue_post_attempted(&check, now_ms)
                .await
                .unwrap()
        );
        assert!(
            store
                .record_queued_check_id(
                    &check,
                    NEXT_CHECK_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
                    now_ms,
                )
                .await
                .unwrap()
        );
    }
}

#[tokio::test(start_paused = true)]
async fn a_heartbeat_stalled_for_a_minute_keeps_the_local_owner_fence_open() {
    let store = Arc::new(MockReviewJobStore::default());
    store.admit_batch(vec![stalled_review(41)]).await.unwrap();
    let now = unix_time_ms();
    acknowledge_queued_checks(&*store, now).await;
    let owned = store.claim_next("stalled", now).await.unwrap().unwrap();

    let started = tokio::time::Instant::now();
    let (lost_tx, lost) = watch::channel(false);
    let (until_tx, until) = watch::channel(conservative_owner_deadline(started));
    store.delay_renew(Duration::from_secs(60));
    let _heartbeat = spawn_review_heartbeat(
        store.clone(),
        owned,
        lost_tx,
        until_tx,
        Duration::from_secs(15),
    );

    // The first renewal starts at 15 s and only returns at 75 s.
    tokio::time::sleep(Duration::from_secs(76)).await;
    assert!(!*lost.borrow(), "a slow renewal is not a lost renewal");
    assert!(
        tokio::time::Instant::now() < *until.borrow(),
        "a one-minute stall must not trip the local owner fence"
    );
}

#[derive(Clone, Default)]
struct CapturedLogs(Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for CapturedLogs {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl CapturedLogs {
    /// Route this thread's events here; the current-thread test runtime runs
    /// the spawned heartbeat on the same thread, so its events land here too.
    fn install(&self) -> tracing::subscriber::DefaultGuard {
        let writer = self.clone();
        tracing::subscriber::set_default(
            tracing_subscriber::fmt()
                .without_time()
                .with_ansi(false)
                .with_writer(move || writer.clone())
                .finish(),
        )
    }

    fn text(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
    }
}

#[tokio::test(start_paused = true)]
async fn a_slow_heartbeat_renewal_is_logged_with_its_elapsed_time() {
    let logs = CapturedLogs::default();
    let _logging = logs.install();
    let store = Arc::new(MockReviewJobStore::default());
    store.admit_batch(vec![stalled_review(42)]).await.unwrap();
    let now = unix_time_ms();
    acknowledge_queued_checks(&*store, now).await;
    let owned = store.claim_next("slow", now).await.unwrap().unwrap();
    let (lost_tx, lost) = watch::channel(false);
    let (until_tx, _until) =
        watch::channel(conservative_owner_deadline(tokio::time::Instant::now()));
    store.delay_renew(Duration::from_secs(6));
    let _heartbeat = spawn_review_heartbeat(
        store.clone(),
        owned,
        lost_tx,
        until_tx,
        Duration::from_secs(15),
    );

    tokio::time::sleep(Duration::from_secs(22)).await;
    assert!(!*lost.borrow());
    let text = logs.text();
    let line = text
        .lines()
        .find(|line| line.contains("WARN") && line.contains("slow"))
        .unwrap_or_else(|| panic!("no slow-renewal warning: {text}"));
    assert!(line.contains("elapsed_ms=6000"), "{line}");
    assert!(line.contains("job_id=stall-42"), "{line}");
}

#[tokio::test(start_paused = true)]
async fn a_heartbeat_that_finds_its_record_replaced_logs_the_lost_ownership() {
    let logs = CapturedLogs::default();
    let _logging = logs.install();
    let store = Arc::new(MockReviewJobStore::default());
    store.admit_batch(vec![stalled_review(43)]).await.unwrap();
    let now = unix_time_ms();
    acknowledge_queued_checks(&*store, now).await;
    let owned = store.claim_next("replaced", now).await.unwrap().unwrap();
    // Another path releases the record before the first heartbeat lands.
    assert!(store.defer(&owned, now, now).await.unwrap());
    let (lost_tx, lost) = watch::channel(false);
    let (until_tx, _until) =
        watch::channel(conservative_owner_deadline(tokio::time::Instant::now()));
    let _heartbeat = spawn_review_heartbeat(
        store.clone(),
        owned,
        lost_tx,
        until_tx,
        Duration::from_secs(15),
    );

    tokio::time::sleep(Duration::from_secs(16)).await;
    assert!(*lost.borrow());
    let text = logs.text();
    let line = text
        .lines()
        .find(|line| line.contains("ownership lost: record replaced or expired"))
        .unwrap_or_else(|| panic!("no lost-ownership warning: {text}"));
    assert!(line.contains("WARN"), "{line}");
    assert!(line.contains("job_id=stall-43"), "{line}");
}

fn worker_execution() -> Arc<crate::app::review_execution::ReviewExecution> {
    let started_at = tokio::time::Instant::now();
    let large_pr = Config::default().review.large_pr;
    crate::app::review_execution::ReviewRunContext::new(
        crate::app::review_execution::ReviewExecutionOptions {
            started_at,
            hard_deadline: started_at + REVIEW_DEADLINE,
            mode: crate::lanes::large_pr::ReviewMode::Normal,
            max_calls: None,
            soft_deadline_percent: large_pr.soft_deadline_percent,
            drain_seconds: large_pr.drain_seconds,
        },
    )
    .execution()
}

/// What a lane reports when the owner fence cancels its execution scope —
/// the "model execution refused: Cancelled" seen in production.
fn cancelled_by_owner_fence() -> Error {
    Error::ModelExecutionRefused {
        refusal: crate::ports::model_execution::ModelExecutionRefusal::Cancelled,
        usage: None,
        accounting_complete: true,
    }
}

/// One claimed generation of the accepted job with its head confirmed.
async fn next_owner(
    store: &MockReviewJobStore,
    owner: &str,
    number: u64,
    now_ms: i64,
) -> OwnedReview {
    acknowledge_queued_checks(store, now_ms).await;
    let owned = store
        .claim_next(owner, now_ms)
        .await
        .unwrap()
        .expect("the lapsed job is claimable");
    assert!(
        store
            .confirm_head(&owned, &format!("sha-{number}"), now_ms)
            .await
            .unwrap()
    );
    owned
}

#[tokio::test]
async fn ownership_lost_before_any_model_call_does_not_spend_the_paid_budget() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let store = MockReviewJobStore::default();
    store.admit_batch(vec![stalled_review(44)]).await.unwrap();
    let mut now_ms = unix_time_ms();
    let starts = AtomicUsize::new(0);
    // openhuman#8425: five stalls in a row, each fenced ~55 s into the lanes
    // with calls_attempted=0. One more than the whole budget here.
    let stalls = crate::ports::review_job::MAX_REVIEW_PAID_ATTEMPTS + 1;
    for generation in 0..stalls {
        let owned = next_owner(&store, &format!("stalled-{generation}"), 44, now_ms).await;
        let execution = worker_execution();
        let timing = crate::review_timing::ReviewTiming::new("owner/repo", 44, "Incremental");
        let result = review_with_refundable_paid_budget(
            &store,
            Some(&owned),
            "sha-44",
            now_ms,
            &execution,
            &timing,
            || async {
                starts.fetch_add(1, Ordering::SeqCst);
                Ok(())
            },
            || async { Ok(true) },
            || async {
                execution.cancel();
                Err(cancelled_by_owner_fence())
            },
        )
        .await;
        assert!(result.is_err());
        now_ms += crate::ports::review_job::REVIEW_OWNER_TTL_MS + 1;
    }
    assert_eq!(starts.load(Ordering::SeqCst), stalls as usize);

    let owned = next_owner(&store, "healthy", 44, now_ms).await;
    let execution = worker_execution();
    let timing = crate::review_timing::ReviewTiming::new("owner/repo", 44, "Incremental");
    let reviewed = review_with_refundable_paid_budget(
        &store,
        Some(&owned),
        "sha-44",
        now_ms,
        &execution,
        &timing,
        || async { Ok(()) },
        || async { Ok(true) },
        || async { Ok(InnerReview::Complete(None)) },
    )
    .await
    .expect("the healthy run proceeds");
    assert!(
        matches!(reviewed, InnerReview::Complete(_)),
        "fenced attempts without a model call left budget for this run"
    );
    assert_eq!(
        store.snapshot_for_test().incremental_heads[0].paid_attempts,
        1
    );
}

#[tokio::test]
async fn a_supersession_fence_before_any_model_call_is_refunded_without_a_cancel() {
    // A stale webhook hint can supersede a worker whose next owner confirms the
    // same live head. `check_work` returns `ReviewSuperseded` directly, before
    // anything cancels the execution scope, so the refund cannot rely on
    // `is_cancelled` alone.
    let store = MockReviewJobStore::default();
    store.admit_batch(vec![stalled_review(47)]).await.unwrap();
    let now_ms = unix_time_ms();
    let owned = next_owner(&store, "superseded", 47, now_ms).await;
    let execution = worker_execution();
    let timing = crate::review_timing::ReviewTiming::new("owner/repo", 47, "Incremental");
    let result = review_with_refundable_paid_budget(
        &store,
        Some(&owned),
        "sha-47",
        now_ms,
        &execution,
        &timing,
        || async { Err(Error::ReviewSuperseded) },
        || async { Ok(true) },
        || async { Ok(InnerReview::Complete(None)) },
    )
    .await;
    assert!(matches!(result, Err(Error::ReviewSuperseded)));
    assert!(!execution.is_cancelled());
    assert_eq!(
        store.snapshot_for_test().incremental_heads[0].paid_attempts,
        0,
        "a supersession fence before any model call spends nothing"
    );
}

#[tokio::test]
async fn an_attempt_fenced_after_a_model_call_still_spends_its_paid_start() {
    let store = MockReviewJobStore::default();
    store.admit_batch(vec![stalled_review(45)]).await.unwrap();
    let now_ms = unix_time_ms();
    let owned = next_owner(&store, "spent", 45, now_ms).await;
    let execution = worker_execution();
    let timing = crate::review_timing::ReviewTiming::new("owner/repo", 45, "Incremental");
    let result = review_with_refundable_paid_budget(
        &store,
        Some(&owned),
        "sha-45",
        now_ms,
        &execution,
        &timing,
        || async { Ok(()) },
        || async { Ok(true) },
        || async {
            timing.record(|record| record.calls_attempted += 1);
            execution.cancel();
            Err(Error::ReviewOwnershipLost)
        },
    )
    .await;
    assert!(matches!(result, Err(Error::ReviewOwnershipLost)));
    assert_eq!(
        store.snapshot_for_test().incremental_heads[0].paid_attempts,
        1
    );
}

#[tokio::test]
async fn an_ordinary_failure_before_any_model_call_still_spends_its_paid_start() {
    let store = MockReviewJobStore::default();
    store.admit_batch(vec![stalled_review(46)]).await.unwrap();
    let now_ms = unix_time_ms();
    let owned = next_owner(&store, "failing", 46, now_ms).await;
    let execution = worker_execution();
    let timing = crate::review_timing::ReviewTiming::new("owner/repo", 46, "Incremental");
    let result = review_with_refundable_paid_budget(
        &store,
        Some(&owned),
        "sha-46",
        now_ms,
        &execution,
        &timing,
        || async { Ok(()) },
        || async { Ok(true) },
        || async { Err(Error::Forge("transient".into())) },
    )
    .await;
    assert!(result.is_err());
    assert_eq!(
        store.snapshot_for_test().incremental_heads[0].paid_attempts,
        1
    );
}

#[tokio::test]
async fn an_exhausted_head_concludes_its_umbrella_check_as_a_visible_failure() {
    use crate::forge::{CheckConclusion, MockForge, RepoId, Write};

    let forge = MockForge::new();
    let repo = RepoId::parse("owner/repo").unwrap();
    let check_id = forge
        .publish_check(&repo, status::in_progress("sha-47"))
        .await
        .unwrap();
    let open = ReviewStatus {
        repo,
        check_id,
        head_sha: "sha-47".into(),
        installation: 1,
        stage: crate::ports::review_job::ReviewCheckStage::InProgress,
        owned: None,
        hub_comment_id: None,
        prior_hub_body: None,
    };
    let timing = crate::review_timing::ReviewTiming::new("owner/repo", 47, "Incremental");
    let disposition = finalize_exhausted_review(
        &timing,
        || async { Ok(()) },
        |conclusion| async {
            let (check, _) =
                conclude_status_with_writer(&open, conclusion, &forge, || async {}).await;
            check.expect("the open check is concluded");
        },
    )
    .await;
    assert!(matches!(
        disposition,
        WorkerDisposition::Terminal(TerminalOutcome::Exhausted)
    ));

    let concluded = forge
        .writes()
        .into_iter()
        .find_map(|write| match write {
            Write::CheckUpdate {
                check_id: id,
                check,
            } if id == check_id => Some(check),
            _ => None,
        })
        .expect("the umbrella check is concluded in place");
    assert_eq!(concluded.title, "Review failed — retry budget exhausted");
    assert_eq!(concluded.conclusion, Some(CheckConclusion::ActionRequired));
    assert!(
        concluded.summary.contains("@tinysweeper"),
        "the summary says how to ask again: {}",
        concluded.summary
    );
}

fn forge_status_clients(forge: crate::forge::MockForge) -> QueueStatusClientFactory {
    Arc::new(move |_| {
        let forge = forge.clone();
        async move {
            Ok(QueueStatusClients {
                read: Arc::new(forge.clone()),
                write: Arc::new(forge),
            })
        }
        .boxed()
    })
}

/// Spend every paid start on `sha-{number}` and record the head Exhausted,
/// as five fenced or failed attempts in production would.
async fn exhaust_head(store: &MockReviewJobStore, number: u64) -> i64 {
    store
        .admit_batch(vec![stalled_review(number)])
        .await
        .unwrap();
    let mut now_ms = unix_time_ms();
    for generation in 0..crate::ports::review_job::MAX_REVIEW_PAID_ATTEMPTS {
        let owned = next_owner(store, &format!("spent-{generation}"), number, now_ms).await;
        assert_eq!(
            store.begin_paid(&owned, now_ms).await.unwrap(),
            PaidStart::Started
        );
        now_ms += crate::ports::review_job::REVIEW_OWNER_TTL_MS + 1;
    }
    let owned = next_owner(store, "exhausted", number, now_ms).await;
    assert_eq!(
        store.begin_paid(&owned, now_ms).await.unwrap(),
        PaidStart::Exhausted
    );
    assert!(
        store
            .finish(&owned, TerminalOutcome::Exhausted, now_ms, now_ms)
            .await
            .unwrap()
    );
    assert_eq!(store.unfinished(), 0);
    now_ms
}

#[tokio::test]
async fn a_comment_rerequest_on_an_exhausted_head_is_admitted_and_reviewed() {
    use crate::forge::types::CheckRunStatus;
    use crate::forge::{MockForge, types::PullRequest};
    use crate::ports::review_job::ReconciledReviewCheck;

    let store = MockReviewJobStore::default();
    let now_ms = exhaust_head(&store, 48).await;
    let forge = MockForge::new().with_pull_request(
        PullRequest {
            number: 48,
            head_sha: "sha-48".into(),
            ..PullRequest::default()
        },
        vec![],
        vec![],
    );
    let comment = Action::Review {
        repo: "owner/repo".into(),
        number: 48,
        author: "maintainer".into(),
        installation: 1,
        head_sha: None,
    };
    assert_eq!(
        admit_webhook_review(
            &store,
            &comment,
            "rerequest-48",
            "issue_comment",
            forge_status_clients(forge.clone()),
        )
        .await
        .unwrap(),
        Some(Admission::Admitted)
    );
    let queued = forge
        .checks()
        .remove(status::CHECK_NAME)
        .expect("the re-requested head gets a queued check");
    assert_eq!(queued.head_sha, "sha-48");
    assert_eq!(queued.status, Some(CheckRunStatus::Queued));

    let owned = store
        .claim_next("rerequested", now_ms)
        .await
        .unwrap()
        .expect("the re-request is claimable");
    assert!(matches!(
        store
            .reconcile_live_check(&owned, "sha-48", now_ms)
            .await
            .unwrap(),
        ReconciledReviewCheck::Ready(_)
    ));
    assert!(!store.head_is_terminal(&owned, now_ms).await.unwrap());
    assert_eq!(
        store.begin_paid(&owned, now_ms).await.unwrap(),
        PaidStart::Started
    );
}

#[tokio::test]
async fn a_comment_on_a_published_head_still_does_not_rereview_it() {
    use crate::ports::review_job::ReconciledReviewCheck;

    let store = MockReviewJobStore::default();
    store.admit_batch(vec![stalled_review(49)]).await.unwrap();
    let now_ms = unix_time_ms();
    let owned = next_owner(&store, "publisher", 49, now_ms).await;
    assert_eq!(
        store.begin_paid(&owned, now_ms).await.unwrap(),
        PaidStart::Started
    );
    assert!(
        store
            .finish(&owned, TerminalOutcome::Published, now_ms, now_ms)
            .await
            .unwrap()
    );
    let mut comment = stalled_review(49);
    comment.id = crate::ports::review_job::ReviewJobId("webhook:comment-49".into());
    comment.head_sha = Some("sha-49".into());
    comment.event = "issue_comment".into();
    assert_eq!(
        store.admit_batch(vec![comment]).await.unwrap(),
        Admission::Admitted
    );
    let owned = store
        .claim_next("commenter", now_ms)
        .await
        .unwrap()
        .expect("the comment is claimable");
    assert!(matches!(
        store
            .reconcile_live_check(&owned, "sha-49", now_ms)
            .await
            .unwrap(),
        ReconciledReviewCheck::Terminal
    ));
}
