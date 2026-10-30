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
        assert!(store.mark_queue_post_attempted(&check, now_ms).await.unwrap());
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
