//! Offline contract tests for admission, recovery ownership, and terminal dedupe.

use super::super::review_job_mock::MockReviewJobStore;
use super::{
    Admission, MAX_RUNNING_REVIEWS, MAX_TERMINAL_REVIEW_RESERVATIONS, MAX_UNFINISHED_REVIEWS,
    REVIEW_OWNER_TTL_MS, REVIEW_TOMBSTONE_TTL_MS, ReviewJobId, ReviewJobMode, ReviewJobStore,
    ReviewRequest, ReviewSchedulerState, ReviewTombstone, TerminalOutcome,
};

fn request(id: &str, accepted_at_ms: i64) -> ReviewRequest {
    ReviewRequest {
        id: ReviewJobId(id.to_owned()),
        repo: "owner/repo".into(),
        number: 7,
        author: "contributor".into(),
        installation: 1,
        head_sha: Some("sha".into()),
        delivery: Some(id.to_owned()),
        event: "pull_request".into(),
        mode: ReviewJobMode::Incremental,
        accepted_at_ms,
    }
}

/// Simulate a prior status worker acknowledgement for reducer-only ownership
/// tests. This does not exercise Forge publication or its retry path.
async fn seed_published_queue_checks(store: &MockReviewJobStore, now_ms: i64) {
    static NEXT_FIXTURE_CHECK_ID: std::sync::atomic::AtomicU64 =
        std::sync::atomic::AtomicU64::new(120_000);
    let pending = store
        .claim_pending_checks(now_ms, super::MAX_REVIEW_STATUS_RECORDS)
        .await
        .expect("fixture status claims succeed");
    for check in pending {
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
                    NEXT_FIXTURE_CHECK_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
                    now_ms,
                )
                .await
                .unwrap()
        );
    }
}

async fn claim_after_queue_ack(
    store: &MockReviewJobStore,
    owner: &str,
    now_ms: i64,
) -> crate::error::Result<Option<super::OwnedReview>> {
    seed_published_queue_checks(store, now_ms).await;
    store.claim_next(owner, now_ms).await
}

#[test]
fn legacy_scheduled_reviews_deserialize_with_empty_status_identity() {
    let mut state = ReviewSchedulerState::default();
    state.admit_batch(vec![request("legacy", 42)], 42);
    let mut document = serde_json::to_value(&state).unwrap();
    let scheduled = document["unfinished"][0]
        .as_object_mut()
        .expect("scheduled review object");
    scheduled.remove("enqueued_at_ms");
    scheduled.remove("review_checks");

    let recovered: ReviewSchedulerState = serde_json::from_value(document).unwrap();
    let scheduled = &recovered.unfinished[0];
    assert_eq!(scheduled.enqueued_at_ms, 0);
    assert!(scheduled.review_checks.is_empty());
}

#[test]
fn coalesced_deliveries_keep_the_original_durable_queue_order() {
    let mut state = ReviewSchedulerState::default();
    assert_eq!(
        state.admit_batch(vec![request("first", 42)], 42),
        Admission::Admitted
    );
    assert_eq!(
        state.admit_batch(vec![request("newer-head", 99)], 99),
        Admission::Admitted
    );

    assert_eq!(state.unfinished.len(), 1);
    assert_eq!(state.unfinished[0].enqueued_at_ms, 42);
    assert_eq!(state.unfinished[0].request.accepted_at_ms, 99);
}

#[test]
fn queued_check_identity_keeps_head_and_stage_separate_from_paid_head_state() {
    let record = super::ReviewCheckRecord {
        head_sha: "queued-head".into(),
        external_id: "review:job:queued-head".into(),
        check_id: Some(17),
        stage: super::ReviewCheckStage::Queued,
        owner_generation: None,
        published: false,
        post_attempted: false,
        write_claim_until_ms: 0,
        write_generation: 0,
    };
    let encoded = serde_json::to_value(&record).unwrap();
    let recovered: super::ReviewCheckRecord = serde_json::from_value(encoded).unwrap();

    assert_eq!(recovered.head_sha, "queued-head");
    assert_eq!(recovered.check_id, Some(17));
    assert_eq!(recovered.stage, super::ReviewCheckStage::Queued);
    assert_eq!(recovered.owner_generation, None);
    assert!(
        !format!("{recovered:?}").contains("paid_attempts"),
        "check identity remains separate from paid-head authority"
    );
}

#[test]
fn queued_status_claims_cannot_publish_or_adopt_a_superseded_head() {
    let mut state = ReviewSchedulerState::default();
    assert_eq!(
        state.admit_batch(vec![request("push-a", 10)], 10),
        Admission::Admitted
    );
    let old_claim = state.claim_pending_checks(10, 1).pop().unwrap();

    let mut newer = request("push-b", 20);
    newer.head_sha = Some("new-head".into());
    assert_eq!(state.admit_batch(vec![newer], 20), Admission::Admitted);
    assert!(!state.queued_check_claim_current(&old_claim, 20));
    assert!(!state.record_queued_check_id(&old_claim, 91, 20));

    let replacement = state
        .claim_pending_checks(20 + super::QUEUE_STATUS_CLAIM_TTL_MS, 1)
        .pop()
        .unwrap();
    assert_eq!(replacement.check.head_sha, "new-head");
    assert_ne!(replacement.check.external_id, old_claim.check.external_id);
}

