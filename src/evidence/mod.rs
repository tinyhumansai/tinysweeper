//! Evidence collection: turning a pull request into what a lane can reason about.
//!
//! Nothing here calls a model, and nothing here executes anything from the
//! repository under review. It parses diffs and reads files.

pub mod diff;
pub mod replay;

pub use crate::evidence::diff::{
    DiffLine, FileDiff, Hunk, LineKind, parse_changed_files, parse_file_patch,
};
