//! Review-scoped control over physical model dispatches.
//!
//! A model wrapper acquires before monetary waiting, then commits the permit
//! immediately before invoking the concrete provider. The same control object
//! must survive clones and budget re-scoping; wrappers do not create a fresh
//! review ceiling. Backends that cannot preserve the control keep the default
//! `None` compatibility path and cannot claim a physical-call guarantee.

use async_trait::async_trait;

/// Why a review-wide policy refused to start a physical model dispatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelExecutionRefusal {
    /// The review has exhausted its ceiling with committed physical dispatches.
    ///
    /// Acquisitions wait while the ceiling is occupied only by refundable
    /// reservations; a reservation becoming committed or being dropped wakes
    /// those waiters to re-evaluate the ceiling.
    CallLimit,
    /// The absolute review soft deadline has passed, either at acquisition or
    /// when a reserved permit is committed for dispatch.
    SoftDeadline,
    /// The original review drain window ended while an admitted model future
    /// was still in flight. This is a host-side timeout, not a provider event.
    DrainDeadline,
    /// The review's logical owner cancelled its execution scope.
    Cancelled,
}

/// Outcome of a physical provider dispatch, independent of any provider SDK.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelExecutionTerminalOutcome {
    /// The provider returned a response.
    Succeeded,
    /// The provider returned a refusal response.
    Refused,
    /// The provider call failed after dispatch began.
    Failed,
    /// The physical call was cancelled before returning a response.
    Cancelled,
}

/// Usage reported by the provider for one physical dispatch.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ModelExecutionTerminalUsage {
    /// Input tokens reported by the provider.
    pub input_tokens: u64,
    /// Output tokens reported by the provider.
    pub output_tokens: u64,
    /// Reasoning tokens reported by the provider.
    pub reasoning_tokens: u64,
    /// Cached input tokens reported by the provider.
    pub cached_tokens: u64,
    /// Provider-reported charge, absent when the provider did not report it.
    pub cost_usd: Option<f64>,
}

/// Raw terminal observation for one dispatched physical provider call.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelExecutionTerminal {
    /// Configured model requested for this physical dispatch.
    pub requested_model: String,
    /// Model that answered, when the provider identified it.
    pub answered_model: Option<String>,
    /// How the physical provider call ended.
    pub outcome: ModelExecutionTerminalOutcome,
    /// Whether the provider reported a refusal response.
    ///
    /// This is separate from `outcome`: a refusal response can still be a
    /// successfully completed provider dispatch.
    pub refused: bool,
    /// Raw provider usage, kept optional so an absent report stays unknown.
    pub usage: Option<ModelExecutionTerminalUsage>,
}

/// A review-scoped source of physical-dispatch permits.
#[async_trait]
pub trait ModelExecutionControl: Send + Sync {
    /// Reserve one dispatch slot before any potentially blocking money wait.
    ///
    /// If outstanding uncommitted reservations temporarily fill the ceiling,
    /// wait for a reservation to commit or be dropped instead of refusing the
    /// request. Return `CallLimit` once committed dispatches exhaust the
    /// ceiling, `SoftDeadline` once the absolute soft deadline has passed, or
    /// `Cancelled` when the review owner stops execution.
    async fn acquire(
        &self,
    ) -> std::result::Result<Box<dyn ModelExecutionPermit>, ModelExecutionRefusal>;

    /// Observe a physical dispatch terminal before any host-side price estimate.
    ///
    /// This callback is independent of review timing, must not run while a
    /// monetary-ledger lock is held, and is never called for a refusal before
    /// provider dispatch. Controls that do not collect raw usage may ignore it.
    fn on_terminal(&self, _terminal: ModelExecutionTerminal) {}
}

/// One reserved physical-dispatch slot.
pub trait ModelExecutionPermit: Send + Sync {
    /// Commit the slot immediately before provider invocation.
    ///
    /// Implementations make this idempotent. Dropping an uncommitted permit
    /// releases its reservation; dropping a committed permit never refunds it.
    /// Recheck the absolute soft deadline here because callers may wait for
    /// money after acquiring. A refusal leaves this permit uncommitted and does
    /// not consume a committed-call slot.
    fn mark_dispatched(&self) -> std::result::Result<(), ModelExecutionRefusal>;

    /// Admit one physical provider attempt made through this permit.
    ///
    /// The first attempt uses the slot committed by [`Self::mark_dispatched`];
    /// each later attempt is a distinct physical call and may be refused by
    /// the review-wide call limit. Compatibility controls that cannot observe
    /// physical attempts may retain the default no-op implementation.
    fn mark_physical_attempt(&self) -> std::result::Result<(), ModelExecutionRefusal> {
        Ok(())
    }
}