#[test]
fn unowned_coalescing_detaches_the_old_queued_check_until_terminal_ack() {
    let mut state = ReviewSchedulerState::default();
    let mut first = request("status-head-a", 10);
    first.head_sha = Some("head-a".into());
    assert_eq!(state.admit_batch(vec![first], 10), Admission::Admitted);
    let old = state.claim_pending_checks(10, 1).pop().unwrap();
    assert!(state.record_queued_check_id(&old, 41, 11));

    let mut next = request("status-head-b", 12);
    next.head_sha = Some("head-b".into());
    assert_eq!(state.admit_batch(vec![next], 12), Admission::Admitted);
    assert_eq!(state.retired_checks.len(), 1);
    assert_eq!(state.retired_checks[0].check.check_id, Some(41));
    assert_eq!(
        state.unfinished[0].request.head_sha.as_deref(),
        Some("head-b")
    );

    let claims = state.claim_pending_checks(12, 2);
    let active = claims.iter().find(|claim| !claim.retired).unwrap();
    let retired = claims.iter().find(|claim| claim.retired).unwrap();
    assert_eq!(active.check.head_sha, "head-b");
    assert_eq!(retired.check.head_sha, "head-a");
    assert!(state.record_queued_check_id(active, 42, 13));
    assert!(state.complete_retired_check(retired, 13));
    assert!(state.retired_checks.is_empty());
}

#[test]
fn ambiguous_retired_post_survives_restart_and_empty_recovery_reads() {
    let mut state = ReviewSchedulerState::default();
    let mut first = request("ambiguous-status-a", 10);
    first.head_sha = Some("head-a".into());
    state.admit_batch(vec![first], 10);
    let old = state.claim_pending_checks(10, 1).pop().unwrap();
    assert!(state.mark_queue_post_attempted(&old, 10));
    let mut next = request("ambiguous-status-b", 12);
    next.head_sha = Some("head-b".into());
    state.admit_batch(vec![next], 12);
    let retired = state
        .claim_pending_checks(12, 2)
        .into_iter()
        .find(|claim| claim.retired)
        .unwrap();
    assert!(retired.check.check_id.is_none());
    // The POST for A may have succeeded, but an initially empty external_id
    // recovery read is not evidence that it did not.
    assert_eq!(state.retired_checks.len(), 1);

    let bytes = serde_json::to_vec(&state).unwrap();
    let mut recovered: ReviewSchedulerState = serde_json::from_slice(&bytes).unwrap();
    let again = recovered
        .claim_pending_checks(12 + super::QUEUE_STATUS_CLAIM_TTL_MS + 1, 2)
        .into_iter()
        .find(|claim| claim.retired)
        .unwrap();
    assert_eq!(again.check.external_id, old.check.external_id);
    assert!(recovered.record_queued_check_id(&again, 81, 13 + super::QUEUE_STATUS_CLAIM_TTL_MS));
    assert!(recovered.complete_retired_check(&again, 14 + super::QUEUE_STATUS_CLAIM_TTL_MS));
    assert!(recovered.retired_checks.is_empty());
}

#[test]
#[cfg(feature = "serve")]
fn retired_status_outbox_is_bounded_and_fits_under_the_bson_document_cap() {
    let check = super::ReviewCheckRecord {
        head_sha: "h".repeat(super::REVIEW_JOB_FIELD_LIMIT),
        external_id: "e".repeat(super::REVIEW_JOB_FIELD_LIMIT),
        check_id: Some(1),
        stage: super::ReviewCheckStage::Queued,
        owner_generation: None,
        published: true,
        post_attempted: true,
        write_claim_until_ms: 0,
        write_generation: 1,
    };
    let mut state = ReviewSchedulerState {
        retired_checks: (0..super::MAX_REVIEW_STATUS_RECORDS)
            .map(|_| super::RetiredReviewCheck {
                repo: "r".repeat(super::REVIEW_JOB_FIELD_LIMIT),
                installation: 1,
                check: check.clone(),
            })
            .collect(),
        ..ReviewSchedulerState::default()
    };
    let encoded = bson::serialize_to_vec(&state).expect("bounded scheduler serializes");
    assert!(encoded.len() < 2 * 1024 * 1024, "{} bytes", encoded.len());
    let mut extra = request("overflow-status", 1);
    extra.head_sha = Some("new-head".into());
    assert_eq!(state.admit_batch(vec![extra], 1), Admission::Full);
}

#[test]
fn resolving_a_headless_status_does_not_consume_unreserved_status_capacity() {
    let check = super::ReviewCheckRecord {
        head_sha: "old-head".into(),
        external_id: "old-external-id".into(),
        check_id: Some(7),
        stage: super::ReviewCheckStage::Queued,
        owner_generation: None,
        published: true,
        post_attempted: true,
        write_claim_until_ms: 0,
        write_generation: 1,
    };
    let mut state = ReviewSchedulerState {
        retired_checks: (0..super::MAX_REVIEW_STATUS_RECORDS - 1)
            .map(|_| super::RetiredReviewCheck {
                repo: "owner/repo".into(),
                installation: 1,
                check: check.clone(),
            })
            .collect(),
        ..ReviewSchedulerState::default()
    };
    let mut headless = request("headless-capacity", 20);
    headless.head_sha = None;
    assert_eq!(
        state.admit_batch(vec![headless.clone()], 20),
        Admission::Full
    );
    state.retired_checks.pop();
    assert_eq!(state.admit_batch(vec![headless], 20), Admission::Admitted);
    assert_eq!(
        state.retired_checks.len()
            + state
                .unfinished
                .iter()
                .map(|job| job.review_checks.len())
                .sum::<usize>()
            + state
                .unfinished
                .iter()
                .filter(|job| job.status_reconcile_reserved)
                .count()
            + state
                .unfinished
                .iter()
                .filter(|job| job.metadata_resolution_pending)
                .count(),
        super::MAX_REVIEW_STATUS_RECORDS,
    );

    let pending = state.claim_pending_metadata(20, 1).pop().unwrap();
    assert!(state.resolve_pending_metadata(&pending, "live-head", true, false, 21));
    let status_records = state.retired_checks.len()
        + state
            .unfinished
            .iter()
            .map(|job| job.review_checks.len())
            .sum::<usize>()
        + state
            .unfinished
            .iter()
            .filter(|job| job.status_reconcile_reserved)
            .count()
        + state
            .unfinished
            .iter()
            .filter(|job| job.metadata_resolution_pending)
            .count();
    assert!(
        status_records <= super::MAX_REVIEW_STATUS_RECORDS,
        "metadata resolution must exchange its reserved slot for the check identity"
    );
}

