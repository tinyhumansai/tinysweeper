//! The review-state port: what tinysweeper remembers between two pushes.
//!
//! Everything here is an *optimisation*, and the review path must work without
//! it. The authoritative record of what has already been said lives on GitHub,
//! in the `tinysweeper:fp=` markers on the comments themselves — that is what
//! keeps dedupe correct for `local-review`, for a fresh deployment, and for a
//! database that has been wiped. What a store adds is the one thing the markers
//! cannot carry: the exact bytes of the evidence already reviewed, which prompt
//! layer 3 has to replay verbatim to earn a cache hit.
//!
//! So a missing store costs money, never correctness.

use async_trait::async_trait;

use crate::error::Result;
use crate::lanes::e2e::runs::Watch;
use crate::state::types::ReviewedState;

/// Somewhere durable to keep the last review of a pull request.
#[async_trait]
pub trait ReviewStateStore: Send + Sync {
    /// What was last reviewed under `key`, if anything.
    async fn load_state(&self, key: &str) -> Result<Option<ReviewedState>>;

    /// Record what has now been reviewed under `key`.
    async fn save_state(&self, key: &str, state: &ReviewedState) -> Result<()>;

    /// Clear the `e2e` watch under `key`, but only if it is still exactly
    /// `watch` — every field, not only `head_sha`.
    ///
    /// The compare-and-clear `settle_e2e` needs and `load_state` +
    /// `save_state` cannot give it: a plain reload-then-save still has a
    /// window between the two calls in which a new review can overwrite the
    /// whole record. Comparing the full watch, not just its `head_sha`,
    /// matters for the same reason: a manual re-review of the *same* commit
    /// (the `/admin/reviews` route reviews a head again on request) can save
    /// a replacement watch with the same `head_sha` but different `jobs`,
    /// `summary` or `failed` before this clears — matching on `head_sha`
    /// alone would clear that newer watch too, leaving its freshly
    /// published `Neutral` check with nothing to settle it. Returns whether
    /// anything was cleared — `false` when there was no record, no watch, or
    /// the watch does not match, all of which mean nothing here needed
    /// clearing.
    async fn clear_e2e_watch(&self, key: &str, watch: &Watch) -> Result<bool>;
}
