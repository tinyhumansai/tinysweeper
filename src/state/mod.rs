//! Review state that outlives a single run.
//!
//! Deliberately thin. GitHub holds the record of what has already been said —
//! the markers on the comments — and this holds only what a marker cannot: the
//! exact evidence bytes the last review sent, so the next one can replay them
//! and hit the prompt cache. Losing everything here makes re-reviews expensive
//! and leaves them correct.

pub mod memory;
pub mod types;

pub use crate::state::memory::MemoryState;
pub use crate::state::types::{ReviewedState, key};
