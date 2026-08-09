//! Evidence collection: turning a pull request into what a lane can reason about.
//!
//! Nothing here calls a model, and nothing here executes anything from the
//! repository under review. It parses diffs and reads files — `git` is the one
//! program it spawns, and [`git`] documents the two ways a diff could have run
//! something else and how both are closed.

pub mod diff;
pub mod git;
pub mod replay;

pub use crate::evidence::diff::{
    DiffLine, FileDiff, Hunk, LineKind, parse_changed_files, parse_file_patch,
};
