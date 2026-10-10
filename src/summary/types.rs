//! Wire types for the generated narrative and deterministic review history.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::config::types::LaneId;

/// Generated and deterministic material carried from review to apply.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ReviewSummary {
    /// Short model-authored explanation shown above the fold.
    pub executive_summary: String,
    /// Model-authored behavioral explanation.
    pub changes: String,
    /// Cited feature descriptions.
    pub features: Vec<Feature>,
    /// Cited test-to-behavior mappings.
    pub tests: Vec<TestCoverage>,
    /// Positive observations, kept separate by lane.
    pub positive_observations: BTreeMap<LaneId, Vec<String>>,
    /// Deterministically classified changed files.
    pub surface: ChangeSurface,
    /// Recent review passes, oldest first.
    pub history: Vec<ReviewPass>,
    /// Whether the stored cache conversation had to restart at this pass.
    pub cache_chain_restarted: bool,
    /// Number of supported feature claims omitted by the configured limit.
    pub omitted_features: usize,
    /// Number of supported test claims omitted by the configured limit.
    pub omitted_tests: usize,
    /// Update time as seconds since the Unix epoch.
    pub updated_at_epoch: u64,
}

/// One generated behavior claim.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Feature {
    /// Addition, modification, removal, or internal refactor.
    pub kind: FeatureKind,
    /// Short behavior name.
    pub name: String,
    /// Observable impact.
    pub impact: String,
    /// Changed paths or known symbols supporting the claim.
    pub citations: Vec<String>,
}

/// How a behavior changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FeatureKind {
    /// New behavior.
    Addition,
    /// Existing behavior changed.
    Modification,
    /// Existing behavior removed.
    Removal,
    /// Behavior preserved while internals changed.
    InternalRefactor,
}

impl FeatureKind {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Addition => "Added",
            Self::Modification => "Modified",
            Self::Removal => "Removed",
            Self::InternalRefactor => "Internal refactor",
        }
    }
}

/// One generated test-to-behavior mapping.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TestCoverage {
    /// Unit, integration, end-to-end, regression, fixture, or infrastructure.
    pub kind: String,
    /// Behavior the test exercises.
    pub behavior: String,
    /// What the assertion establishes, or what remains uncovered.
    pub assessment: String,
    /// Changed paths or known symbols supporting the claim.
    pub citations: Vec<String>,
}

/// Counts that answer what kind of pull request this is.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangeSurface {
    /// Production-source files.
    pub production: usize,
    /// Test and fixture files.
    pub tests: usize,
    /// Documentation files.
    pub documentation: usize,
    /// Configuration and workflow files.
    pub configuration: usize,
}

/// One bounded review-pass history entry.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ReviewPass {
    /// Reviewed commit.
    pub head_sha: String,
    /// Stable state label.
    pub state: String,
    /// Concise deterministic account of the pass.
    pub summary: String,
    /// Pass time as seconds since the Unix epoch.
    pub reviewed_at_epoch: u64,
}

/// One exact user/assistant pair in the summary conversation.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SummaryTranscriptTurn {
    /// Reviewed commit associated with the evidence.
    pub head_sha: String,
    /// Exact fenced evidence message sent to the provider.
    pub evidence: String,
    /// Exact structured assistant value returned by the provider.
    pub assistant: String,
}
