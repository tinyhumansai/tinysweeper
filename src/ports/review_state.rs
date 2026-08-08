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
use crate::state::types::ReviewedState;

/// Somewhere durable to keep the last review of a pull request.
#[async_trait]
pub trait ReviewStateStore: Send + Sync {
    /// What was last reviewed under `key`, if anything.
    async fn load_state(&self, key: &str) -> Result<Option<ReviewedState>>;

    /// Record what has now been reviewed under `key`.
    async fn save_state(&self, key: &str, state: &ReviewedState) -> Result<()>;
}
