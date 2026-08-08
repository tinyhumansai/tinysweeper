//! Configuration types.
//!
//! These deserialize from the *merged* TOML table, not from a single file, so
//! every section is `#[serde(default)]` and every field has a built-in default
//! in `defaults.toml`. Deserialization is therefore infallible for shape;
//! everything else is caught by [`crate::config::validate`], which reports
//! every problem at once rather than stopping at the first.

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};

/// A lane: one agent, one narrow job, one GitHub check run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LaneId {
    /// Correctness of the diff, with surrounding code pulled in as context.
    Critique,
    /// Dependency changes, new network/exec sites, workflow permission widening.
    Security,
    /// Whether changed behaviour is covered and the assertions mean anything.
    Tests,
    /// Secrets, blobs and other dirt committed into the history.
    Commits,
    /// Whether the pull request body matches what the diff does.
    Description,
    /// The deterministic aggregate of every other lane.
    Gate,
}

impl LaneId {
    /// Every lane, in the order they are reported.
    pub const ALL: [LaneId; 6] = [
        LaneId::Critique,
        LaneId::Security,
        LaneId::Tests,
        LaneId::Commits,
        LaneId::Description,
        LaneId::Gate,
    ];

    /// The lane's stable id, as written in config and in check-run names.
    pub fn as_str(self) -> &'static str {
        match self {
            LaneId::Critique => "critique",
            LaneId::Security => "security",
            LaneId::Tests => "tests",
            LaneId::Commits => "commits",
            LaneId::Description => "description",
            LaneId::Gate => "gate",
        }
    }

    /// The GitHub check-run name this lane publishes under.
    pub fn check_name(self) -> String {
        format!("tinysweeper/{}", self.as_str())
    }

    /// Parse a lane id, or `None` if it names no lane.
    pub fn parse(value: &str) -> Option<Self> {
        LaneId::ALL
            .into_iter()
            .find(|lane| lane.as_str() == value.trim().to_ascii_lowercase())
    }
}

impl fmt::Display for LaneId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// How much a finding matters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    /// Worth knowing, not worth blocking on.
    Low,
    /// Should be addressed before merge.
    Medium,
    /// Blocks merge.
    High,
    /// Blocks merge and needs a human immediately.
    Critical,
}

impl Severity {
    /// Every severity, ascending.
    pub const ALL: [Severity; 4] = [
        Severity::Low,
        Severity::Medium,
        Severity::High,
        Severity::Critical,
    ];

    /// The severity's stable id, as written in config.
    pub fn as_str(self) -> &'static str {
        match self {
            Severity::Low => "low",
            Severity::Medium => "medium",
            Severity::High => "high",
            Severity::Critical => "critical",
        }
    }

    /// Parse a severity, or `None` if it names no severity.
    pub fn parse(value: &str) -> Option<Self> {
        Severity::ALL
            .into_iter()
            .find(|s| s.as_str() == value.trim().to_ascii_lowercase())
    }
}

impl fmt::Display for Severity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Which model a lane runs on: a named tier, or an explicit model id that
/// bypasses the tiers entirely.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ModelRef(pub String);

impl ModelRef {
    /// The two tier names that resolve against `[models]`.
    pub const TIERS: [&'static str; 2] = ["scan", "deep"];

    /// Whether this reference names a tier rather than a raw model id.
    pub fn is_tier(&self) -> bool {
        Self::TIERS.contains(&self.0.as_str())
    }
}

/// How merges are performed when auto-merge fires.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MergeMethod {
    /// Squash every commit into one.
    Squash,
    /// A merge commit.
    Merge,
    /// Rebase onto the base branch.
    Rebase,
}

impl MergeMethod {
    /// Every method, for error messages.
    pub const ALL: [MergeMethod; 3] = [
        MergeMethod::Squash,
        MergeMethod::Merge,
        MergeMethod::Rebase,
    ];

    /// The method's stable id, as written in config.
    pub fn as_str(self) -> &'static str {
        match self {
            MergeMethod::Squash => "squash",
            MergeMethod::Merge => "merge",
            MergeMethod::Rebase => "rebase",
        }
    }
}

