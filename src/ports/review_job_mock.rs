//! Always-compiled offline implementation of the durable review-job port.
//!
//! The mock uses the same scheduler state transitions as production while
//! replacing MongoDB persistence with an in-memory mutex. It never performs
//! network I/O and is available to downstream crates as well as unit tests.

use std::sync::Mutex;

use crate::error::{Error, Result};
use crate::ports::review_job::{
    Admission, MAX_TERMINAL_REVIEW_RESERVATIONS, MAX_UNFINISHED_REVIEWS, OwnedReview, PaidStart,
    ReviewJobStore, ReviewRequest, ReviewSchedulerState, TerminalOutcome,
};

/// In-memory implementation of [`ReviewJobStore`] for offline use and tests.
#[derive(Default)]
pub struct MockReviewJobStore {
    state: Mutex<ReviewSchedulerState>,
    fail_admission: Mutex<bool>,
    #[cfg(test)]
    should_continue_delay: Mutex<Option<std::time::Duration>>,
}

impl MockReviewJobStore {
    /// Make the next whole-batch admission fail with a simulated store error.
    pub fn fail_next_admission(&self) {
        *self.fail_admission.lock().expect("admission failure") = true;
    }

    /// Delay the control-plane check to exercise a local lease expiring in flight.
    #[cfg(all(test, feature = "serve"))]
    pub(crate) fn delay_should_continue(&self, delay: std::time::Duration) {
        *self
            .should_continue_delay
            .lock()
            .expect("control-plane delay") = Some(delay);
    }

    /// Return the number of pending or currently owned requests.
    pub fn unfinished(&self) -> usize {
        self.state.lock().expect("state").unfinished.len()
    }

    /// Replace scheduler state in a contract-test fixture.
    #[cfg(test)]
    pub(crate) fn replace_state_for_test(&self, state: ReviewSchedulerState) {
        *self.state.lock().expect("state") = state;
    }

    /// Snapshot scheduler state for a contract-test assertion.
    #[cfg(test)]
    pub(crate) fn snapshot_for_test(&self) -> ReviewSchedulerState {
        self.state.lock().expect("state").clone()
    }
}

#[async_trait::async_trait]
impl ReviewJobStore for MockReviewJobStore {
    async fn has_capacity_for(&self, count: usize) -> Result<bool> {
        let state = self.state.lock().expect("state");
        Ok(count > 0
            && state.unfinished.len() + count <= MAX_UNFINISHED_REVIEWS
            && state.reserved_capacity() + count <= MAX_TERMINAL_REVIEW_RESERVATIONS)
    }

    async fn admit_batch(&self, requests: Vec<ReviewRequest>) -> Result<Admission> {
        if std::mem::take(&mut *self.fail_admission.lock().expect("admission failure")) {
            return Err(Error::Forge("simulated scheduler write failure".into()));
        }
        let now_ms = requests
            .iter()
            .map(|request| request.accepted_at_ms)
            .max()
            .unwrap_or_default();
        Ok(self
            .state
            .lock()
            .expect("state")
            .admit_batch(requests, now_ms))
    }

    async fn claim_pending_checks(
        &self,
        now_ms: i64,
        limit: usize,
    ) -> Result<Vec<crate::ports::review_job::PendingReviewCheck>> {
        Ok(self
            .state
            .lock()
            .expect("state")
            .claim_pending_checks(now_ms, limit))
    }

    async fn claim_pending_metadata(
        &self,
        now_ms: i64,
        limit: usize,
    ) -> Result<Vec<crate::ports::review_job::PendingReviewMetadata>> {
        Ok(self
            .state
            .lock()
            .expect("state")
            .claim_pending_metadata(now_ms, limit))
    }

    async fn metadata_claim_current(
        &self,
        pending: &crate::ports::review_job::PendingReviewMetadata,
        now_ms: i64,
    ) -> Result<bool> {
        Ok(self
            .state
            .lock()
            .expect("state")
            .metadata_claim_current(pending, now_ms))
    }

    async fn resolve_pending_metadata(
        &self,
        pending: &crate::ports::review_job::PendingReviewMetadata,
        head_sha: &str,
        open: bool,
        draft: bool,
        now_ms: i64,
    ) -> Result<bool> {
        let pending = pending.clone();
        let head_sha = head_sha.to_owned();
        Ok(self
            .state
            .lock()
            .expect("state")
            .resolve_pending_metadata(&pending, &head_sha, open, draft, now_ms))
    }

    async fn record_queued_check_id(
        &self,
        pending: &crate::ports::review_job::PendingReviewCheck,
        check_id: u64,
        now_ms: i64,
    ) -> Result<bool> {
        Ok(self
            .state
            .lock()
            .expect("state")
            .record_queued_check_id(pending, check_id, now_ms))
    }