#[test]
fn stale_headless_metadata_cannot_replace_a_newer_accepted_head() {
    let mut state = ReviewSchedulerState::default();
    let mut headless = request("headless-stale-a", 10);
    headless.head_sha = None;
    assert_eq!(state.admit_batch(vec![headless], 10), Admission::Admitted);
    let pending = state.claim_pending_metadata(11, 1).pop().unwrap();

    let mut newer = request("headless-stale-b", 12);
    newer.head_sha = Some("head-b".into());
    assert_eq!(state.admit_batch(vec![newer], 12), Admission::Admitted);
    assert!(!state.resolve_pending_metadata(&pending, "stale-head-a", true, false, 13));
    let job = &state.unfinished[0];
    assert_eq!(job.request.head_sha.as_deref(), Some("head-b"));
    assert!(
        job.review_checks
            .iter()
            .any(|check| check.head_sha == "head-b")
    );
    assert!(!job.metadata_resolution_pending);
}

#[tokio::test]
async fn published_review_is_not_retired_as_not_reviewed_when_status_updates_failed() {
    let store = MockReviewJobStore::default();
    assert_eq!(
        store
            .admit_batch(vec![request("published-with-status-outage", 10)])
            .await
            .unwrap(),
        Admission::Admitted
    );

    // Model both status-only forge writes failing: the in-progress PATCH did
    // not advance the durable stage from Queued, and the terminal PATCH could
    // not acknowledge the published review either. Paid review work still ran
    // and reached Published, so cleanup must not replace its check with the
    // detached queue check's NotReviewed conclusion.
    let pending = store
        .claim_pending_checks(10, 1)
        .await
        .unwrap()
        .pop()
        .unwrap();
    assert!(
        store
            .record_queued_check_id(&pending, 77, 11)
            .await
            .unwrap()
    );
    let owned = claim_after_queue_ack(&store, "review-worker", 12)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        owned.review_check.as_ref().unwrap().stage,
        super::ReviewCheckStage::Queued
    );
    assert!(store.confirm_head(&owned, "sha", 13).await.unwrap());
    assert_eq!(
        store.begin_paid(&owned, 14).await.unwrap(),
        super::PaidStart::Started
    );
    assert!(
        store
            .finish(&owned, TerminalOutcome::Published, 20, 20)
            .await
            .unwrap()
    );

    assert!(
        store.snapshot_for_test().retired_checks.is_empty(),
        "the current published check must retain review truth after status API failures"
    );
}

#[test]
fn published_head_is_not_retired_as_not_reviewed_when_owner_yields() {
    let mut state = ReviewSchedulerState::default();
    let mut first = request("published-head-a", 10);
    first.head_sha = Some("head-a".into());
    assert_eq!(
        state.admit_batch(vec![first.clone()], 10),
        Admission::Admitted
    );
    let pending = state.claim_pending_checks(10, 1).pop().unwrap();
    assert!(state.record_queued_check_id(&pending, 78, 11));
    let owned = state.claim_next("review-worker", 12).unwrap();
    assert!(state.confirm_head(&owned, "head-a", 13));
    assert_eq!(state.begin_paid(&owned, 14), super::PaidStart::Started);

    let mut next = first;
    next.id = ReviewJobId("push-head-b".into());
    next.delivery = Some("push-head-b".into());
    next.head_sha = Some("head-b".into());
    assert_eq!(state.admit_batch(vec![next], 15), Admission::Admitted);
    assert!(!state.should_continue(&owned, 16));
    assert!(state.finish_with_yield(&owned, TerminalOutcome::Published, 17));

    assert!(
        state.retired_checks.is_empty(),
        "a published head's queued/in-progress status must not be overwritten as NotReviewed"
    );
    assert_eq!(
        state.unfinished.len(),
        1,
        "the newer accepted head remains resumable"
    );
}

#[test]
fn owned_advisory_head_does_not_retire_the_current_check_until_yield() {
    let mut state = ReviewSchedulerState::default();
    let mut first = request("owned-status-a", 10);
    first.head_sha = Some("head-a".into());
    state.admit_batch(vec![first], 10);
    let pending = state.claim_pending_checks(10, 1).pop().unwrap();
    assert!(state.record_queued_check_id(&pending, 51, 11));
    let owner = state.claim_next("owner-a", 12).unwrap();

    let mut next = request("owned-status-b", 13);
    next.head_sha = Some("head-b".into());
    state.admit_batch(vec![next], 13);
    assert!(
        state.retired_checks.is_empty(),
        "an advisory push cannot retire a live owner's check"
    );
    assert!(
        state.unfinished[0]
            .review_checks
            .iter()
            .any(|check| check.head_sha == "head-a")
    );

    assert!(state.defer_yield(&owner, 14));
    assert_eq!(state.retired_checks.len(), 1);
    assert_eq!(state.retired_checks[0].check.check_id, Some(51));
    assert_eq!(
        state.unfinished[0].request.head_sha.as_deref(),
        Some("head-b")
    );
}

#[test]
fn live_head_reconciliation_requeues_a_new_check_without_spending_paid_authority() {
    let mut state = ReviewSchedulerState::default();
    let mut first = request("live-reconcile", 10);
    first.head_sha = Some("hint-a".into());
    state.admit_batch(vec![first], 10);
    let queued = state.claim_pending_checks(10, 1).pop().unwrap();
    assert!(state.record_queued_check_id(&queued, 61, 11));
    let owner = state.claim_next("owner", 12).unwrap();

    let result = state.reconcile_live_check(&owner, "live-b", 13);
    let super::ReconciledReviewCheck::Pending(live_check) = result else {
        panic!("new live head must retain a queued status intent");
    };
    assert_eq!(live_check.head_sha, "live-b");
    assert_eq!(state.retired_checks[0].check.check_id, Some(61));
    assert_eq!(state.incremental_heads[0].head_sha, "live-b");
    assert_eq!(
        state.unfinished[0].request.head_sha.as_deref(),
        Some("live-b")
    );

    let pending = state.claim_pending_checks(13, 2);
    let active = pending.iter().find(|claim| !claim.retired).unwrap();
    assert_eq!(active.check.external_id, live_check.external_id);
    assert!(state.record_queued_check_id(active, 62, 14));
    assert!(state.defer(&owner, 15, 15));
    let resumed = state.claim_next("replacement", 16).unwrap();
    assert_eq!(resumed.request.head_sha.as_deref(), Some("live-b"));
    assert_eq!(resumed.review_check.unwrap().check_id, Some(62));
}