/// The whole effective configuration, after merging defaults, preset and the
/// repository's own file.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Schema version. Bumped only for breaking changes.
    pub version: u32,
    /// The preset this config builds on, if any.
    pub preset: Option<String>,
    /// Review behaviour and the noise gates.
    pub review: Review,
    /// Which paths are reviewed at all.
    pub paths: Paths,
    /// Per-glob review rules injected into the relevant lane's prompt.
    pub path_instructions: Vec<PathInstruction>,
    /// Review-cache behaviour.
    pub cache: Cache,
    /// Labels that switch the bot off for a pull request.
    pub labels: Labels,
    /// The model gateway and the tiers.
    pub models: Models,
    /// Per-lane overrides, keyed by lane id.
    pub lanes: BTreeMap<String, Lane>,
    /// Auto-merge policy.
    pub automerge: AutoMerge,
    /// Issue triage. Scaffolded; not model-wired yet.
    pub issues: Issues,
    /// Sentry issue promotion. Scaffolded; not model-wired yet.
    pub sentry: Sentry,
}

/// Review behaviour and the gates that keep it quiet.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Review {
    /// Which lanes run. Order does not matter; lanes run concurrently.
    pub lanes: Vec<String>,
    /// The single noise dial: 1 chill, 2 default, 3 assertive.
    pub strictness: u8,
    /// Post findings at or above this severity; fold the rest into the summary.
    pub severity_gate: String,
    /// Drop findings the model is less sure about than this.
    pub confidence_min: f64,
    /// Hard cap on posted comments per pull request.
    pub max_comments: usize,
    /// Review only the commits added since the last reviewed SHA.
    pub incremental: bool,
    /// Review draft pull requests too.
    pub draft_prs: bool,
    /// Treat each changed path's ancestor `AGENTS.md` files as review policy.
    pub respect_agents_md: bool,
}

/// Which paths are reviewed at all.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Paths {
    /// Gitignore-style globs excluded from every lane.
    pub ignore: Vec<String>,
}

/// A review rule scoped to a glob.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PathInstruction {
    /// The glob these instructions apply to.
    pub glob: String,
    /// The instructions, injected verbatim into the lane prompt.
    pub instructions: String,
}

/// Review-cache behaviour. See `docs/modules/cache/README.md`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Cache {
    /// Whether any cache stage runs.
    pub enabled: bool,
    /// Reuse a review when the diff changed only in whitespace or comments.
    pub semantic: bool,
    /// Refuse to reuse a review older than this, however clean the hashes are.
    pub max_age_days: u32,
}

/// Labels that switch the bot off for a pull request. Checked before any model
/// call, so a label is a genuine kill switch and not just a filter on output.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Labels {
    /// Stop automating; a human is handling this one.
    pub human_review: String,
    /// Review only on explicit command, never automatically.
    pub manual_only: String,
}

/// The model gateway and the two tiers lanes select between.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Models {
    /// Informational name for the gateway, e.g. `openrouter`.
    pub gateway: String,
    /// The OpenAI-compatible base URL.
    pub base_url: String,
    /// Environment variable holding the API key. Never the key itself.
    pub api_key_env: String,
    /// The cheap, high-volume tier.
    pub scan: String,
    /// The expensive tier used for deep review.
    pub deep: String,
    /// Tried in order when the selected model fails.
    pub fallback: Vec<String>,
    /// Cap on tokens generated per model call.
    pub max_tokens: u32,
    /// Hard USD ceiling for a single pull request's review.
    pub budget_usd_per_pr: f64,
}

/// Per-lane overrides.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Lane {
    /// A tier name (`scan`, `deep`) or an explicit model id.
    pub model: Option<ModelRef>,
    /// Fail this lane's check run at or above this severity.
    pub fail_on: Option<String>,
    /// `commits` only: which secret rulepack to scan history with.
    pub secret_rulepack: Option<String>,
    /// `commits` only: flag any committed blob larger than this.
    pub max_blob_bytes: Option<u64>,
}

/// Auto-merge policy. Deterministic: no model output reaches this.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AutoMerge {
    /// Off unless a repository explicitly opts in.
    pub enabled: bool,
    /// Check runs that must be green.
    pub require_checks: Vec<String>,
    /// Human approvals required on top of the checks.
    pub require_approvals: u32,
    /// How to merge.
    pub method: String,
    /// Merge only pull requests carrying one of these labels.
    pub allow_labels: Vec<String>,
    /// Never merge a pull request carrying one of these.
    pub block_labels: Vec<String>,
}

