//! Deterministic scanners.
//!
//! These run *before* any model call, so a committed private key fails for free
//! and the model is only asked to adjudicate what a scanner already flagged.
//! They are cheap, certain, and offline.

pub mod blobs;
pub mod secrets;
pub mod types;
pub mod workflows;

pub use crate::scan::types::{Finding, ScanKind, redact};
