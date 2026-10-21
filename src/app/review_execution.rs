//! Review-wide execution scope for physical model dispatches.
//!
//! Review-wide execution limits and model bindings shared by one review job.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
#[cfg(test)]
use std::time::Duration;

use tokio::sync::watch;
use tokio::time::Instant;

#[cfg(test)]
use crate::lanes::large_pr::ReviewMode;
use crate::ports::model::{Model, Usage};
use crate::ports::model_execution::{
    ModelExecutionControl, ModelExecutionPermit, ModelExecutionRefusal, ModelExecutionTerminal,
    ModelExecutionTerminalOutcome,
};

/// Absolute review window and the operator limits applied to it.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ReviewExecutionOptions {
    /// The already-established start of this review's hard window.
    pub started_at: Instant,
    /// The already-established server hard deadline.
    pub hard_deadline: Instant,
    /// Review policy mode retained for test diagnostics.
    #[cfg(test)]
    pub mode: ReviewMode,
    /// Physical call ceiling, absent for normal-mode scheduling.
    pub max_calls: Option<usize>,
    /// Percent of the original hard window available for new work.
    pub soft_deadline_percent: u8,
    /// Maximum finish interval for admitted work, consumed by bounded fan-out.
    pub drain_seconds: u64,
}

/// A snapshot of committed and currently reserved provider-call slots.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ReviewExecutionSnapshot {
    /// Calls committed immediately before their provider invocation.
    pub committed_calls: usize,
    /// Acquisitions that have not yet reached a provider invocation.
    pub reserved_calls: usize,
    /// Terminal physical usage received by this review scope.
    pub physical_usage: Usage,
    /// Terminal usage grouped by the model that actually answered when known.
    pub physical_usage_by_model: BTreeMap<String, Usage>,
    /// Number of provider terminal callbacks received by this scope.
    pub physical_terminal_count: u64,
    /// Raw physical terminal callbacks received by this execution scope.
    #[cfg(test)]
    pub physical_terminals: Vec<ModelExecutionTerminal>,
    /// Whether every committed call has a terminal with complete reported usage.
    pub physical_accounting_complete: bool,
}

/// One model binding shared across every attempt of a review job.
#[derive(Clone)]
pub(crate) struct ReviewModelBinding {
    /// The root-budgeted and execution-controlled model when those hooks exist.
    pub(crate) model: Arc<dyn Model>,
    /// Whether the model supplied a shared review-level monetary root.
    #[cfg(test)]
    pub(crate) review_budget_bound: bool,
    /// Whether physical dispatches are governed by this job's execution scope.
    pub(crate) execution_control_bound: bool,
}

/// Clock, physical control and model binding retained for one logical review.
///
/// The bound model may own a reference to `execution`; execution never owns the
/// context or model, so the binding cannot form an `Arc` cycle.
pub(crate) struct ReviewRunContext {
    execution: Arc<ReviewExecution>,
    model: OnceLock<ReviewModelBinding>,
    retrieval_usage: Mutex<Usage>,
    retrieval_models: Mutex<Vec<String>>,
    logical_usage: Mutex<Usage>,
    logical_models: Mutex<Vec<String>>,
}

/// One review shares this ceiling across physical turns, retries and turns
/// started by read-only agents.
const MAX_CONCURRENT_MODEL_DISPATCHES: usize = 8;

struct BoundedExecutionControl {
    inner: Arc<ReviewExecution>,
    permits: Arc<tokio::sync::Semaphore>,
}

struct BoundedExecutionPermit {
    inner: Box<dyn ModelExecutionPermit>,
    _permit: tokio::sync::OwnedSemaphorePermit,
}

/// Compatibility bound for adapters that do not expose physical dispatch
/// admission. One permit covers their whole `complete` or `review` operation.
struct BoundedLogicalModel {
    inner: Arc<dyn Model>,
    permits: Arc<tokio::sync::Semaphore>,
    execution: Arc<ReviewExecution>,
}

impl BoundedLogicalModel {
    fn new(inner: Arc<dyn Model>, execution: Arc<ReviewExecution>) -> Self {
        Self {
            inner,
            permits: Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_MODEL_DISPATCHES)),
            execution,
        }
    }

    fn with_inner(&self, inner: Arc<dyn Model>) -> Arc<dyn Model> {
        Arc::new(Self {
            inner,
            permits: self.permits.clone(),
            execution: self.execution.clone(),
        })
    }

    async fn acquire(
        &self,
    ) -> std::result::Result<tokio::sync::OwnedSemaphorePermit, ModelExecutionRefusal> {
        let permit = tokio::select! {
            biased;
            _ = self.execution.cancelled() => Err(ModelExecutionRefusal::Cancelled),
            _ = tokio::time::sleep_until(self.execution.soft_deadline()) => {
                Err(ModelExecutionRefusal::SoftDeadline)
            }
            permit = Arc::clone(&self.permits).acquire_owned() => {
                Ok(permit.expect("the review logical-call semaphore is never closed"))
            }
        }?;
        if let Some(refusal) = self.execution.refusal_for_new_work() {
            return Err(refusal);
        }
        Ok(permit)
    }
}

#[async_trait::async_trait]
impl Model for BoundedLogicalModel {
    async fn complete(
        &self,
        request: crate::ports::model::ModelRequest,
    ) -> crate::error::Result<crate::ports::model::ModelResponse> {
        let _permit = self.acquire().await.map_err(refusal_error)?;
        self.inner.complete(request).await
    }

    fn complete_reports_physical_calls(&self) -> bool {
        self.inner.complete_reports_physical_calls()
    }

    async fn review(
        &self,
        request: crate::ports::model::ModelRequest,
        tree: &dyn crate::ports::tree::TreeReader,
        policy: &crate::config::types::LookupPolicy,
    ) -> crate::error::Result<crate::ports::model::ModelResponse> {
        let _permit = self.acquire().await.map_err(refusal_error)?;
        self.inner.review(request, tree, policy).await
    }

    fn review_reports_physical_calls(&self) -> bool {
        self.inner.review_reports_physical_calls()
    }

    fn reports_raw_completion_usage(&self) -> bool {
        self.inner.reports_raw_completion_usage()
    }

    fn scoped_budget(&self, budget_usd: f64) -> Option<Arc<dyn Model>> {
        self.inner
            .scoped_budget(budget_usd)
            .map(|inner| self.with_inner(inner))
    }

    fn with_review_budget(&self, limit_usd: f64) -> Option<Arc<dyn Model>> {
        self.inner
            .with_review_budget(limit_usd)
            .map(|inner| self.with_inner(inner))
    }

    fn with_execution_control(
        &self,
        control: Arc<dyn ModelExecutionControl>,
    ) -> Option<Arc<dyn Model>> {
        // If a later binding supplies a physical hook, it replaces this
        // logical fallback rather than nesting both limiters around a call.
        self.inner.with_execution_control(control)
    }
}

#[async_trait::async_trait]
impl ModelExecutionControl for BoundedExecutionControl {
    async fn acquire(
        &self,
    ) -> std::result::Result<Box<dyn ModelExecutionPermit>, ModelExecutionRefusal> {
        // Wait for shared dispatch capacity before reserving the review's
        // finite call budget, so queued work does not occupy admission slots.
        let permit = tokio::select! {
            biased;
            _ = self.inner.cancelled() => return Err(ModelExecutionRefusal::Cancelled),
            _ = tokio::time::sleep_until(self.inner.soft_deadline()) => {
                return Err(ModelExecutionRefusal::SoftDeadline);
            }
            permit = Arc::clone(&self.permits).acquire_owned() => {
                permit.expect("the review dispatch semaphore is never closed")
            }
        };
        match self.inner.acquire().await {
            Ok(inner) => Ok(Box::new(BoundedExecutionPermit {
                inner,
                _permit: permit,
            })),
            Err(refusal) => Err(refusal),
        }
    }

    fn on_terminal(&self, terminal: ModelExecutionTerminal) {
        self.inner.on_terminal(terminal);
    }
}

impl ModelExecutionPermit for BoundedExecutionPermit {
    fn mark_dispatched(&self) -> std::result::Result<(), ModelExecutionRefusal> {
        self.inner.mark_dispatched()
    }

    fn mark_physical_attempt(&self) -> std::result::Result<(), ModelExecutionRefusal> {
        self.inner.mark_physical_attempt()
    }
}

fn refusal_error(refusal: ModelExecutionRefusal) -> crate::error::Error {
    crate::error::Error::ModelExecutionRefused {
        refusal,
        usage: None,
        accounting_complete: true,
    }
}

impl ReviewRunContext {
    /// Create the shared scope from the run's original absolute clock window.
    pub(crate) fn new(options: ReviewExecutionOptions) -> Self {
        Self {
            execution: Arc::new(ReviewExecution::new(options)),
            model: OnceLock::new(),
            retrieval_usage: Mutex::new(Usage::default()),
            retrieval_models: Mutex::new(Vec::new()),
            logical_usage: Mutex::new(Usage::default()),
            logical_models: Mutex::new(Vec::new()),
        }
    }

