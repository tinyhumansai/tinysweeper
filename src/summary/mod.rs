//! Structured generation and deterministic rendering for the durable PR review hub.

mod generate;
pub(crate) mod render;
mod types;

pub use generate::{
    SummaryGenerationInput, deterministic, deterministic_with_paths, generate, generate_with_input,
};
pub use render::{LEGACY_MARKER, MARKER, failed, in_progress, render};
pub use types::{
    ChangeSurface, Feature, FeatureKind, FileReviewCoverage, ReviewPass, ReviewSummary,
    SummaryDiffstat, SummaryTranscriptTurn, TestCoverage,
};