/// Issue triage. Scaffolded in v1.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Issues {
    /// Whether issue triage runs.
    pub enabled: bool,
}

/// Sentry issue promotion. Scaffolded in v1.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Sentry {
    /// Whether Sentry promotion runs.
    pub enabled: bool,
}

impl Config {
    /// The lanes that will actually run, parsed and deduplicated.
    ///
    /// Unknown lane names are rejected by validation, so by the time anything
    /// calls this they cannot appear.
    pub fn enabled_lanes(&self) -> Vec<LaneId> {
        let mut lanes: Vec<LaneId> = self
            .review
            .lanes
            .iter()
            .filter_map(|name| LaneId::parse(name))
            .collect();
        lanes.sort_unstable();
        lanes.dedup();
        lanes
    }

    /// The severity at or above which findings are posted.
    pub fn severity_gate(&self) -> Severity {
        Severity::parse(&self.review.severity_gate).unwrap_or(Severity::Medium)
    }

    /// The lane-specific overrides for `lane`, if it has any.
    pub fn lane(&self, lane: LaneId) -> Option<&Lane> {
        self.lanes.get(lane.as_str())
    }

    /// Resolve a lane's model to a concrete model id.
    ///
    /// A lane may name a tier (`scan`, `deep`) or an explicit id. Lanes with no
    /// override run on the `scan` tier, because the expensive tier should be an
    /// opt-in.
    pub fn model_for(&self, lane: LaneId) -> &str {
        let reference = self.lane(lane).and_then(|l| l.model.as_ref());
        match reference.map(|r| r.0.as_str()) {
            Some("deep") => &self.models.deep,
            Some("scan") | None => &self.models.scan,
            Some(explicit) => explicit,
        }
    }

    /// The severity at which `lane` fails its check run.
    pub fn fail_on(&self, lane: LaneId) -> Severity {
        self.lane(lane)
            .and_then(|l| l.fail_on.as_deref())
            .and_then(Severity::parse)
            .unwrap_or(Severity::High)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lane_ids_round_trip_through_their_string_form() {
        for lane in LaneId::ALL {
            assert_eq!(LaneId::parse(lane.as_str()), Some(lane));
        }
        assert_eq!(LaneId::parse("nonsense"), None);
    }

    #[test]
    fn lane_parsing_is_forgiving_about_case_and_padding() {
        assert_eq!(LaneId::parse("  Critique "), Some(LaneId::Critique));
    }

    #[test]
    fn check_names_are_namespaced() {
        assert_eq!(LaneId::Gate.check_name(), "tinysweeper/gate");
    }

    #[test]
    fn severities_order_from_low_to_critical() {
        assert!(Severity::Low < Severity::Medium);
        assert!(Severity::Medium < Severity::High);
        assert!(Severity::High < Severity::Critical);
    }

    #[test]
    fn enabled_lanes_deduplicates_and_sorts() {
        let mut config = Config::default();
        config.review.lanes = vec![
            "security".into(),
            "critique".into(),
            "security".into(),
        ];
        assert_eq!(
            config.enabled_lanes(),
            vec![LaneId::Critique, LaneId::Security]
        );
    }

    #[test]
    fn a_lane_without_an_override_runs_on_the_cheap_tier() {
        let mut config = Config::default();
        config.models.scan = "cheap-model".into();
        config.models.deep = "expensive-model".into();
        assert_eq!(config.model_for(LaneId::Critique), "cheap-model");
    }

    #[test]
    fn a_lane_may_name_a_tier_or_an_explicit_model() {
        let mut config = Config::default();
        config.models.scan = "cheap-model".into();
        config.models.deep = "expensive-model".into();
        config.lanes.insert(
            "security".into(),
            Lane {
                model: Some(ModelRef("deep".into())),
                ..Lane::default()
            },
        );
        config.lanes.insert(
            "tests".into(),
            Lane {
                model: Some(ModelRef("vendor/some-other-model".into())),
                ..Lane::default()
            },
        );
        assert_eq!(config.model_for(LaneId::Security), "expensive-model");
        assert_eq!(config.model_for(LaneId::Tests), "vendor/some-other-model");
    }

    #[test]
    fn fail_on_defaults_to_high() {
        let config = Config::default();
        assert_eq!(config.fail_on(LaneId::Security), Severity::High);
    }
}