#[test]
fn live_head_reconciliation_defers_atomically_at_status_capacity() {
    let mut state = ReviewSchedulerState::default();
    let mut first = request("status-capacity", 10);
    first.head_sha = Some("head-a".into());
    state.admit_batch(vec![first], 10);
    let pending = state.claim_pending_checks(10, 1).pop().unwrap();
    assert!(state.record_queued_check_id(&pending, 71, 11));
    let base_check = state.unfinished[0].review_checks[0].clone();
    state.unfinished[0].status_reconcile_reserved = false;
    state.unfinished[0]
        .review_checks
        .extend((0..super::MAX_REVIEW_STATUS_RECORDS - 1).map(|index| {
            let mut check = base_check.clone();
            check.head_sha = format!("padding-{index}");
            check.external_id = format!("padding-id-{index}");
            check
        }));
    let owner = state.claim_next("owner", 12).unwrap();
    let before = state.incremental_heads.clone();
    assert_eq!(
        state.reconcile_live_check(&owner, "head-b", 13),
        super::ReconciledReviewCheck::CapacityDeferred,
    );
    assert_eq!(
        state.unfinished[0].request.head_sha.as_deref(),
        Some("head-a")
    );
    assert_eq!(
        state.incremental_heads, before,
        "capacity failure cannot change paid-head authority"
    );
}

#[test]
fn repeated_live_rebind_respects_headless_metadata_status_reservations() {
    let mut state = ReviewSchedulerState::default();
    let mut known = request("rebind-capacity", 10);
    known.head_sha = Some("head-a".into());
    let mut headless = request("headless-reserved-capacity", 11);
    headless.number = 8;
    headless.head_sha = None;
    assert_eq!(state.admit_batch(vec![known], 10), Admission::Admitted);
    assert_eq!(state.admit_batch(vec![headless], 10), Admission::Admitted);
    assert_eq!(
        state.unfinished.len(),
        2,
        "the headless PR owns an independent reservation"
    );
    assert!(state.unfinished[1].metadata_resolution_pending);

    let base = state.unfinished[0].review_checks[0].clone();
    state.retired_checks = (0..super::MAX_REVIEW_STATUS_RECORDS - 4)
        .map(|index| {
            let mut check = base.clone();
            check.head_sha = format!("padding-{index}");
            check.external_id = format!("padding-id-{index}");
            check.stage = super::ReviewCheckStage::Completed;
            check.published = true;
            check.write_claim_until_ms = i64::MAX;
            super::RetiredReviewCheck {
                repo: "owner/repo".into(),
                installation: 1,
                check,
            }
        })
        .collect();
    assert_eq!(
        state.retired_checks.len()
            + state
                .unfinished
                .iter()
                .map(|job| job.review_checks.len())
                .sum::<usize>()
            + state
                .unfinished
                .iter()
                .filter(|job| job.status_reconcile_reserved)
                .count()
            + state
                .unfinished
                .iter()
                .filter(|job| job.metadata_resolution_pending)
                .count(),
        super::MAX_REVIEW_STATUS_RECORDS,
    );

    let queued_a = state.claim_pending_checks(10, 1).pop().unwrap();
    assert_eq!(queued_a.check.head_sha, "head-a");
    assert!(state.record_queued_check_id(&queued_a, 90, 11));
    let owner = state.claim_next("owner", 12).unwrap();
    let super::ReconciledReviewCheck::Pending(check_b) =
        state.reconcile_live_check(&owner, "head-b", 13)
    else {
        panic!("first live rebind consumes its reserved slot");
    };
    let queued_b = state
        .claim_pending_checks(13, 2)
        .into_iter()
        .find(|claim| !claim.retired && claim.check.external_id == check_b.external_id)
        .unwrap();
    assert!(state.record_queued_check_id(&queued_b, 91, 14));

    assert_eq!(
        state.reconcile_live_check(&owner, "head-c", 15),
        super::ReconciledReviewCheck::CapacityDeferred,
        "a second rebind cannot spend the unresolved headless job's reserved status slot",
    );
    assert_eq!(
        state.unfinished[0].request.head_sha.as_deref(),
        Some("head-b")
    );
    assert!(
        state.unfinished[0]
            .review_checks
            .iter()
            .any(|check| check.head_sha == "head-b")
    );
    assert!(
        !state.unfinished[0]
            .review_checks
            .iter()
            .any(|check| check.head_sha == "head-c")
    );
}

#[test]
fn one_queued_check_id_survives_in_progress_completion_and_scheduler_restart() {
    let mut state = ReviewSchedulerState::default();
    assert_eq!(
        state.admit_batch(vec![request("same-check", 10)], 10),
        Admission::Admitted
    );
    let status_claim = state.claim_pending_checks(10, 1).pop().unwrap();
    assert!(state.record_queued_check_id(&status_claim, 77, 11));

    let owner = state.claim_next("worker-a", 12).unwrap();
    assert!(state.check_transition_current(&owner, "sha", 77, 13));
    assert_eq!(owner.review_check.as_ref().unwrap().check_id, Some(77));
    assert_eq!(
        owner.review_check.as_ref().unwrap().stage,
        super::ReviewCheckStage::Queued
    );
    assert!(state.set_check_stage(&owner, "sha", 77, super::ReviewCheckStage::InProgress, 13,));

    let persisted = serde_json::to_vec(&state).unwrap();
    let mut recovered: ReviewSchedulerState = serde_json::from_slice(&persisted).unwrap();
    let replacement = recovered
        .claim_next("worker-b", 12 + REVIEW_OWNER_TTL_MS + 1)
        .unwrap();
    let resumed_check = replacement.review_check.as_ref().unwrap();
    assert_eq!(resumed_check.check_id, Some(77));
    assert_eq!(resumed_check.stage, super::ReviewCheckStage::InProgress);
    assert!(recovered.set_check_stage(
        &replacement,
        "sha",
        77,
        super::ReviewCheckStage::Completed,
        13 + REVIEW_OWNER_TTL_MS + 1,
    ));
    let check = &recovered.unfinished[0].review_checks[0];
    assert_eq!(check.check_id, Some(77));
    assert_eq!(check.stage, super::ReviewCheckStage::Completed);
}