    /// Bind monetary and physical hooks once, before any paid model operation.
    ///
    /// A missing monetary hook preserves legacy behavior; Large-mode callers
    /// require physical control separately, while the production Gateway
    /// supplies the shared monetary root.
    pub(crate) fn bind_model(&self, base: Arc<dyn Model>, budget_usd: f64) -> &ReviewModelBinding {
        self.model.get_or_init(|| {
            let rooted = base.with_review_budget(budget_usd);
            #[cfg(test)]
            let review_budget_bound = rooted.is_some();
            let rooted = rooted.unwrap_or(base);
            let execution: Arc<dyn ModelExecutionControl> = Arc::new(BoundedExecutionControl {
                inner: self.execution.clone(),
                permits: Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_MODEL_DISPATCHES)),
            });
            let controlled = rooted.with_execution_control(execution);
            let execution_control_bound = controlled.is_some();
            ReviewModelBinding {
                model: controlled.unwrap_or_else(|| {
                    Arc::new(BoundedLogicalModel::new(rooted, self.execution.clone()))
                }),
                #[cfg(test)]
                review_budget_bound,
                execution_control_bound,
            }
        })
    }

    /// The model binding initialized for this job, if it has started an attempt.
    pub(crate) fn model_binding(&self) -> Option<&ReviewModelBinding> {
        self.model.get()
    }

    /// Shared physical execution control for this review job.
    pub(crate) fn execution(&self) -> Arc<ReviewExecution> {
        Arc::clone(&self.execution)
    }

    /// Accumulate embedding work over every attempt in this review job.
    pub(crate) fn record_retrieval_usage(&self, usage: Usage, models: &[String]) {
        let mut total = lock(&self.retrieval_usage);
        total.embed_tokens = total.embed_tokens.saturating_add(usage.embed_tokens);
        let cost = total.cost_usd + usage.cost_usd;
        total.cost_usd = if cost.is_finite() { cost } else { f64::MAX };
        drop(total);
        let mut known_models = lock(&self.retrieval_models);
        for model in models {
            if !known_models.contains(model) {
                known_models.push(model.clone());
            }
        }
    }

    /// The embedding invoice accumulated across every attempt in this job.
    pub(crate) fn retrieval_usage(&self) -> Usage {
        *lock(&self.retrieval_usage)
    }

    /// Models that billed retrieval work over every attempt in this job.
    pub(crate) fn retrieval_models(&self) -> Vec<String> {
        lock(&self.retrieval_models).clone()
    }

    /// Retain successful nonphysical model usage for the job-level proposal.
    pub(crate) fn record_logical_usage(&self, usage: Usage, models: &[String]) -> Usage {
        let mut total = lock(&self.logical_usage);
        total.add(usage);
        let mut known_models = lock(&self.logical_models);
        for model in models {
            if !known_models.contains(model) {
                known_models.push(model.clone());
            }
        }
        *total
    }

    /// Models that supplied the retained nonphysical usage.
    pub(crate) fn logical_models(&self) -> Vec<String> {
        lock(&self.logical_models).clone()
    }

    /// The retained nonphysical usage for a job skipped after earlier work.
    pub(crate) fn logical_usage(&self) -> Usage {
        *lock(&self.logical_usage)
    }
}

#[derive(Debug)]
struct ExecutionState {
    committed_calls: usize,
    reserved_calls: usize,
    physical_accounting: PhysicalExecutionAccounting,
    #[cfg(test)]
    physical_terminals: Vec<ModelExecutionTerminal>,
    cancelled: bool,
    max_calls: Option<usize>,
}

#[derive(Debug)]
struct PhysicalExecutionAccounting {
    terminal_count: u64,
    usage: Usage,
    usage_by_model: BTreeMap<String, Usage>,
    complete: bool,
}

impl Default for PhysicalExecutionAccounting {
    fn default() -> Self {
        Self {
            terminal_count: 0,
            usage: Usage::default(),
            usage_by_model: BTreeMap::new(),
            complete: true,
        }
    }
}

impl PhysicalExecutionAccounting {
    fn record(&mut self, terminal: &ModelExecutionTerminal) {
        let succeeded = matches!(terminal.outcome, ModelExecutionTerminalOutcome::Succeeded);
        let refused =
            terminal.refused || matches!(terminal.outcome, ModelExecutionTerminalOutcome::Refused);
        let mut usage = Usage {
            calls_attempted: 1,
            calls_succeeded: u64::from(succeeded),
            calls_refused: u64::from(refused),
            calls_errored: u64::from(!succeeded),
            ..Usage::default()
        };
        if let Some(reported) = terminal.usage {
            usage.input_tokens = reported.input_tokens;
            usage.output_tokens = reported.output_tokens;
            usage.reasoning_tokens = reported.reasoning_tokens;
            usage.cached_tokens = reported.cached_tokens;
            match reported
                .cost_usd
                .filter(|cost| cost.is_finite() && *cost >= 0.0)
            {
                Some(cost) => usage.cost_usd = cost,
                None => self.complete = false,
            }
        } else {
            self.complete = false;
        }

        if !add_usage_saturating(&mut self.usage, usage) {
            self.complete = false;
        }
        let model = terminal
            .answered_model
            .as_deref()
            .filter(|model| !model.is_empty())
            .unwrap_or(&terminal.requested_model)
            .to_owned();
        if !add_usage_saturating(self.usage_by_model.entry(model).or_default(), usage) {
            self.complete = false;
        }
        self.terminal_count = self.terminal_count.saturating_add(1);
    }
}

fn add_usage_saturating(total: &mut Usage, addition: Usage) -> bool {
    let mut complete = true;
    complete &= saturating_add_exact(&mut total.input_tokens, addition.input_tokens);
    complete &= saturating_add_exact(&mut total.output_tokens, addition.output_tokens);
    complete &= saturating_add_exact(&mut total.reasoning_tokens, addition.reasoning_tokens);
    complete &= saturating_add_exact(&mut total.cached_tokens, addition.cached_tokens);
    complete &= saturating_add_exact(&mut total.calls_attempted, addition.calls_attempted);
    complete &= saturating_add_exact(&mut total.calls_succeeded, addition.calls_succeeded);
    complete &= saturating_add_exact(&mut total.calls_refused, addition.calls_refused);
    complete &= saturating_add_exact(&mut total.calls_errored, addition.calls_errored);
    let cost = total.cost_usd + addition.cost_usd;
    if cost.is_finite() {
        total.cost_usd = cost;
    } else {
        total.cost_usd = f64::MAX;
        complete = false;
    }
    complete
}

fn saturating_add_exact(total: &mut u64, addition: u64) -> bool {
    match total.checked_add(addition) {
        Some(sum) => {
            *total = sum;
            true
        }
        None => {
            *total = u64::MAX;
            false
        }
    }
}

struct ExecutionInner {
    #[cfg(test)]
    started_at: Instant,
    hard_deadline: Instant,
    soft_deadline: Instant,
    #[cfg(test)]
    mode: ReviewMode,
    drain_seconds: u64,
    _now: Arc<dyn Fn() -> Instant + Send + Sync>,
    availability_generation: watch::Sender<u64>,
    state: Mutex<ExecutionState>,
}

/// Shared execution limits and accounting for one pull-request review.
#[derive(Clone)]
pub(crate) struct ReviewExecution {
    inner: Arc<ExecutionInner>,
}

impl ReviewExecution {
    /// Create a scope from a hard deadline established before checkout work.
    pub(crate) fn new(options: ReviewExecutionOptions) -> Self {
        Self::with_clock(options, Arc::new(Instant::now))
    }

    fn with_clock(
        options: ReviewExecutionOptions,
        now: Arc<dyn Fn() -> Instant + Send + Sync>,
    ) -> Self {
        let window = options
            .hard_deadline
            .saturating_duration_since(options.started_at);
        let soft_deadline =
            options.started_at + window.mul_f64(f64::from(options.soft_deadline_percent) / 100.0);
        let (availability_generation, _) = watch::channel(0);
        Self {
            inner: Arc::new(ExecutionInner {
                #[cfg(test)]
                started_at: options.started_at,
                hard_deadline: options.hard_deadline,
                soft_deadline,
                #[cfg(test)]
                mode: options.mode,
                drain_seconds: options.drain_seconds,
                _now: now,
                availability_generation,
                state: Mutex::new(ExecutionState {
                    committed_calls: 0,
                    reserved_calls: 0,
                    physical_accounting: PhysicalExecutionAccounting::default(),
                    #[cfg(test)]
                    physical_terminals: Vec::new(),
                    cancelled: false,
                    max_calls: options.max_calls,
                }),
            }),
        }
    }

    /// The original absolute review start, before checkout and setup.
    #[cfg(test)]
    pub(crate) fn started_at(&self) -> Instant {
        self.inner.started_at
    }

    /// The absolute hard deadline inherited from the server job.
    pub(crate) fn hard_deadline(&self) -> Instant {
        self.inner.hard_deadline
    }

    /// The absolute soft deadline for stopping new work.
    pub(crate) fn soft_deadline(&self) -> Instant {
        self.inner.soft_deadline
    }

    /// Refusal for optional new work after ownership loss, an exhausted
    /// committed-call ceiling, or the absolute soft deadline. This preflight
    /// does not reserve a physical dispatch permit or emit a terminal event;
    /// refundable reservations alone do not exhaust the ceiling.
    pub(crate) fn refusal_for_new_work(&self) -> Option<ModelExecutionRefusal> {
        let state = lock(&self.inner.state);
        if state.cancelled {
            Some(ModelExecutionRefusal::Cancelled)
        } else if (self.inner._now)() >= self.inner.soft_deadline {
            Some(ModelExecutionRefusal::SoftDeadline)
        } else if state
            .max_calls
            .is_some_and(|limit| state.committed_calls >= limit)
        {
            Some(ModelExecutionRefusal::CallLimit)
        } else {
            None
        }
    }

    /// The mode used when this execution scope was created.
    ///
    /// A later per-head plan may tighten the job's call ceiling without
    /// changing this initial scope mode.
    #[cfg(test)]
    pub(crate) fn mode(&self) -> ReviewMode {
        self.inner.mode
    }

