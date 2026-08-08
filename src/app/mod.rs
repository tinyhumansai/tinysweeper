//! Application-level entry points: the work behind each CLI subcommand.
//!
//! Kept out of `src/bin/tinysweeper.rs` so it is testable. The binary parses
//! arguments and nothing else.

pub mod doctor;

pub use crate::app::doctor::{check, doctor};