#[test]
fn expired_owner_cannot_start_a_queued_check_after_replacement_claims_it() {
    let mut state = ReviewSchedulerState::default();
    assert_eq!(
        state.admit_batch(vec![request("fenced-transition", 1)], 1),
        Admission::Admitted
    );
    let pending = state.claim_pending_checks(1, 1).pop().unwrap();
    assert!(state.record_queued_check_id(&pending, 700, 2));
    let old = state.claim_next("old", 3).unwrap();
    assert!(state.check_transition_current(&old, "sha", 700, 4));

    let replacement = state
        .claim_next("replacement", 3 + REVIEW_OWNER_TTL_MS + 1)
        .unwrap();
    assert!(!state.check_transition_current(&old, "sha", 700, 3 + REVIEW_OWNER_TTL_MS + 1,));
    assert!(state.check_transition_current(&replacement, "sha", 700, 3 + REVIEW_OWNER_TTL_MS + 1,));
}

#[tokio::test]
async fn incremental_pushes_for_one_pull_request_coalesce_before_claim() {
    let store = MockReviewJobStore::default();
    let mut first = request("push-a", 14);
    first.repo = "org/repo".into();
    first.number = 7;
    first.head_sha = Some("head-a".into());
    let mut second = first.clone();
    second.id = ReviewJobId("push-b".into());
    second.delivery = Some("push-b".into());
    second.head_sha = Some("head-b".into());

    assert_eq!(
        store.admit_batch(vec![first]).await.unwrap(),
        Admission::Admitted
    );
    assert_eq!(
        store.admit_batch(vec![second]).await.unwrap(),
        Admission::Admitted
    );

    assert_eq!(
        store.unfinished(),
        1,
        "one pending incremental review per PR"
    );
    let claimed = claim_after_queue_ack(&store, "worker", 20)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(claimed.request.head_sha.as_deref(), Some("head-b"));
    assert_eq!(
        claimed.accepted_ids.len(),
        2,
        "both ACKed delivery IDs remain reserved"
    );
}

#[tokio::test]
async fn a_burst_of_incremental_pushes_runs_only_the_last_queued_head() {
    let store = MockReviewJobStore::default();
    let mut requests = (0..3)
        .map(|index| {
            let mut item = request(&format!("push-{index}"), 14 + index);
            item.repo = "org/repo".into();
            item.number = 7;
            item.head_sha = Some(format!("head-{index}"));
            item
        })
        .collect::<Vec<_>>();
    assert_eq!(
        store.admit_batch(vec![requests.remove(0)]).await.unwrap(),
        Admission::Admitted
    );
    assert_eq!(
        store.admit_batch(vec![requests.remove(0)]).await.unwrap(),
        Admission::Admitted
    );
    assert_eq!(
        store.admit_batch(vec![requests.remove(0)]).await.unwrap(),
        Admission::Admitted
    );

    let latest = claim_after_queue_ack(&store, "worker", 20)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(latest.request.head_sha.as_deref(), Some("head-2"));
    assert_eq!(latest.accepted_ids.len(), 3);
}

#[tokio::test]
async fn an_out_of_order_older_head_hint_yields_without_stealing_the_owner() {
    let store = MockReviewJobStore::default();
    let mut newer = request("push-b", 14);
    newer.repo = "org/repo".into();
    newer.number = 7;
    newer.head_sha = Some("head-b".into());
    store.admit_batch(vec![newer.clone()]).await.unwrap();
    let active = claim_after_queue_ack(&store, "worker", 20)
        .await
        .unwrap()
        .unwrap();

    let mut delayed = newer;
    delayed.id = ReviewJobId("push-a-delayed".into());
    delayed.delivery = Some("push-a-delayed".into());
    delayed.head_sha = Some("head-a".into());
    store.admit_batch(vec![delayed]).await.unwrap();

    assert!(
        store.owns(&active, 21).await.unwrap(),
        "a hint cannot steal durable ownership"
    );
    assert!(!store.should_continue(&active, 21).await.unwrap());
    assert_eq!(store.unfinished(), 1);
    assert!(store.defer_yield(&active, 22).await.unwrap());
    let resumed = claim_after_queue_ack(&store, "worker-2", 23)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(resumed.request.head_sha.as_deref(), Some("head-a"));
    assert_eq!(resumed.accepted_ids.len(), 2);
}

#[tokio::test]
async fn delayed_old_hint_yields_but_confirming_the_live_head_keeps_its_paid_budget() {
    let store = MockReviewJobStore::default();
    let mut newer = request("push-b", 14);
    newer.repo = "org/repo".into();
    newer.number = 7;
    newer.head_sha = Some("head-b".into());
    store.admit_batch(vec![newer.clone()]).await.unwrap();
    let active = claim_after_queue_ack(&store, "worker", 20)
        .await
        .unwrap()
        .unwrap();
    assert!(store.confirm_head(&active, "head-b", 21).await.unwrap());
    assert_eq!(
        store.begin_paid(&active, 22).await.unwrap(),
        super::PaidStart::Started
    );

    let mut delayed = newer;
    delayed.id = ReviewJobId("push-a-delayed".into());
    delayed.delivery = Some("push-a-delayed".into());
    delayed.head_sha = Some("head-a".into());
    store.admit_batch(vec![delayed]).await.unwrap();
    assert!(!store.should_continue(&active, 23).await.unwrap());
    assert!(store.defer_yield(&active, 24).await.unwrap());

    let resumed = claim_after_queue_ack(&store, "worker-2", 25)
        .await
        .unwrap()
        .unwrap();
    assert!(store.confirm_head(&resumed, "head-b", 26).await.unwrap());
    assert_eq!(
        store.begin_paid(&resumed, 27).await.unwrap(),
        super::PaidStart::Started
    );
    assert_eq!(
        store.snapshot_for_test().incremental_heads[0].paid_attempts,
        2,
        "a stale webhook hint cannot refill the actual head's budget"
    );
}