    /// Maximum time reserved for admitted work to drain.
    pub(crate) fn drain_seconds(&self) -> u64 {
        self.inner.drain_seconds
    }

    /// The effective physical-call ceiling, tightened monotonically by later
    /// Large-mode plans when the scope began in Normal mode.
    #[cfg(test)]
    pub(crate) fn max_calls(&self) -> Option<usize> {
        lock(&self.inner.state).max_calls
    }

    /// Tighten this review job's physical-call ceiling after Large mode is
    /// selected from host-derived metadata.
    ///
    /// A retry may inspect a newer pull-request head, but remains in the same
    /// job: tightening never resets spent calls or raises an earlier ceiling.
    /// Normal-mode jobs remain uncapped unless this is called.
    pub(crate) fn restrict_to_large(&self, max_calls: usize) {
        let mut state = lock(&self.inner.state);
        let tightened = match state.max_calls {
            Some(existing) => existing.min(max_calls),
            None => max_calls,
        };
        if state.max_calls == Some(tightened) {
            return;
        }
        state.max_calls = Some(tightened);
        drop(state);
        self.inner
            .availability_generation
            .send_modify(|generation| *generation = generation.wrapping_add(1));
    }

    /// Snapshot the counters used when the proposal is finalized.
    pub(crate) fn snapshot(&self) -> ReviewExecutionSnapshot {
        let state = lock(&self.inner.state);
        let physical_accounting_complete = state.physical_accounting.complete
            && u64::try_from(state.committed_calls).unwrap_or(u64::MAX)
                == state.physical_accounting.terminal_count;
        ReviewExecutionSnapshot {
            committed_calls: state.committed_calls,
            reserved_calls: state.reserved_calls,
            physical_usage: state.physical_accounting.usage,
            physical_usage_by_model: state.physical_accounting.usage_by_model.clone(),
            physical_terminal_count: state.physical_accounting.terminal_count,
            #[cfg(test)]
            physical_terminals: state.physical_terminals.clone(),
            physical_accounting_complete,
        }
    }

    /// Cancel this review's remaining execution and wake pending acquisitions.
    #[cfg(any(test, feature = "serve"))]
    pub(crate) fn cancel(&self) {
        let mut state = lock(&self.inner.state);
        if !state.cancelled {
            state.cancelled = true;
            drop(state);
            self.inner
                .availability_generation
                .send_modify(|generation| *generation = generation.wrapping_add(1));
        }
    }

    /// Whether an ownership fence has stopped this review's remaining work.
    pub(crate) fn is_cancelled(&self) -> bool {
        lock(&self.inner.state).cancelled
    }

    /// Wait until the owner cancels this execution scope.
    pub(crate) async fn cancelled(&self) {
        let mut generation = self.inner.availability_generation.subscribe();
        loop {
            if self.is_cancelled() {
                return;
            }
            if generation.changed().await.is_err() {
                return;
            }
        }
    }
}

#[async_trait::async_trait]
impl ModelExecutionControl for ReviewExecution {
    async fn acquire(
        &self,
    ) -> std::result::Result<Box<dyn ModelExecutionPermit>, ModelExecutionRefusal> {
        let mut availability = self.inner.availability_generation.subscribe();
        loop {
            let changed = availability.changed();
            tokio::pin!(changed);

            {
                let mut state = lock(&self.inner.state);
                if state.cancelled {
                    return Err(ModelExecutionRefusal::Cancelled);
                }

                if (self.inner._now)() >= self.inner.soft_deadline {
                    return Err(ModelExecutionRefusal::SoftDeadline);
                }

                if state
                    .max_calls
                    .is_some_and(|limit| state.committed_calls >= limit)
                {
                    return Err(ModelExecutionRefusal::CallLimit);
                }

                let capacity_is_reserved = state.max_calls.is_some_and(|limit| {
                    state.committed_calls.saturating_add(state.reserved_calls) >= limit
                });
                if !capacity_is_reserved {
                    state.reserved_calls = state.reserved_calls.saturating_add(1);
                    return Ok(Box::new(ReviewDispatchPermit {
                        inner: Arc::clone(&self.inner),
                        state: AtomicU8::new(PERMIT_RESERVED),
                        physical_attempts: AtomicUsize::new(0),
                    }));
                }
            }

            tokio::select! {
                _ = &mut changed => {}
                _ = tokio::time::sleep_until(self.inner.soft_deadline) => {}
            }
        }
    }

    fn on_terminal(&self, terminal: ModelExecutionTerminal) {
        let mut state = lock(&self.inner.state);
        state.physical_accounting.record(&terminal);
        #[cfg(test)]
        state.physical_terminals.push(terminal);
    }
}

const PERMIT_RESERVED: u8 = 0;
const PERMIT_COMMITTED: u8 = 1;
const PERMIT_REFUSED: u8 = 2;
const PERMIT_RELEASED: u8 = 3;
const PERMIT_CANCELLED: u8 = 4;
const PERMIT_CALL_LIMIT: u8 = 5;

struct ReviewDispatchPermit {
    inner: Arc<ExecutionInner>,
    state: AtomicU8,
    physical_attempts: AtomicUsize,
}

impl ModelExecutionPermit for ReviewDispatchPermit {
    fn mark_dispatched(&self) -> std::result::Result<(), ModelExecutionRefusal> {
        match self.state.load(Ordering::Acquire) {
            // A committed permit can be reused by adapters that retry a
            // physical send, so it still has to recheck cancellation and the
            // deadline below. Its already-counted dispatch is never charged
            // again.
            PERMIT_COMMITTED => {}
            PERMIT_REFUSED => return Err(ModelExecutionRefusal::SoftDeadline),
            PERMIT_CANCELLED => return Err(ModelExecutionRefusal::Cancelled),
            PERMIT_CALL_LIMIT => return Err(ModelExecutionRefusal::CallLimit),
            PERMIT_RESERVED => {}
            _ => return Err(ModelExecutionRefusal::SoftDeadline),
        }

        let mut state = lock(&self.inner.state);
        match self.state.load(Ordering::Acquire) {
            PERMIT_COMMITTED => {}
            PERMIT_REFUSED => return Err(ModelExecutionRefusal::SoftDeadline),
            PERMIT_CANCELLED => return Err(ModelExecutionRefusal::Cancelled),
            PERMIT_CALL_LIMIT => return Err(ModelExecutionRefusal::CallLimit),
            PERMIT_RESERVED => {}
            _ => return Err(ModelExecutionRefusal::SoftDeadline),
        }

        if state.cancelled {
            if self.state.load(Ordering::Acquire) != PERMIT_COMMITTED {
                self.state.store(PERMIT_CANCELLED, Ordering::Release);
            }
            return Err(ModelExecutionRefusal::Cancelled);
        }

        if (self.inner._now)() >= self.inner.soft_deadline {
            if self.state.load(Ordering::Acquire) != PERMIT_COMMITTED {
                self.state.store(PERMIT_REFUSED, Ordering::Release);
            }
            return Err(ModelExecutionRefusal::SoftDeadline);
        }

        if self.state.load(Ordering::Acquire) == PERMIT_COMMITTED {
            return Ok(());
        }

        if state
            .max_calls
            .is_some_and(|limit| state.committed_calls >= limit)
        {
            self.state.store(PERMIT_CALL_LIMIT, Ordering::Release);
            return Err(ModelExecutionRefusal::CallLimit);
        }

        state.reserved_calls = state.reserved_calls.saturating_sub(1);
        state.committed_calls = state.committed_calls.saturating_add(1);
        self.state.store(PERMIT_COMMITTED, Ordering::Release);
        drop(state);
        self.inner
            .availability_generation
            .send_modify(|generation| *generation = generation.wrapping_add(1));
        Ok(())
    }

    fn mark_physical_attempt(&self) -> std::result::Result<(), ModelExecutionRefusal> {
        let mut state = lock(&self.inner.state);
        match self.state.load(Ordering::Acquire) {
            PERMIT_COMMITTED => {}
            PERMIT_CANCELLED => return Err(ModelExecutionRefusal::Cancelled),
            PERMIT_CALL_LIMIT => return Err(ModelExecutionRefusal::CallLimit),
            PERMIT_REFUSED | PERMIT_RELEASED | PERMIT_RESERVED => {
                return Err(ModelExecutionRefusal::SoftDeadline);
            }
            _ => return Err(ModelExecutionRefusal::SoftDeadline),
        }

        if state.cancelled {
            return Err(ModelExecutionRefusal::Cancelled);
        }

        if (self.inner._now)() >= self.inner.soft_deadline {
            return Err(ModelExecutionRefusal::SoftDeadline);
        }

        let physical_attempts = self.physical_attempts.load(Ordering::Relaxed);
        if physical_attempts == 0 {
            self.physical_attempts.store(1, Ordering::Release);
            return Ok(());
        }

        if state.max_calls.is_some_and(|limit| {
            state.committed_calls.saturating_add(state.reserved_calls) >= limit
        }) {
            return Err(ModelExecutionRefusal::CallLimit);
        }

        state.committed_calls = state.committed_calls.saturating_add(1);
        self.physical_attempts
            .store(physical_attempts.saturating_add(1), Ordering::Release);
        drop(state);
        self.inner
            .availability_generation
            .send_modify(|generation| *generation = generation.wrapping_add(1));
        Ok(())
    }
}

