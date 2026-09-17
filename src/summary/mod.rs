//! Structured generation and deterministic rendering for the durable PR review hub.

mod generate;
mod render;
mod types;

pub use generate::{deterministic, generate};
pub use render::{LEGACY_MARKER, MARKER, failed, in_progress, render};
pub use types::{
    ChangeSurface, Feature, FeatureKind, ReviewPass, ReviewSummary, SummaryTranscriptTurn,
    TestCoverage,
};