#[tokio::test]
async fn full_manual_review_is_not_yielded_by_incremental_pushes() {
    let store = MockReviewJobStore::default();
    let mut full = request("manual-full", 14);
    full.repo = "org/repo".into();
    full.number = 7;
    full.mode = ReviewJobMode::Full;
    full.head_sha = Some("head-a".into());
    store.admit_batch(vec![full.clone()]).await.unwrap();
    let active = claim_after_queue_ack(&store, "worker", 20)
        .await
        .unwrap()
        .unwrap();

    let mut incremental = full;
    incremental.id = ReviewJobId("push-b".into());
    incremental.delivery = Some("push-b".into());
    incremental.mode = ReviewJobMode::Incremental;
    incremental.head_sha = Some("head-b".into());
    store.admit_batch(vec![incremental]).await.unwrap();

    assert!(store.should_continue(&active, 21).await.unwrap());
    assert_eq!(store.unfinished(), 2);
}

#[tokio::test]
async fn a_deadline_is_terminal_for_the_confirmed_head_across_delivery_ids() {
    let store = MockReviewJobStore::default();
    let mut first = request("push-a", 1);
    first.repo = "org/repo".into();
    first.number = 7;
    first.head_sha = Some("head-a".into());
    store.admit_batch(vec![first.clone()]).await.unwrap();
    let timed_out = claim_after_queue_ack(&store, "worker", 2)
        .await
        .unwrap()
        .unwrap();
    assert!(store.confirm_head(&timed_out, "head-a", 3).await.unwrap());
    assert_eq!(
        store.begin_paid(&timed_out, 4).await.unwrap(),
        super::PaidStart::Started
    );
    assert!(store.finish_deadline(&timed_out, 5).await.unwrap());

    first.id = ReviewJobId("push-a-redelivery".into());
    first.delivery = Some("push-a-redelivery".into());
    first.accepted_at_ms = 6;
    store.admit_batch(vec![first.clone()]).await.unwrap();
    let retry = claim_after_queue_ack(&store, "worker-2", 7)
        .await
        .unwrap()
        .unwrap();
    assert!(store.confirm_head(&retry, "head-a", 8).await.unwrap());
    assert_eq!(
        store.begin_paid(&retry, 9).await.unwrap(),
        super::PaidStart::Exhausted
    );
    assert!(
        store
            .finish(&retry, TerminalOutcome::Exhausted, 9, 9)
            .await
            .unwrap()
    );

    first.id = ReviewJobId("push-b".into());
    first.delivery = Some("push-b".into());
    first.head_sha = Some("head-b".into());
    first.accepted_at_ms = 10;
    store.admit_batch(vec![first]).await.unwrap();
    let new_head = claim_after_queue_ack(&store, "worker-3", 11)
        .await
        .unwrap()
        .unwrap();
    assert!(store.confirm_head(&new_head, "head-b", 12).await.unwrap());
    assert_eq!(
        store.begin_paid(&new_head, 13).await.unwrap(),
        super::PaidStart::Started
    );
}

#[tokio::test]
async fn a_push_racing_the_deadline_preserves_the_marker_until_live_head_resolution() {
    let store = MockReviewJobStore::default();
    let mut first = request("push-a", 1);
    first.repo = "org/repo".into();
    first.number = 7;
    first.head_sha = Some("head-a".into());
    store.admit_batch(vec![first.clone()]).await.unwrap();
    let timed_out = claim_after_queue_ack(&store, "worker", 2)
        .await
        .unwrap()
        .unwrap();
    assert!(store.confirm_head(&timed_out, "head-a", 3).await.unwrap());

    first.id = ReviewJobId("push-b".into());
    first.delivery = Some("push-b".into());
    first.head_sha = Some("head-b-hint".into());
    first.accepted_at_ms = 4;
    store.admit_batch(vec![first]).await.unwrap();

    assert!(store.finish_deadline(&timed_out, 5).await.unwrap());
    assert_eq!(
        store.unfinished(),
        1,
        "both delivery IDs stay attached to recoverable work"
    );
    let resumed = claim_after_queue_ack(&store, "worker-2", 6)
        .await
        .unwrap()
        .unwrap();
    assert!(store.confirm_head(&resumed, "head-a", 7).await.unwrap());
    assert!(store.head_is_terminal(&resumed, 8).await.unwrap());
    assert_eq!(
        store.begin_paid(&resumed, 9).await.unwrap(),
        super::PaidStart::Exhausted
    );

    // The webhook hint is not proof of advancement. Only resolving a different
    // Forge head clears the prior head's marker and makes new paid work eligible.
    assert!(store.confirm_head(&resumed, "head-b", 10).await.unwrap());
    assert!(!store.head_is_terminal(&resumed, 11).await.unwrap());
    assert_eq!(
        store.begin_paid(&resumed, 12).await.unwrap(),
        super::PaidStart::Started
    );
}

#[tokio::test]
async fn paid_starts_require_a_head_confirmed_from_the_live_forge_read() {
    let store = MockReviewJobStore::default();
    store.admit_batch(vec![request("push-a", 1)]).await.unwrap();
    let owned = claim_after_queue_ack(&store, "worker", 2)
        .await
        .unwrap()
        .unwrap();

    assert_eq!(
        store.begin_paid(&owned, 3).await.unwrap(),
        super::PaidStart::OwnershipLost,
        "the request SHA is an untrusted hint until the worker confirms the live head"
    );
    assert!(store.confirm_head(&owned, "actual-head", 4).await.unwrap());
    assert_eq!(
        store.begin_paid(&owned, 5).await.unwrap(),
        super::PaidStart::Started
    );
}