impl Drop for ReviewDispatchPermit {
    fn drop(&mut self) {
        if self.state.load(Ordering::Acquire) != PERMIT_COMMITTED {
            let mut state = lock(&self.inner.state);
            state.reserved_calls = state.reserved_calls.saturating_sub(1);
            self.state.store(PERMIT_RELEASED, Ordering::Release);
            drop(state);
            self.inner
                .availability_generation
                .send_modify(|generation| *generation = generation.wrapping_add(1));
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    use crate::ports::model_execution::ModelExecutionControl;

    #[derive(Clone)]
    struct BindingProbe {
        model: Arc<dyn Model>,
        budget_hooks: Arc<std::sync::atomic::AtomicUsize>,
        control_hooks: Arc<std::sync::atomic::AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl Model for BindingProbe {
        fn complete_reports_physical_calls(&self) -> bool {
            self.model.complete_reports_physical_calls()
        }

        async fn complete(
            &self,
            request: crate::ports::model::ModelRequest,
        ) -> crate::error::Result<crate::ports::model::ModelResponse> {
            self.model.complete(request).await
        }

        fn with_review_budget(&self, _limit_usd: f64) -> Option<Arc<dyn Model>> {
            self.budget_hooks
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Some(Arc::new(self.clone()))
        }

        fn with_execution_control(
            &self,
            control: Arc<dyn ModelExecutionControl>,
        ) -> Option<Arc<dyn Model>> {
            self.control_hooks
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            self.model.with_execution_control(control)
        }
    }

    struct LegacyOpaqueMockModel(crate::harness::mock::MockModel);

    #[async_trait::async_trait]
    impl Model for LegacyOpaqueMockModel {
        async fn complete(
            &self,
            request: crate::ports::model::ModelRequest,
        ) -> crate::error::Result<crate::ports::model::ModelResponse> {
            self.0.complete(request).await
        }
    }

    fn options(
        mode: ReviewMode,
        max_calls: Option<usize>,
        started_at: Instant,
        hard_deadline: Instant,
    ) -> ReviewExecutionOptions {
        ReviewExecutionOptions {
            started_at,
            hard_deadline,
            mode,
            max_calls,
            soft_deadline_percent: 80,
            drain_seconds: 30,
        }
    }

    #[tokio::test]
    async fn queued_dispatch_admission_observes_cancel_and_original_soft_deadline() {
        let started = Instant::now();
        let execution = Arc::new(ReviewExecution::new(options(
            ReviewMode::Normal,
            None,
            started,
            started + Duration::from_secs(2),
        )));
        let control = Arc::new(BoundedExecutionControl {
            inner: execution.clone(),
            permits: Arc::new(tokio::sync::Semaphore::new(0)),
        });
        let waiting_control = control.clone();
        let waiting = tokio::spawn(async move { waiting_control.acquire().await });
        tokio::task::yield_now().await;
        execution.cancel();
        let cancelled = tokio::time::timeout(Duration::from_millis(100), waiting)
            .await
            .expect("owner cancellation wakes a dispatch waiting for capacity")
            .expect("waiter task completes");
        assert!(matches!(cancelled, Err(ModelExecutionRefusal::Cancelled)));
        assert_eq!(execution.snapshot().reserved_calls, 0);
        assert_eq!(execution.snapshot().committed_calls, 0);

        let started = Instant::now();
        let mut soft_options = options(
            ReviewMode::Normal,
            None,
            started,
            started + Duration::from_millis(100),
        );
        soft_options.soft_deadline_percent = 10;
        let execution = Arc::new(ReviewExecution::new(soft_options));
        let control = Arc::new(BoundedExecutionControl {
            inner: execution.clone(),
            permits: Arc::new(tokio::sync::Semaphore::new(0)),
        });
        let waiting = tokio::spawn(async move { control.acquire().await });
        let expired = tokio::time::timeout(Duration::from_millis(200), waiting)
            .await
            .expect("the original soft deadline wakes a dispatch waiting for capacity")
            .expect("waiter task completes");
        assert!(matches!(expired, Err(ModelExecutionRefusal::SoftDeadline)));
        assert_eq!(execution.snapshot().reserved_calls, 0);
        assert_eq!(execution.snapshot().committed_calls, 0);
    }

    #[tokio::test]
    async fn opaque_call_released_after_soft_deadline_never_reaches_the_model() {
        struct CountingModel {
            inner: crate::harness::mock::MockModel,
            calls: Arc<std::sync::atomic::AtomicUsize>,
        }

        #[async_trait::async_trait]
        impl Model for CountingModel {
            async fn complete(
                &self,
                request: crate::ports::model::ModelRequest,
            ) -> crate::error::Result<crate::ports::model::ModelResponse> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                self.inner.complete(request).await
            }
        }

        let started = Instant::now();
        let mut options = options(
            ReviewMode::Normal,
            None,
            started,
            started + Duration::from_millis(500),
        );
        options.soft_deadline_percent = 40;
        let execution = Arc::new(ReviewExecution::new(options));
        let permits = Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_MODEL_DISPATCHES));
        let mut held = Vec::new();
        for _ in 0..MAX_CONCURRENT_MODEL_DISPATCHES {
            held.push(permits.clone().acquire_owned().await.expect("permit"));
        }
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let model: Arc<dyn Model> = Arc::new(BoundedLogicalModel {
            inner: Arc::new(CountingModel {
                inner: crate::harness::mock::MockModel::always(serde_json::json!({"ok": true})),
                calls: calls.clone(),
            }),
            permits,
            execution: execution.clone(),
        });
        let waiting_model = model.clone();
        let waiting = tokio::spawn(async move {
            waiting_model
                .complete(crate::ports::model::ModelRequest {
                    model: "mock".into(),
                    messages: Vec::new(),
                    schema: serde_json::json!({"type": "object"}),
                    schema_name: "queued".into(),
                    reasoning_effort: None,
                    max_tokens: 8,
                })
                .await
        });
        tokio::task::yield_now().await;
        tokio::time::sleep_until(execution.soft_deadline()).await;
        drop(held.pop());

        let result = tokio::time::timeout(Duration::from_millis(100), waiting)
            .await
            .expect("soft deadline wakes the queued logical call")
            .expect("model task completes");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "a call queued before the cutoff must not enter the model after it"
        );
        assert!(matches!(
            result,
            Err(crate::error::Error::ModelExecutionRefused {
                refusal: ModelExecutionRefusal::SoftDeadline,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn review_model_hooks_bind_once_without_resetting_execution_state() {
        let started_at = Instant::now();
        let context = ReviewRunContext::new(options(
            ReviewMode::Normal,
            None,
            started_at,
            started_at + Duration::from_secs(100),
        ));
        let execution = context.execution();
        execution
            .acquire()
            .await
            .expect("the initial normal scope admits work")
            .mark_dispatched()
            .expect("the initial dispatch commits");
        execution.restrict_to_large(1);

        let mock = crate::harness::mock::MockModel::silent();
        let budget_hooks = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let control_hooks = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let first_base: Arc<dyn Model> = Arc::new(BindingProbe {
            model: Arc::new(mock.clone()),
            budget_hooks: Arc::clone(&budget_hooks),
            control_hooks: Arc::clone(&control_hooks),
        });
        let first = context.bind_model(Arc::clone(&first_base), 2.0).clone();

        let other_budget_hooks = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let other_control_hooks = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let second_base: Arc<dyn Model> = Arc::new(BindingProbe {
            model: Arc::new(crate::harness::mock::MockModel::silent()),
            budget_hooks: Arc::clone(&other_budget_hooks),
            control_hooks: Arc::clone(&other_control_hooks),
        });
        let second = context.bind_model(second_base, 100.0).clone();

        assert!(Arc::ptr_eq(&first.model, &second.model));
        assert!(first.review_budget_bound);
        assert!(first.execution_control_bound);
        assert_eq!(budget_hooks.load(std::sync::atomic::Ordering::Relaxed), 1);
        assert_eq!(control_hooks.load(std::sync::atomic::Ordering::Relaxed), 1);
        assert_eq!(
            other_budget_hooks.load(std::sync::atomic::Ordering::Relaxed),
            0
        );
        assert_eq!(
            other_control_hooks.load(std::sync::atomic::Ordering::Relaxed),
            0
        );
        assert!(matches!(
            first
                .model
                .complete(crate::ports::model::ModelRequest {
                    model: "mock".into(),
                    messages: vec![crate::ports::model::Message::user("must be refused")],
                    schema: serde_json::json!({"type":"object"}),
                    schema_name: "review-binding-cap".into(),
                    reasoning_effort: None,
                    max_tokens: 8,
                })
                .await,
            Err(crate::error::Error::ModelExecutionRefused {
                refusal: ModelExecutionRefusal::CallLimit,
                ..
            })
        ));
        assert_eq!(mock.calls(), 0, "refusal precedes mock request recording");
        assert_eq!(execution.snapshot().committed_calls, 1);
    }

    #[tokio::test]
    async fn a_retry_keeps_the_original_soft_window_and_committed_dispatches() {
        use crate::ports::model::{Message, ModelRequest, Usage};
        use crate::ports::model_execution::ModelExecutionRefusal;

        let started_at = Instant::now();
        let after_soft_deadline = Arc::new(AtomicBool::new(false));
        let clock_flag = Arc::clone(&after_soft_deadline);
        let now: Arc<dyn Fn() -> Instant + Send + Sync> = Arc::new(move || {
            if clock_flag.load(Ordering::Acquire) {
                started_at + Duration::from_secs(81)
            } else {
                started_at
            }
        });
        let execution = Arc::new(ReviewExecution::with_clock(
            options(
                ReviewMode::Large,
                Some(2),
                started_at,
                started_at + Duration::from_secs(100),
            ),
            now,
        ));
        assert_eq!(
            execution.hard_deadline(),
            started_at + Duration::from_secs(100)
        );
        assert_eq!(
            execution.soft_deadline(),
            started_at + Duration::from_secs(80)
        );
        let original_context = Arc::new(ReviewRunContext {
            execution: Arc::clone(&execution),
            model: OnceLock::new(),
            retrieval_usage: Mutex::new(Usage::default()),
            retrieval_models: Mutex::new(Vec::new()),
            logical_usage: Mutex::new(Usage::default()),
            logical_models: Mutex::new(Vec::new()),
        });
        let mock = crate::harness::mock::MockModel::silent().with_usage(Usage {
            input_tokens: 7,
            output_tokens: 3,
            calls_attempted: 1,
            calls_succeeded: 1,
            cost_usd: 0.02,
            ..Usage::default()
        });
        let model = original_context
            .bind_model(Arc::new(mock.clone()), 1.0)
            .model
            .clone();
        let retry_context = Arc::clone(&original_context);
        assert!(Arc::ptr_eq(&original_context, &retry_context));
        assert_eq!(retry_context.execution().started_at(), started_at);
        let retry_model = retry_context
            .bind_model(Arc::new(crate::harness::mock::MockModel::silent()), 99.0)
            .model
            .clone();
        assert!(Arc::ptr_eq(&model, &retry_model));

        let request = || ModelRequest {
            model: "mock".into(),
            messages: vec![Message::user("retry the same review")],
            schema: serde_json::json!({"type":"object"}),
            schema_name: "same-review-retry".into(),
            reasoning_effort: None,
            max_tokens: 16,
        };
        assert!(model.complete(request()).await.is_ok());
        assert_eq!(execution.snapshot().committed_calls, 1);
        assert_eq!(mock.calls(), 1);

        // A transient retry reuses the original worker clock, so it cannot
        // reopen the paid window after the first dispatch has committed.
        after_soft_deadline.store(true, Ordering::Release);
        assert!(matches!(
            retry_model.complete(request()).await,
            Err(crate::error::Error::ModelExecutionRefused {
                refusal: ModelExecutionRefusal::SoftDeadline,
                ..
            })
        ));
        assert_eq!(mock.calls(), 1, "the retry is refused before provider work");
        assert_eq!(execution.snapshot().committed_calls, 1);
        assert_eq!(execution.snapshot().reserved_calls, 0);

        let replacement_started_at = Instant::now();
        let replacement_execution = Arc::new(ReviewExecution::new(options(
            ReviewMode::Large,
            Some(2),
            replacement_started_at,
            replacement_started_at + Duration::from_secs(100),
        )));
        let replacement_context = Arc::new(ReviewRunContext {
            execution: Arc::clone(&replacement_execution),
            model: OnceLock::new(),
            retrieval_usage: Mutex::new(Usage::default()),
            retrieval_models: Mutex::new(Vec::new()),
            logical_usage: Mutex::new(Usage::default()),
            logical_models: Mutex::new(Vec::new()),
        });
        let replacement_mock = crate::harness::mock::MockModel::silent();
        let replacement_model = replacement_context
            .bind_model(Arc::new(replacement_mock.clone()), 1.0)
            .model
            .clone();
        assert!(!Arc::ptr_eq(&original_context, &replacement_context));
        assert_ne!(replacement_execution.started_at(), started_at);
        assert!(replacement_model.complete(request()).await.is_ok());
        assert_eq!(replacement_mock.calls(), 1);
        assert_eq!(replacement_execution.snapshot().committed_calls, 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_dispatches_wait_for_reservations_then_refuse_at_the_shared_ceiling() {
        let started_at = Instant::now();
        let execution = Arc::new(ReviewExecution::new(options(
            ReviewMode::Large,
            Some(3),
            started_at,
            started_at + Duration::from_secs(100),
        )));
        let control: Arc<dyn ModelExecutionControl> = execution.clone();
        let start_barrier = Arc::new(tokio::sync::Barrier::new(101));
        let (started_tx, mut started_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut tasks = tokio::task::JoinSet::new();
        for _ in 0..100 {
            let control = Arc::clone(&control);
            let start_barrier = Arc::clone(&start_barrier);
            let started_tx = started_tx.clone();
            tasks.spawn(async move {
                started_tx.send(()).expect("test receiver");
                start_barrier.wait().await;
                control.acquire().await
            });
        }
        drop(started_tx);
        for _ in 0..100 {
            started_rx.recv().await.expect("acquire task started");
        }
        start_barrier.wait().await;

        let mut permits = Vec::new();
        let admissions = tokio::time::timeout(Duration::from_secs(2), async {
            while permits.len() < 3 {
                let result = tasks.join_next().await.expect("admission task");
                match result.expect("dispatch task") {
                    Ok(permit) => permits.push(permit),
                    Err(reason) => panic!("empty call budget refused an admission: {reason:?}"),
                }
            }
        })
        .await;
        assert!(
            admissions.is_ok(),
            "three concurrent admissions complete promptly"
        );

        let pending = tokio::time::timeout(Duration::from_millis(25), tasks.join_next()).await;
        assert!(
            pending.is_err(),
            "callers wait while all three admitted reservations are held"
        );
        let snapshot = execution.snapshot();
        assert_eq!(snapshot.reserved_calls, 3);
        assert_eq!(snapshot.committed_calls, 0);

        for permit in &permits {
            permit.mark_dispatched().expect("commit reserved slot");
        }
        let refusals = tokio::time::timeout(Duration::from_secs(2), async {
            let mut refused = 0;
            while let Some(result) = tasks.join_next().await {
                match result.expect("dispatch task") {
                    Err(reason) => assert_eq!(reason, ModelExecutionRefusal::CallLimit),
                    Ok(_) => panic!("committed quota must refuse after all reserved slots commit"),
                }
                refused += 1;
            }
            refused
        })
        .await;

        assert_eq!(refusals.expect("bounded refusal completion"), 97);
        let snapshot = execution.snapshot();
        assert_eq!(snapshot.committed_calls, 3);
        assert_eq!(snapshot.reserved_calls, 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn one_hundred_mock_model_calls_share_the_three_call_execution_limit() {
        use crate::ports::model::{Message, ModelRequest};

        let started_at = Instant::now();
        let execution = Arc::new(ReviewExecution::new(options(
            ReviewMode::Large,
            Some(3),
            started_at,
            started_at + Duration::from_secs(100),
        )));
        let control: Arc<dyn ModelExecutionControl> = execution.clone();
        let mock = crate::harness::mock::MockModel::always(serde_json::json!({"ok": true}));
        let model = mock
            .with_execution_control(control)
            .expect("the offline mock accepts review-wide execution control");
        let barrier = Arc::new(tokio::sync::Barrier::new(101));
        let mut tasks = tokio::task::JoinSet::new();

        for _ in 0..100 {
            let barrier = Arc::clone(&barrier);
            let model = Arc::clone(&model);
            tasks.spawn(async move {
                barrier.wait().await;
                model
                    .complete(ModelRequest {
                        model: "mock".into(),
                        messages: vec![Message::user("concurrent model call")],
                        schema: serde_json::json!({"type": "object"}),
                        schema_name: "review-execution-concurrency".into(),
                        reasoning_effort: None,
                        max_tokens: 8,
                    })
                    .await
            });
        }
        barrier.wait().await;

        let mut succeeded = 0;
        let mut refused = 0;
        while let Some(joined) = tasks.join_next().await {
            match joined.expect("mock dispatch task completes") {
                Ok(_) => succeeded += 1,
                Err(crate::error::Error::ModelExecutionRefused {
                    refusal: ModelExecutionRefusal::CallLimit,
                    ..
                }) => refused += 1,
                Err(error) => panic!("unexpected mock dispatch result: {error}"),
            }
        }

        assert_eq!(succeeded, 3);
        assert_eq!(refused, 97);
        assert_eq!(
            mock.calls(),
            3,
            "only admitted calls reach MockModel recording"
        );
        assert_eq!(mock.requests().len(), 3);
        let snapshot = execution.snapshot();
        assert_eq!(snapshot.committed_calls, 3);
        assert_eq!(snapshot.reserved_calls, 0);
    }

    #[tokio::test]
    async fn acquisition_uses_the_absolute_soft_deadline_from_review_start() {
        let started_at = Instant::now() - Duration::from_secs(85);
        let hard_deadline = started_at + Duration::from_secs(100);
        let execution =
            ReviewExecution::new(options(ReviewMode::Normal, None, started_at, hard_deadline));

        assert!(execution.soft_deadline() < Instant::now());
        assert!(execution.hard_deadline() > Instant::now());
        assert_eq!(execution.started_at(), started_at);
        assert_eq!(
            execution.acquire().await.err(),
            Some(ModelExecutionRefusal::SoftDeadline)
        );
    }

    #[tokio::test]
    async fn dropped_reservations_are_refunded_but_committed_slots_are_not() {
        let started_at = Instant::now();
        let execution = ReviewExecution::new(options(
            ReviewMode::Large,
            Some(4),
            started_at,
            started_at + Duration::from_secs(100),
        ));

        let abandoned = execution.acquire().await.expect("reservation");
        assert_eq!(execution.snapshot().reserved_calls, 1);
        drop(abandoned);
        assert_eq!(execution.snapshot().reserved_calls, 0);

        let committed = execution.acquire().await.expect("reservation");
        committed.mark_dispatched().expect("commit");
        drop(committed);
        let snapshot = execution.snapshot();
        assert_eq!(snapshot.reserved_calls, 0);
        assert_eq!(snapshot.committed_calls, 1);
    }

    #[tokio::test]
    async fn abandoning_a_full_reservation_wakes_the_next_acquirer() {
        let started_at = Instant::now();
        let execution = Arc::new(ReviewExecution::new(options(
            ReviewMode::Large,
            Some(1),
            started_at,
            started_at + Duration::from_secs(100),
        )));
        let control: Arc<dyn ModelExecutionControl> = execution.clone();
        let first = control.acquire().await.expect("first reservation");
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let mut task = tokio::task::JoinSet::new();
        task.spawn(async move {
            started_tx.send(()).expect("test receiver");
            control.acquire().await
        });
        started_rx.await.expect("replacement task started");
        tokio::task::yield_now().await;
        assert!(
            task.try_join_next().is_none(),
            "replacement waits at full reserved capacity"
        );

        drop(first);
        let replacement = task
            .join_next()
            .await
            .expect("replacement result")
            .expect("replacement task")
            .expect("refunded slot admits replacement");
        assert_eq!(execution.snapshot().reserved_calls, 1);
        replacement.mark_dispatched().expect("commit replacement");
        let snapshot = execution.snapshot();
        assert_eq!(snapshot.committed_calls, 1);
        assert_eq!(snapshot.reserved_calls, 0);
    }

    #[tokio::test]
    async fn a_waiting_reservation_expires_at_the_soft_deadline() {
        let started_at = Instant::now();
        let execution = Arc::new(ReviewExecution::new(options(
            ReviewMode::Large,
            Some(1),
            started_at,
            started_at + Duration::from_millis(1_250),
        )));
        let control: Arc<dyn ModelExecutionControl> = execution.clone();
        let held = control.acquire().await.expect("first reservation");
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let mut waiting = tokio::task::JoinSet::new();
        waiting.spawn(async move {
            started_tx.send(()).expect("test receiver");
            control.acquire().await
        });
        started_rx.await.expect("waiting acquire started");
        let pending = tokio::time::timeout(Duration::from_millis(25), waiting.join_next()).await;
        assert!(
            pending.is_err(),
            "the second acquisition waits while the reservation is held"
        );

        let result = tokio::time::timeout(Duration::from_secs(3), waiting.join_next())
            .await
            .expect("deadline wakes a capacity waiter")
            .expect("waiting task")
            .expect("waiting acquire task");
        assert!(matches!(result, Err(ModelExecutionRefusal::SoftDeadline)));
        assert_eq!(execution.snapshot().reserved_calls, 1);
        assert_eq!(execution.snapshot().committed_calls, 0);
        drop(held);
    }

    #[tokio::test]
    async fn a_permit_crossing_the_absolute_soft_deadline_cannot_commit() {
        let started_at = Instant::now();
        let crossed_deadline = Arc::new(AtomicBool::new(false));
        let clock_flag = Arc::clone(&crossed_deadline);
        let now: Arc<dyn Fn() -> Instant + Send + Sync> = Arc::new(move || {
            if clock_flag.load(Ordering::Acquire) {
                started_at + Duration::from_secs(81)
            } else {
                started_at
            }
        });
        let execution = ReviewExecution::with_clock(
            options(
                ReviewMode::Large,
                None,
                started_at,
                started_at + Duration::from_secs(100),
            ),
            now,
        );
        let permit = execution.acquire().await.expect("pre-deadline reservation");
        assert_eq!(execution.snapshot().reserved_calls, 1);

        crossed_deadline.store(true, Ordering::Release);
        assert_eq!(
            permit.mark_dispatched(),
            Err(ModelExecutionRefusal::SoftDeadline)
        );
        assert_eq!(
            permit.mark_dispatched(),
            Err(ModelExecutionRefusal::SoftDeadline)
        );
        let snapshot = execution.snapshot();
        assert_eq!(snapshot.committed_calls, 0);
        assert_eq!(snapshot.reserved_calls, 1);
        drop(permit);
        assert_eq!(execution.snapshot().reserved_calls, 0);
    }

    #[tokio::test]
    async fn committing_a_permit_twice_counts_one_dispatch() {
        let started_at = Instant::now();
        let execution = ReviewExecution::new(options(
            ReviewMode::Large,
            Some(1),
            started_at,
            started_at + Duration::from_secs(100),
        ));
        let permit = execution.acquire().await.expect("reservation");

        permit.mark_dispatched().expect("first commit");
        permit.mark_dispatched().expect("idempotent commit");

        assert_eq!(execution.snapshot().committed_calls, 1);
    }

    #[tokio::test]
    async fn a_physical_retry_cannot_exceed_the_review_call_limit() {
        let started_at = Instant::now();
        let execution = ReviewExecution::new(options(
            ReviewMode::Large,
            Some(1),
            started_at,
            started_at + Duration::from_secs(100),
        ));
        let permit = execution.acquire().await.expect("reservation");

        permit.mark_dispatched().expect("commit first send");
        permit
            .mark_physical_attempt()
            .expect("first physical send uses its committed slot");
        assert_eq!(execution.snapshot().committed_calls, 1);

        assert_eq!(
            permit.mark_physical_attempt(),
            Err(ModelExecutionRefusal::CallLimit),
            "a second physical send on the same permit needs another slot"
        );
        assert_eq!(execution.snapshot().committed_calls, 1);
    }

    #[tokio::test]
    async fn a_physical_retry_preserves_capacity_reserved_for_another_call() {
        let started_at = Instant::now();
        let execution = ReviewExecution::new(options(
            ReviewMode::Large,
            Some(2),
            started_at,
            started_at + Duration::from_secs(100),
        ));
        let first = execution.acquire().await.expect("first reservation");
        let pending = execution.acquire().await.expect("pending reservation");
        first.mark_dispatched().expect("commit first call");
        first
            .mark_physical_attempt()
            .expect("first physical send uses its committed slot");

        assert_eq!(
            first.mark_physical_attempt(),
            Err(ModelExecutionRefusal::CallLimit),
            "an extra send cannot consume a slot already reserved for another call"
        );
        let during_retry = execution.snapshot();
        assert_eq!(during_retry.committed_calls, 1);
        assert_eq!(during_retry.reserved_calls, 1);

        pending
            .mark_dispatched()
            .expect("the pending call still owns the second slot");
        let after_pending_commit = execution.snapshot();
        assert_eq!(after_pending_commit.committed_calls, 2);
        assert_eq!(after_pending_commit.reserved_calls, 0);
    }

    #[tokio::test]
    async fn physical_attempts_from_shared_permits_compete_for_one_extra_slot() {
        let started_at = Instant::now();
        let execution = ReviewExecution::new(options(
            ReviewMode::Large,
            Some(3),
            started_at,
            started_at + Duration::from_secs(100),
        ));
        let first = execution.acquire().await.expect("first reservation");
        let second = execution.acquire().await.expect("second reservation");
        first.mark_dispatched().expect("commit first logical call");
        second
            .mark_dispatched()
            .expect("commit second logical call");
        first
            .mark_physical_attempt()
            .expect("first call's initial send is covered");
        second
            .mark_physical_attempt()
            .expect("second call's initial send is covered");
        assert_eq!(execution.snapshot().committed_calls, 2);

        let first: Arc<dyn ModelExecutionPermit> = Arc::from(first);
        let second: Arc<dyn ModelExecutionPermit> = Arc::from(second);
        let barrier = Arc::new(std::sync::Barrier::new(3));
        let first_barrier = Arc::clone(&barrier);
        let first_retry = std::thread::spawn(move || {
            first_barrier.wait();
            first.mark_physical_attempt()
        });
        let second_barrier = Arc::clone(&barrier);
        let second_retry = std::thread::spawn(move || {
            second_barrier.wait();
            second.mark_physical_attempt()
        });
        barrier.wait();

        let results = [
            first_retry.join().expect("first retry thread"),
            second_retry.join().expect("second retry thread"),
        ];
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        assert_eq!(
            results
                .iter()
                .filter(|result| **result == Err(ModelExecutionRefusal::CallLimit))
                .count(),
            1
        );
        assert_eq!(execution.snapshot().committed_calls, 3);
    }

    #[tokio::test]
    async fn a_physical_retry_rechecks_cancellation_after_the_first_send() {
        let started_at = Instant::now();
        let execution = ReviewExecution::new(options(
            ReviewMode::Large,
            Some(2),
            started_at,
            started_at + Duration::from_secs(100),
        ));
        let permit = execution.acquire().await.expect("reservation");
        permit.mark_dispatched().expect("commit first send");
        permit
            .mark_physical_attempt()
            .expect("first physical send is admitted");

        execution.cancel();
        assert_eq!(
            permit.mark_physical_attempt(),
            Err(ModelExecutionRefusal::Cancelled)
        );
        assert_eq!(execution.snapshot().committed_calls, 1);
    }

    #[tokio::test]
    async fn a_committed_permit_rechecks_the_soft_deadline_before_a_later_send() {
        let started_at = Instant::now();
        let crossed_deadline = Arc::new(AtomicBool::new(false));
        let clock_flag = Arc::clone(&crossed_deadline);
        let now: Arc<dyn Fn() -> Instant + Send + Sync> = Arc::new(move || {
            if clock_flag.load(Ordering::Acquire) {
                started_at + Duration::from_secs(81)
            } else {
                started_at
            }
        });
        let execution = ReviewExecution::with_clock(
            options(
                ReviewMode::Large,
                Some(1),
                started_at,
                started_at + Duration::from_secs(100),
            ),
            now,
        );
        let permit = execution.acquire().await.expect("reservation");

        permit.mark_dispatched().expect("first physical send");
        permit
            .mark_physical_attempt()
            .expect("first physical-attempt admission");
        crossed_deadline.store(true, Ordering::Release);

        assert_eq!(
            permit.mark_physical_attempt(),
            Err(ModelExecutionRefusal::SoftDeadline)
        );
        assert_eq!(
            permit.mark_dispatched(),
            Err(ModelExecutionRefusal::SoftDeadline)
        );
        assert_eq!(execution.snapshot().committed_calls, 1);
    }

    #[tokio::test]
    async fn cancellation_refuses_an_uncommitted_permit_and_drop_refunds_it() {
        let started_at = Instant::now();
        let execution = ReviewExecution::new(options(
            ReviewMode::Large,
            Some(1),
            started_at,
            started_at + Duration::from_secs(100),
        ));
        let permit = execution.acquire().await.expect("reserve one dispatch");
        execution.cancel();

        let result = permit.mark_dispatched();
        let repeated_result = permit.mark_dispatched();
        drop(permit);
        let snapshot = execution.snapshot();
        assert_eq!(result, Err(ModelExecutionRefusal::Cancelled));
        assert_eq!(repeated_result, Err(ModelExecutionRefusal::Cancelled));
        assert_eq!(snapshot.committed_calls, 0);
        assert_eq!(snapshot.reserved_calls, 0);
    }

    #[tokio::test]
    async fn cancellation_wakes_a_waiter_blocked_by_uncommitted_capacity() {
        let started_at = Instant::now();
        let execution = Arc::new(ReviewExecution::new(options(
            ReviewMode::Large,
            Some(1),
            started_at,
            started_at + Duration::from_secs(100),
        )));
        let control: Arc<dyn ModelExecutionControl> = execution.clone();
        let held = control.acquire().await.expect("hold the reserved slot");
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let mut waiting = tokio::task::JoinSet::new();
        let waiting_control = Arc::clone(&control);
        waiting.spawn(async move {
            use std::future::Future;

            let mut started_tx = Some(started_tx);
            let mut acquire = Box::pin(waiting_control.acquire());
            std::future::poll_fn(|context| {
                let result = acquire.as_mut().poll(context);
                if result.is_pending()
                    && let Some(started_tx) = started_tx.take()
                {
                    started_tx
                        .send(())
                        .expect("acquire was polled while capacity was held");
                }
                result
            })
            .await
        });
        tokio::time::timeout(Duration::from_secs(1), started_rx)
            .await
            .expect("waiter polls acquire under the held reservation")
            .expect("waiter reached its acquire call");
        assert!(
            tokio::time::timeout(Duration::from_millis(25), waiting.join_next())
                .await
                .is_err(),
            "the second reservation is blocked by the held slot"
        );

        execution.cancel();
        let result = tokio::time::timeout(Duration::from_secs(1), waiting.join_next()).await;
        let outcome = match result {
            Ok(Some(Ok(outcome))) => outcome,
            _ => {
                waiting.abort_all();
                waiting.shutdown().await;
                drop(held);
                panic!("cancellation did not settle the blocked acquisition");
            }
        };
        drop(held);

        assert_eq!(outcome.err(), Some(ModelExecutionRefusal::Cancelled));
        let snapshot = execution.snapshot();
        assert_eq!(snapshot.committed_calls, 0);
        assert_eq!(snapshot.reserved_calls, 0);
    }

    #[tokio::test]
    async fn cancellation_after_commit_keeps_dispatch_spent_and_blocks_new_work() {
        let started_at = Instant::now();
        let execution = ReviewExecution::new(options(
            ReviewMode::Large,
            Some(2),
            started_at,
            started_at + Duration::from_secs(100),
        ));
        let permit = execution.acquire().await.expect("reserve a dispatch");
        permit
            .mark_dispatched()
            .expect("commit before cancellation");

        execution.cancel();
        assert_eq!(
            permit.mark_dispatched(),
            Err(ModelExecutionRefusal::Cancelled),
            "a subsequent physical send must observe cancellation"
        );
        drop(permit);

        assert_eq!(
            execution.acquire().await.err(),
            Some(ModelExecutionRefusal::Cancelled)
        );
        let snapshot = execution.snapshot();
        assert_eq!(snapshot.committed_calls, 1);
        assert_eq!(snapshot.reserved_calls, 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_commits_of_one_permit_count_one_dispatch() {
        let started_at = Instant::now();
        let execution = ReviewExecution::new(options(
            ReviewMode::Large,
            Some(4),
            started_at,
            started_at + Duration::from_secs(100),
        ));
        let permit: Arc<dyn ModelExecutionPermit> =
            Arc::from(execution.acquire().await.expect("one shared reservation"));
        let workers = 16;
        let barrier = Arc::new(tokio::sync::Barrier::new(workers + 1));
        let mut tasks = tokio::task::JoinSet::new();
        for _ in 0..workers {
            let permit = Arc::clone(&permit);
            let barrier = Arc::clone(&barrier);
            tasks.spawn(async move {
                barrier.wait().await;
                permit.mark_dispatched()
            });
        }
        barrier.wait().await;

        while let Some(result) = tasks.join_next().await {
            result.expect("commit task").expect("commit succeeds");
        }
        let snapshot = execution.snapshot();
        assert_eq!(snapshot.committed_calls, 1);
        assert_eq!(snapshot.reserved_calls, 0);
    }

    #[tokio::test]
    async fn execution_clones_share_the_same_reservation_and_usage_ledger() {
        let started_at = Instant::now();
        let execution = ReviewExecution::new(options(
            ReviewMode::Large,
            Some(4),
            started_at,
            started_at + Duration::from_secs(100),
        ));
        let clone = execution.clone();
        let permit = execution.acquire().await.expect("reservation");

        assert_eq!(clone.snapshot().reserved_calls, 1);
        permit.mark_dispatched().expect("commit");
        clone.on_terminal(ModelExecutionTerminal {
            requested_model: "model-b".into(),
            answered_model: Some("model-b".into()),
            outcome: ModelExecutionTerminalOutcome::Succeeded,
            refused: false,
            usage: Some(crate::ports::model_execution::ModelExecutionTerminalUsage {
                input_tokens: 17,
                output_tokens: 0,
                reasoning_tokens: 0,
                cached_tokens: 0,
                cost_usd: Some(0.0),
            }),
        });

        let snapshot = execution.snapshot();
        assert_eq!(snapshot.committed_calls, 1);
        assert_eq!(snapshot.physical_usage.calls_attempted, 1);
        assert_eq!(snapshot.physical_usage.input_tokens, 17);
        assert_eq!(snapshot.physical_usage_by_model["model-b"].input_tokens, 17);
        assert_eq!(snapshot.physical_terminal_count, 1);
        assert!(snapshot.physical_accounting_complete);
    }

    #[tokio::test]
    async fn terminal_accounting_keeps_known_costs_when_other_reports_are_incomplete() {
        let started_at = Instant::now();
        let execution = ReviewExecution::new(options(
            ReviewMode::Large,
            Some(4),
            started_at,
            started_at + Duration::from_secs(100),
        ));
        for _ in 0..4 {
            execution
                .acquire()
                .await
                .expect("reserve one terminal event")
                .mark_dispatched()
                .expect("commit one terminal event");
        }

        execution.on_terminal(ModelExecutionTerminal {
            requested_model: "configured-primary".into(),
            answered_model: Some("actual-provider-a".into()),
            outcome: ModelExecutionTerminalOutcome::Succeeded,
            refused: true,
            usage: Some(crate::ports::model_execution::ModelExecutionTerminalUsage {
                input_tokens: 5,
                output_tokens: 2,
                reasoning_tokens: 1,
                cached_tokens: 1,
                cost_usd: Some(0.125),
            }),
        });
        execution.on_terminal(ModelExecutionTerminal {
            requested_model: "configured-secondary".into(),
            answered_model: Some("actual-provider-b".into()),
            outcome: ModelExecutionTerminalOutcome::Succeeded,
            refused: false,
            usage: None,
        });
        execution.on_terminal(ModelExecutionTerminal {
            requested_model: "configured-third".into(),
            answered_model: None,
            outcome: ModelExecutionTerminalOutcome::Succeeded,
            refused: false,
            usage: Some(crate::ports::model_execution::ModelExecutionTerminalUsage {
                input_tokens: 7,
                output_tokens: 3,
                reasoning_tokens: 0,
                cached_tokens: 0,
                cost_usd: None,
            }),
        });
        execution.on_terminal(ModelExecutionTerminal {
            requested_model: "configured-fourth".into(),
            answered_model: Some("actual-provider-d".into()),
            outcome: ModelExecutionTerminalOutcome::Succeeded,
            refused: false,
            usage: Some(crate::ports::model_execution::ModelExecutionTerminalUsage {
                input_tokens: 1,
                output_tokens: 1,
                reasoning_tokens: 0,
                cached_tokens: 0,
                cost_usd: Some(f64::NAN),
            }),
        });

        let snapshot = execution.snapshot();
        assert_eq!(snapshot.physical_terminal_count, 4);
        assert_eq!(snapshot.physical_usage.calls_attempted, 4);
        assert_eq!(snapshot.physical_usage.calls_succeeded, 4);
        assert_eq!(snapshot.physical_usage.calls_refused, 1);
        assert_eq!(snapshot.physical_usage.calls_errored, 0);
        assert_eq!(snapshot.physical_usage.input_tokens, 13);
        assert_eq!(snapshot.physical_usage.output_tokens, 6);
        assert_eq!(snapshot.physical_usage.cost_usd, 0.125);
        assert_eq!(
            snapshot.physical_usage_by_model["actual-provider-a"].cost_usd,
            0.125
        );
        assert_eq!(
            snapshot.physical_usage_by_model["configured-third"].input_tokens, 7,
            "when the provider omits its answering id, use the requested model"
        );
        assert!(!snapshot.physical_accounting_complete);
    }

    #[tokio::test]
    async fn committed_dispatch_without_terminal_makes_known_sibling_incomplete() {
        let started_at = Instant::now();
        let execution = ReviewExecution::new(options(
            ReviewMode::Large,
            Some(2),
            started_at,
            started_at + Duration::from_secs(100),
        ));
        for _ in 0..2 {
            execution
                .acquire()
                .await
                .expect("reserve a dispatch")
                .mark_dispatched()
                .expect("commit a dispatch");
        }
        execution.on_terminal(ModelExecutionTerminal {
            requested_model: "primary".into(),
            answered_model: Some("actual-primary".into()),
            outcome: ModelExecutionTerminalOutcome::Succeeded,
            refused: false,
            usage: Some(crate::ports::model_execution::ModelExecutionTerminalUsage {
                input_tokens: 8,
                output_tokens: 3,
                reasoning_tokens: 0,
                cached_tokens: 0,
                cost_usd: Some(0.25),
            }),
        });

        let snapshot = execution.snapshot();
        assert_eq!(snapshot.committed_calls, 2);
        assert_eq!(snapshot.physical_terminal_count, 1);
        assert_eq!(snapshot.physical_usage.cost_usd, 0.25);
        assert!(!snapshot.physical_accounting_complete);
    }

    #[tokio::test]
    async fn terminal_statuses_keep_refusal_failure_and_cancellation_counts_distinct() {
        let started_at = Instant::now();
        let execution = ReviewExecution::new(options(
            ReviewMode::Large,
            Some(3),
            started_at,
            started_at + Duration::from_secs(100),
        ));
        for _ in 0..3 {
            execution
                .acquire()
                .await
                .expect("reserve one dispatch")
                .mark_dispatched()
                .expect("commit one dispatch");
        }
        for outcome in [
            ModelExecutionTerminalOutcome::Refused,
            ModelExecutionTerminalOutcome::Failed,
            ModelExecutionTerminalOutcome::Cancelled,
        ] {
            execution.on_terminal(ModelExecutionTerminal {
                requested_model: "model-b".into(),
                answered_model: Some("actual-b".into()),
                outcome,
                refused: false,
                usage: Some(crate::ports::model_execution::ModelExecutionTerminalUsage {
                    input_tokens: 0,
                    output_tokens: 0,
                    reasoning_tokens: 0,
                    cached_tokens: 0,
                    cost_usd: Some(0.0),
                }),
            });
        }

        let snapshot = execution.snapshot();
        assert_eq!(snapshot.physical_terminal_count, 3);
        assert_eq!(snapshot.physical_usage.calls_attempted, 3);
        assert_eq!(snapshot.physical_usage.calls_succeeded, 0);
        assert_eq!(snapshot.physical_usage.calls_refused, 1);
        assert_eq!(snapshot.physical_usage.calls_errored, 3);
        assert!(snapshot.physical_accounting_complete);
    }

    #[tokio::test]
    async fn saturated_terminal_token_totals_are_marked_incomplete() {
        let started_at = Instant::now();
        let execution = ReviewExecution::new(options(
            ReviewMode::Large,
            Some(2),
            started_at,
            started_at + Duration::from_secs(100),
        ));
        for _ in 0..2 {
            execution
                .acquire()
                .await
                .expect("reserve one dispatch")
                .mark_dispatched()
                .expect("commit one dispatch");
        }
        for input_tokens in [u64::MAX, 1] {
            execution.on_terminal(ModelExecutionTerminal {
                requested_model: "model-b".into(),
                answered_model: Some("actual-b".into()),
                outcome: ModelExecutionTerminalOutcome::Succeeded,
                refused: false,
                usage: Some(crate::ports::model_execution::ModelExecutionTerminalUsage {
                    input_tokens,
                    output_tokens: 0,
                    reasoning_tokens: 0,
                    cached_tokens: 0,
                    cost_usd: Some(0.0),
                }),
            });
        }

        let snapshot = execution.snapshot();
        assert_eq!(snapshot.physical_usage.input_tokens, u64::MAX);
        assert!(!snapshot.physical_accounting_complete);
    }

    #[tokio::test]
    async fn logical_mock_usage_does_not_create_a_physical_terminal() {
        let started_at = Instant::now();
        let execution = Arc::new(ReviewExecution::new(options(
            ReviewMode::Normal,
            None,
            started_at,
            started_at + Duration::from_secs(100),
        )));
        let mock: Arc<dyn Model> = Arc::new(crate::harness::mock::MockModel::silent());
        let model = mock
            .with_execution_control(execution.clone())
            .expect("MockModel accepts the offline execution control");
        model
            .complete(crate::ports::model::ModelRequest {
                model: "mock".into(),
                messages: vec![
                    crate::ports::model::Message::system("Rules."),
                    crate::ports::model::Message::user("Diff."),
                ],
                schema: serde_json::json!({"type":"object"}),
                schema_name: "review".into(),
                reasoning_effort: None,
                max_tokens: 16,
            })
            .await
            .expect("the canned response succeeds");

        assert!(!model.complete_reports_physical_calls());
        let snapshot = execution.snapshot();
        assert_eq!(snapshot.committed_calls, 1);
        assert_eq!(snapshot.physical_terminal_count, 0);
        assert_eq!(snapshot.physical_usage.calls_attempted, 0);
        assert!(!snapshot.physical_accounting_complete);
    }

    #[test]
    fn execution_options_keep_the_mode_call_cap_and_drain_policy_together() {
        let started_at = Instant::now();
        let execution = ReviewExecution::new(options(
            ReviewMode::Large,
            Some(48),
            started_at,
            started_at + Duration::from_secs(900),
        ));

        assert_eq!(execution.mode(), ReviewMode::Large);
        assert_eq!(execution.max_calls(), Some(48));
        assert_eq!(execution.drain_seconds(), 30);
        assert_eq!(
            execution.soft_deadline(),
            started_at + Duration::from_secs(720)
        );
    }

    #[tokio::test]
    async fn tightening_a_normal_scope_refuses_a_held_reservation_at_the_new_cap() {
        let started_at = Instant::now();
        let execution = Arc::new(ReviewExecution::new(options(
            ReviewMode::Normal,
            None,
            started_at,
            started_at + Duration::from_secs(100),
        )));
        let control: Arc<dyn ModelExecutionControl> = execution.clone();
        let spent = control.acquire().await.expect("normal scope is uncapped");
        spent.mark_dispatched().expect("first normal dispatch");
        let held = control.acquire().await.expect("second normal reservation");

        execution.restrict_to_large(1);

        assert_eq!(
            held.mark_dispatched(),
            Err(ModelExecutionRefusal::CallLimit)
        );
        assert_eq!(
            held.mark_dispatched(),
            Err(ModelExecutionRefusal::CallLimit),
            "refusal classification remains stable on repeat"
        );
        let snapshot = execution.snapshot();
        assert_eq!(snapshot.committed_calls, 1);
        assert_eq!(snapshot.reserved_calls, 1);

        drop(held);
        let snapshot = execution.snapshot();
        assert_eq!(snapshot.committed_calls, 1);
        assert_eq!(snapshot.reserved_calls, 0);
    }

    #[tokio::test]
    async fn a_large_cap_tightens_a_spent_normal_scope_and_never_later_grows() {
        let started_at = Instant::now();
        let execution = ReviewExecution::new(options(
            ReviewMode::Normal,
            None,
            started_at,
            started_at + Duration::from_secs(100),
        ));
        for _ in 0..2 {
            let permit = execution.acquire().await.expect("normal scope is uncapped");
            permit.mark_dispatched().expect("normal dispatch");
        }

        execution.restrict_to_large(2);
        assert_eq!(
            execution.acquire().await.err(),
            Some(ModelExecutionRefusal::CallLimit)
        );

        execution.restrict_to_large(48);
        assert_eq!(execution.max_calls(), Some(2));
        assert_eq!(
            execution.acquire().await.err(),
            Some(ModelExecutionRefusal::CallLimit),
            "a later, larger Large-mode policy cannot refund or raise the cap"
        );
        assert_eq!(execution.snapshot().committed_calls, 2);
    }

    #[tokio::test]
    async fn normal_scope_without_large_restriction_remains_uncapped() {
        let started_at = Instant::now();
        let execution = ReviewExecution::new(options(
            ReviewMode::Normal,
            None,
            started_at,
            started_at + Duration::from_secs(100),
        ));

        for _ in 0..4 {
            let permit = execution.acquire().await.expect("normal scope is uncapped");
            permit.mark_dispatched().expect("normal dispatch");
        }

        assert_eq!(execution.max_calls(), None);
        assert_eq!(execution.snapshot().committed_calls, 4);
    }

    #[test]
    fn opaque_legacy_models_decline_the_optional_execution_control() {
        let started_at = Instant::now();
        let execution = ReviewExecution::new(options(
            ReviewMode::Normal,
            None,
            started_at,
            started_at + Duration::from_secs(900),
        ));
        let model: Arc<dyn Model> =
            Arc::new(LegacyOpaqueMockModel(crate::harness::mock::MockModel::new()));
        let control: Arc<dyn ModelExecutionControl> = Arc::new(execution);

        assert!(model.with_execution_control(control).is_none());
    }
}
