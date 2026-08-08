//! Application-level entry points: the work behind each CLI subcommand.
//!
//! Kept out of `src/bin/tinysweeper.rs` so it is testable. The binary parses
//! arguments and nothing else.

pub mod apply;
pub mod doctor;
pub mod review;

pub use crate::app::apply::apply;
pub use crate::app::doctor::{check, doctor};
pub use crate::app::review::{Proposal, read_proposal, review, write_proposal};