    async fn queued_check_claim_current(
        &self,
        pending: &crate::ports::review_job::PendingReviewCheck,
        now_ms: i64,
    ) -> Result<bool> {
        Ok(self
            .state
            .lock()
            .expect("state")
            .queued_check_claim_current(pending, now_ms))
    }

    async fn mark_queue_post_attempted(
        &self,
        pending: &crate::ports::review_job::PendingReviewCheck,
        now_ms: i64,
    ) -> Result<bool> {
        Ok(self
            .state
            .lock()
            .expect("state")
            .mark_queue_post_attempted(pending, now_ms))
    }

    async fn complete_retired_check(
        &self,
        pending: &crate::ports::review_job::PendingReviewCheck,
        now_ms: i64,
    ) -> Result<bool> {
        Ok(self
            .state
            .lock()
            .expect("state")
            .complete_retired_check(pending, now_ms))
    }

    async fn set_check_stage(
        &self,
        owned: &OwnedReview,
        head_sha: &str,
        check_id: u64,
        stage: crate::ports::review_job::ReviewCheckStage,
        now_ms: i64,
    ) -> Result<bool> {
        Ok(self
            .state
            .lock()
            .expect("state")
            .set_check_stage(owned, head_sha, check_id, stage, now_ms))
    }

    async fn check_transition_current(
        &self,
        owned: &OwnedReview,
        head_sha: &str,
        check_id: u64,
        now_ms: i64,
    ) -> Result<bool> {
        Ok(self
            .state
            .lock()
            .expect("state")
            .check_transition_current(owned, head_sha, check_id, now_ms))
    }

    async fn claim_next(&self, owner: &str, now_ms: i64) -> Result<Option<OwnedReview>> {
        Ok(self.state.lock().expect("state").claim_next(owner, now_ms))
    }

    async fn claim_next_with_limit(
        &self,
        owner: &str,
        now_ms: i64,
        limit: usize,
    ) -> Result<Option<OwnedReview>> {
        Ok(self
            .state
            .lock()
            .expect("state")
            .claim_next_with_limit(owner, now_ms, limit))
    }

    async fn begin_paid(&self, owned: &OwnedReview, now_ms: i64) -> Result<PaidStart> {
        Ok(self.state.lock().expect("state").begin_paid(owned, now_ms))
    }

    async fn confirm_head(&self, owned: &OwnedReview, head_sha: &str, now_ms: i64) -> Result<bool> {
        Ok(self
            .state
            .lock()
            .expect("state")
            .confirm_head(owned, head_sha, now_ms))
    }

    async fn reconcile_live_check(
        &self,
        owned: &OwnedReview,
        head_sha: &str,
        now_ms: i64,
    ) -> Result<crate::ports::review_job::ReconciledReviewCheck> {
        Ok(self
            .state
            .lock()
            .expect("state")
            .reconcile_live_check(owned, head_sha, now_ms))
    }

    async fn head_is_terminal(&self, owned: &OwnedReview, now_ms: i64) -> Result<bool> {
        Ok(self
            .state
            .lock()
            .expect("state")
            .head_is_terminal(owned, now_ms))
    }

    async fn renew(&self, owned: &OwnedReview, now_ms: i64) -> Result<bool> {
        Ok(self.state.lock().expect("state").renew(owned, now_ms))
    }

    async fn owns(&self, owned: &OwnedReview, now_ms: i64) -> Result<bool> {
        Ok(self.state.lock().expect("state").owns(owned, now_ms))
    }

    async fn should_continue(&self, owned: &OwnedReview, now_ms: i64) -> Result<bool> {
        #[cfg(test)]
        let delay = *self
            .should_continue_delay
            .lock()
            .expect("control-plane delay");
        #[cfg(test)]
        if let Some(delay) = delay {
            tokio::time::sleep(delay).await;
        }
        Ok(self
            .state
            .lock()
            .expect("state")
            .should_continue(owned, now_ms))
    }

    async fn defer(&self, owned: &OwnedReview, now_ms: i64, not_before_ms: i64) -> Result<bool> {
        Ok(self
            .state
            .lock()
            .expect("state")
            .defer(owned, now_ms, not_before_ms))
    }

    async fn defer_yield(&self, owned: &OwnedReview, now_ms: i64) -> Result<bool> {
        Ok(self.state.lock().expect("state").defer_yield(owned, now_ms))
    }

    async fn finish(
        &self,
        owned: &OwnedReview,
        outcome: TerminalOutcome,
        now_ms: i64,
        completed_at_ms: i64,
    ) -> Result<bool> {
        Ok(self
            .state
            .lock()
            .expect("state")
            .finish(owned, outcome, now_ms, completed_at_ms))
    }

    async fn finish_deadline(&self, owned: &OwnedReview, now_ms: i64) -> Result<bool> {
        Ok(self
            .state
            .lock()
            .expect("state")
            .finish_deadline(owned, now_ms))
    }
}