#[test]
fn legacy_paid_attempts_remain_consumed_when_the_live_head_is_first_confirmed() {
    let mut state: ReviewSchedulerState = serde_json::from_value(serde_json::json!({
        "revision": 0,
        "unfinished": [{
            "request": {
                "id": "legacy-delivery",
                "repo": "org/repo",
                "number": 7,
                "author": "contributor",
                "installation": 1,
                "head_sha": "untrusted-old-hint",
                "delivery": "legacy-delivery",
                "event": "pull_request",
                "mode": "incremental",
                "accepted_at_ms": 1
            },
            "owner": null,
            "generation": 0,
            "owner_until_ms": 0,
            "not_before_ms": 0,
            "paid_attempts": 5
        }],
        "terminal": []
    }))
    .expect("pre-supersession scheduler records deserialize with defaults");
    state.normalize_legacy_records();
    let owned = state.claim_next("recovered-worker", 10).unwrap();
    assert!(state.confirm_head(&owned, "actual-live-head", 11));
    assert_eq!(
        state.begin_paid(&owned, 12),
        super::PaidStart::Exhausted,
        "the first authoritative SHA cannot grant five extra starts to a legacy job"
    );
    assert_eq!(state.incremental_heads[0].paid_attempts, 5);
}

#[tokio::test]
async fn head_history_transitions_consume_reserved_capacity_without_overflow() {
    let store = MockReviewJobStore::default();
    let mut first = request("push-a", 1);
    first.head_sha = Some("head-a".into());
    store.admit_batch(vec![first.clone()]).await.unwrap();
    let owner = claim_after_queue_ack(&store, "worker-a", 2)
        .await
        .unwrap()
        .unwrap();
    assert!(store.confirm_head(&owner, "head-a", 3).await.unwrap());
    assert!(
        store
            .finish(&owner, TerminalOutcome::Published, 4, 4)
            .await
            .unwrap()
    );

    first.id = ReviewJobId("push-b".into());
    first.delivery = Some("push-b".into());
    first.head_sha = Some("head-b".into());
    first.accepted_at_ms = 5;
    store.admit_batch(vec![first.clone()]).await.unwrap();
    let state = store.snapshot_for_test();
    let padding = MAX_TERMINAL_REVIEW_RESERVATIONS - state.reserved_capacity();
    let mut state = state;
    state
        .terminal
        .extend((0..padding).map(|index| ReviewTombstone {
            id: ReviewJobId(format!("padding-{index}")),
            outcome: TerminalOutcome::Exhausted,
            expires_at_ms: REVIEW_TOMBSTONE_TTL_MS * 2,
        }));
    assert_eq!(state.reserved_capacity(), MAX_TERMINAL_REVIEW_RESERVATIONS);
    store.replace_state_for_test(state);

    first.id = ReviewJobId("push-c".into());
    first.delivery = Some("push-c".into());
    assert_eq!(
        store.admit_batch(vec![first]).await.unwrap(),
        Admission::Full
    );

    let owner = claim_after_queue_ack(&store, "worker-b", 6)
        .await
        .unwrap()
        .unwrap();
    assert!(store.confirm_head(&owner, "head-b", 7).await.unwrap());
    assert_eq!(
        store.snapshot_for_test().reserved_capacity(),
        MAX_TERMINAL_REVIEW_RESERVATIONS,
        "the appended terminal-head record consumes its prior reservation"
    );
}

#[tokio::test]
async fn accepted_jobs_recover_after_wakeup_loss_and_completed_jobs_stay_done() {
    let store = MockReviewJobStore::default();
    assert_eq!(
        store
            .admit_batch(vec![request("delivery-a", 10)])
            .await
            .unwrap(),
        Admission::Admitted
    );
    // A fresh worker has no channel state; it claims directly from the durable
    // scheduler record, the same path used by startup recovery and polling.
    let recovered = claim_after_queue_ack(&store, "process-a", 11)
        .await
        .unwrap()
        .expect("accepted work survives a lost wakeup");
    assert!(
        store
            .finish(&recovered, TerminalOutcome::Published, 12, 12)
            .await
            .unwrap()
    );
    assert!(
        claim_after_queue_ack(&store, "process-b", 13)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        store
            .admit_batch(vec![request("delivery-a", 14)])
            .await
            .unwrap(),
        Admission::Duplicate
    );
}

#[tokio::test]
async fn an_unpublished_queued_check_blocks_review_claims() {
    let store = MockReviewJobStore::default();
    assert_eq!(
        store
            .admit_batch(vec![request("queued-check-gate", 10)])
            .await
            .unwrap(),
        Admission::Admitted
    );
    assert!(store.claim_next("before-ack", 11).await.unwrap().is_none());

    seed_published_queue_checks(&store, 12).await;
    assert!(store.claim_next("after-ack", 13).await.unwrap().is_some());
}

#[tokio::test]
async fn whole_manual_batch_is_rejected_without_partial_admission() {
    let store = MockReviewJobStore::default();
    let full = (0..MAX_UNFINISHED_REVIEWS)
        .map(|index| {
            let mut manual = request(&format!("job-{index}"), 1);
            manual.mode = ReviewJobMode::Full;
            manual
        })
        .collect::<Vec<_>>();
    assert_eq!(store.admit_batch(full).await.unwrap(), Admission::Admitted);
    assert_eq!(
        store
            .admit_batch(vec![
                {
                    let mut manual = request("manual-a", 2);
                    manual.mode = ReviewJobMode::Full;
                    manual
                },
                {
                    let mut manual = request("manual-b", 2);
                    manual.mode = ReviewJobMode::Full;
                    manual
                },
            ])
            .await
            .unwrap(),
        Admission::Full
    );
    assert_eq!(store.unfinished(), MAX_UNFINISHED_REVIEWS);
}

#[tokio::test]
async fn expired_owner_is_recovered_but_live_owner_and_stale_generation_are_fenced() {
    let store = MockReviewJobStore::default();
    store
        .admit_batch(vec![request("delivery-a", 1)])
        .await
        .unwrap();
    let first = claim_after_queue_ack(&store, "process-a", 2)
        .await
        .unwrap()
        .unwrap();
    assert!(
        claim_after_queue_ack(&store, "process-b", 3)
            .await
            .unwrap()
            .is_none()
    );
    let second = claim_after_queue_ack(&store, "process-b", 2 + REVIEW_OWNER_TTL_MS)
        .await
        .unwrap()
        .unwrap();
    assert!(second.generation > first.generation);
    assert!(
        !store
            .finish(&first, TerminalOutcome::Published, 40_000, 40_000)
            .await
            .unwrap()
    );
    assert!(
        store
            .defer(&second, 40_001, 50_000)
            .await
            .expect("the current generation may defer")
    );
    assert_eq!(store.unfinished(), 1);
}

#[tokio::test]
async fn live_lease_contention_defers_without_converting_it_to_a_success() {
    let store = MockReviewJobStore::default();
    store
        .admit_batch(vec![request("delivery-a", 1)])
        .await
        .unwrap();
    let claimed = claim_after_queue_ack(&store, "process-a", 2)
        .await
        .unwrap()
        .unwrap();
    assert!(store.defer(&claimed, 3, 10_000).await.unwrap());
    assert!(
        claim_after_queue_ack(&store, "process-b", 9_999)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        claim_after_queue_ack(&store, "process-b", 10_000)
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn running_claims_are_bounded_across_process_owners() {
    let store = MockReviewJobStore::default();
    let jobs = (0..=MAX_RUNNING_REVIEWS)
        .map(|index| {
            let mut item = request(&format!("delivery-{index}"), 1);
            item.number = 7 + index as u64;
            item
        })
        .collect();
    assert_eq!(store.admit_batch(jobs).await.unwrap(), Admission::Admitted);
    for index in 0..MAX_RUNNING_REVIEWS {
        assert!(
            claim_after_queue_ack(&store, &format!("process-{index}"), 2)
                .await
                .unwrap()
                .is_some()
        );
    }
    assert!(
        claim_after_queue_ack(&store, "process-overflow", 3)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn only_a_live_generation_can_renew_or_finish() {
    let store = MockReviewJobStore::default();
    store
        .admit_batch(vec![request("delivery-a", 1)])
        .await
        .unwrap();
    let first = claim_after_queue_ack(&store, "process-a", 2)
        .await
        .unwrap()
        .unwrap();
    assert!(store.renew(&first, 10_000).await.unwrap());
    assert!(store.owns(&first, 10_001).await.unwrap());
    assert!(
        !store
            .owns(&first, 10_000 + REVIEW_OWNER_TTL_MS)
            .await
            .unwrap()
    );
    assert!(
        !store
            .renew(&first, 10_000 + REVIEW_OWNER_TTL_MS)
            .await
            .unwrap()
    );
    let recovered = claim_after_queue_ack(&store, "process-b", 10_000 + REVIEW_OWNER_TTL_MS)
        .await
        .unwrap()
        .unwrap();
    assert!(
        !store
            .finish(
                &first,
                TerminalOutcome::Published,
                10_000 + REVIEW_OWNER_TTL_MS,
                50_000,
            )
            .await
            .unwrap()
    );
    assert!(
        store
            .owns(&recovered, 10_001 + REVIEW_OWNER_TTL_MS)
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn terminal_deadlines_remain_deduped_for_seven_days() {
    let store = MockReviewJobStore::default();
    store
        .admit_batch(vec![request("delivery-a", 1)])
        .await
        .unwrap();
    let claimed = claim_after_queue_ack(&store, "process-a", 2)
        .await
        .unwrap()
        .unwrap();
    store
        .finish(&claimed, TerminalOutcome::Exhausted, 3, 3)
        .await
        .unwrap();
    assert_eq!(
        store
            .admit_batch(vec![request("delivery-a", 4)])
            .await
            .unwrap(),
        Admission::Duplicate
    );
    assert_eq!(
        store
            .admit_batch(vec![request("delivery-a", 3 + REVIEW_TOMBSTONE_TTL_MS + 1)])
            .await
            .unwrap(),
        Admission::Admitted
    );
}

#[tokio::test]
async fn failed_batch_persistence_admits_no_jobs() {
    let store = MockReviewJobStore::default();
    store.fail_next_admission();
    assert!(
        store
            .admit_batch(vec![request("manual-a", 1), request("manual-b", 1)])
            .await
            .is_err()
    );
    assert_eq!(store.unfinished(), 0);
    assert_eq!(
        store
            .admit_batch(vec![request("manual-a", 2), request("manual-b", 2)])
            .await
            .unwrap(),
        Admission::Admitted
    );
}

#[tokio::test]
async fn tombstone_reservations_backpressure_without_evicting_live_dedupe_ids() {
    let store = MockReviewJobStore::default();
    let state = ReviewSchedulerState {
        terminal: (0..MAX_TERMINAL_REVIEW_RESERVATIONS)
            .map(|index| ReviewTombstone {
                id: ReviewJobId(format!("done-{index:05}-{}", "x".repeat(260))),
                outcome: TerminalOutcome::Exhausted,
                expires_at_ms: REVIEW_TOMBSTONE_TTL_MS * 2,
            })
            .collect(),
        ..ReviewSchedulerState::default()
    };
    store.replace_state_for_test(state);
    assert_eq!(
        store
            .admit_batch(vec![request("new-work", 10)])
            .await
            .unwrap(),
        Admission::Full
    );
    assert_eq!(store.unfinished(), 0);
    let state = store.snapshot_for_test();
    assert_eq!(state.terminal.len(), MAX_TERMINAL_REVIEW_RESERVATIONS);
    assert!(state.unfinished.is_empty());
}
