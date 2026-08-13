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
}

impl LaneId {
    /// Every lane, in the order they are reported.
    pub const ALL: [LaneId; 5] = [
        LaneId::Critique,
        LaneId::Security,
        LaneId::Tests,
        LaneId::Commits,
        LaneId::Description,
    ];

    /// The lane's stable id, as written in config and in check-run names.
    pub fn as_str(self) -> &'static str {
        match self {
            LaneId::Critique => "critique",
            LaneId::Security => "security",
            LaneId::Tests => "tests",
            LaneId::Commits => "commits",
            LaneId::Description => "description",
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

/// What a strictness level means in gate terms.
///
/// The defaults are deliberately quiet. A reviewer that raises four things on
/// every pull request gets read for a week and ignored thereafter, and the
/// findings people actually want — a real bug, a leaked credential — are the
/// ones that get lost in that noise. Level 2 posts high-severity findings the
/// model is genuinely confident about, and folds everything else into the
/// summary where it costs nobody anything to skim.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Strictness {
    /// Minimum severity to post.
    pub severity: Severity,
    /// Minimum confidence to post.
    pub confidence: f64,
    /// A short description, for `doctor`.
    pub label: &'static str,
}

impl Strictness {
    /// The gates for `level`, clamped to the valid range.
    pub fn for_level(level: u8) -> Self {
        match level {
            0 | 1 => Self {
                severity: Severity::Critical,
                confidence: 0.85,
                label: "chill — only findings that are both severe and near-certain",
            },
            2 => Self {
                severity: Severity::High,
                confidence: 0.75,
                label: "default — high-severity findings the model is confident about",
            },
            _ => Self {
                severity: Severity::Medium,
                confidence: 0.55,
                label: "assertive — medium and above, including less certain calls",
            },
        }
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
    pub const ALL: [MergeMethod; 3] =
        [MergeMethod::Squash, MergeMethod::Merge, MergeMethod::Rebase];

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
    /// The knowledge centre: curated documents and rule extraction.
    pub knowledge: Knowledge,
    /// Which embedding provider fills and queries the code index.
    pub embeddings: Embeddings,
    /// How much repository context a review retrieves, and what it costs.
    pub retrieval: Retrieval,
    /// Per-lane overrides, keyed by lane id.
    pub lanes: BTreeMap<String, Lane>,
    /// Several reviewers on one lane's evidence.
    pub council: Council,
    /// Auto-merge policy.
    pub automerge: AutoMerge,
    /// Review-thread resolution.
    pub threads: Threads,
    /// The change-map comment.
    pub overview: Overview,
    /// Issue triage.
    pub issues: Issues,
    /// Scheduled repository automations.
    pub automation: Automation,
    /// Sentry issue promotion.
    pub sentry: Sentry,
}

/// Review behaviour and the gates that keep it quiet.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Review {
    /// Which lanes run. Order does not matter; lanes run concurrently.
    pub lanes: Vec<String>,
    /// The single noise dial: 1 chill, 2 default, 3 assertive.
    ///
    /// This actually sets the gates — see [`Config::severity_gate`] and
    /// [`Config::confidence_min`]. It was inert for a while: documented as the
    /// dial, validated for range, and read by nothing.
    pub strictness: u8,
    /// Post findings at or above this severity. Overrides what `strictness`
    /// would choose; leave it unset unless you need to.
    pub severity_gate: Option<String>,
    /// Drop findings the model is less sure about than this. Overrides what
    /// `strictness` would choose.
    pub confidence_min: Option<f64>,
    /// Hard cap on posted comments per pull request.
    pub max_comments: usize,
    /// Review only the commits added since the last reviewed SHA.
    pub incremental: bool,
    /// Review draft pull requests too.
    pub draft_prs: bool,
    /// Treat each changed path's ancestor `AGENTS.md` files as review policy.
    pub respect_agents_md: bool,
    /// Block the merge button when a finding reaches this severity.
    ///
    /// `"off"` never blocks. Anything else names the severity floor. Blocking
    /// is a real cost — a false positive now needs a human to clear it — which
    /// is why the floor is high and the setting is one word to turn off.
    pub request_changes_at: String,
    /// Submit an approving review when no lane blocks.
    ///
    /// This is what lets a pull request satisfy a "review required" branch
    /// protection rule without a human, and it is also what retires a previous
    /// changes-request: GitHub keeps one review per reviewer, so an approval
    /// supersedes the objection rather than leaving it for someone to dismiss
    /// by hand.
    ///
    /// Approving is a stronger act than staying quiet, so it is bounded twice
    /// over: the verdict is only ever `Approve` when every lane passed the gate,
    /// and a second approval is not submitted while the previous one still
    /// stands — otherwise every push would add a review nobody asked for.
    pub approve_when_clean: bool,
}

/// When tinysweeper closes its own review conversations.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Threads {
    /// Resolve a thread the review agent confirms the new code fixed.
    ///
    /// The agent receives the prior finding and current diff, but its verdict
    /// is advisory: deterministic code still limits the mutation to our own,
    /// outdated threads.
    pub resolve_fixed: bool,
    /// Ask a model whether a reply settled a finding the code did not change.
    ///
    /// **Off by default, and deliberately.** This is the one case no
    /// fingerprint can decide, so the only evidence is a comment somebody wrote
    /// — and a comment is untrusted input. The verdict stays advisory even when
    /// this is on: it feeds a plan that deterministic code executes, and it can
    /// only ever close a thread tinysweeper itself opened.
    pub ask_model: bool,
}

/// The change-flow comment: the diagram posted on a pull request.
///
/// Every ceiling here is a *legibility* budget, not a cost one — the map costs
/// nothing, because nothing in it comes from a model. Past a dozen boxes a
/// diagram stops being read at all, which is the failure these numbers exist to
/// prevent; whatever does not fit is folded away and counted, never dropped
/// silently.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Overview {
    /// Whether a pull request gets a change-flow comment at all.
    pub enabled: bool,
    /// How many changed behaviours to draw.
    pub max_components: usize,
    /// How many surrounding behaviours to draw beside them.
    pub max_impacted: usize,
    /// How many typed behaviour relationships to draw, heaviest first.
    pub max_links: usize,
    /// Retained for configuration compatibility; change flows list no paths.
    pub max_paths_per_component: usize,
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
    ///
    /// The table is **ordered and first match wins**: a path takes the rules of
    /// the first entry it matches and no other. That is what stops a Rust
    /// file's reviewer being handed the workflow rules — a token saving, and
    /// more importantly a precision one, because every rule a reviewer is shown
    /// is another thing it can find an opinion about.
    pub glob: String,
    /// The instructions, injected verbatim into the lane prompt.
    pub instructions: String,
    /// A rule document under `presets/rules/<name>.md`, appended to
    /// `instructions` when the config is loaded.
    ///
    /// Rule documents are **data**: adding one is a new file under `presets/`,
    /// never a new module. Keeping the long ones out of the TOML is what makes
    /// a rule's negative list — the "do NOT report" half that does most of the
    /// precision work — practical to write and to review.
    pub rules: Option<String>,
    /// The lanes this entry applies to. Empty means every lane.
    ///
    /// A rule document is only precision for the lane it was written for. Shown
    /// to the others it is pure cost: more prefix tokens on every call, and one
    /// more subject each of those reviewers can form an opinion about. Scoping
    /// is what makes a long document — a security taxonomy, say — affordable.
    pub lanes: Vec<LaneId>,
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

/// How a lane's structured answer is obtained from the model.
///
/// Not a performance dial. It selects *who enforces the schema*, and the two
/// answers are not equally strong — see each variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StructuredOutput {
    /// The provider is handed the JSON Schema and constrains generation to it.
    ///
    /// The strong form, and the default: a response that does not fit the
    /// schema is not generated in the first place.
    #[default]
    Schema,
    /// The provider guarantees only that the answer is *some* JSON object.
    ///
    /// The schema travels in the prompt instead, and enforcement moves to
    /// [`crate::harness::schema::parse`], which rejects a mismatch loudly. This
    /// is strictly weaker than [`StructuredOutput::Schema`] — the model can
    /// return well-formed JSON of the wrong shape, and only our own validation
    /// catches it. It exists because some models answer a strict schema request
    /// with a hard error rather than an answer: `deepseek-v4-pro-0813` returns
    /// "This response_format type is unavailable now", which turns every review
    /// into a fallback.
    JsonObject,
}

impl StructuredOutput {
    /// The mode's stable id, as written in config.
    pub fn as_str(self) -> &'static str {
        match self {
            StructuredOutput::Schema => "schema",
            StructuredOutput::JsonObject => "json_object",
        }
    }
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
    ///
    /// Reasoning is billed against this same ceiling, so a thinking-heavy model
    /// can spend the whole budget and return empty content — see
    /// `reasoning_effort`.
    pub max_tokens: u32,
    /// How hard the model should think: `off`, `low`, `medium`, `high`.
    ///
    /// `off` disables reasoning outright. It is the setting that rescues a
    /// deployment whose model reasons past `max_tokens` and answers with
    /// nothing, which is a real failure this repository has measured.
    pub reasoning_effort: String,
    /// Who enforces the response schema — see [`StructuredOutput`].
    ///
    /// Applies to the whole chain, `fallback` included, because a single review
    /// can be answered by any model in it and a prompt that carries its own
    /// schema has to be built before the answering model is known.
    pub structured_output: StructuredOutput,
    /// Hard USD ceiling for a single pull request's review.
    pub budget_usd_per_pr: f64,
}

/// The knowledge centre: curated documents, and rules read out of the
/// repository's own instruction files.
///
/// The caps here are budget dials. The *security* ceilings on extraction —
/// how many rules and how long each may be — are constants in
/// `crate::knowledge::types` and deliberately not configurable: a repository
/// that could raise them could remove them, and they are what bounds what a
/// hostile `AGENTS.md` can inject.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Knowledge {
    /// Run the sandboxed extraction pass over the files below.
    pub extract: bool,
    /// Instruction files to read, at the repository root.
    ///
    /// Plain filenames only — no path separators, no `..`. Anything else is
    /// dropped with a warning, because this list is editable by whoever opens
    /// a pull request and must not become a way to read arbitrary paths.
    pub files: Vec<String>,
    /// How much of one instruction file is sent to the extractor.
    pub max_file_bytes: usize,
    /// How many characters of one pinned document reach the prompt.
    pub pinned_doc_chars: usize,
    /// How many characters all pinned documents reach the prompt with,
    /// together. Zero disables pinned injection.
    pub pinned_total_chars: usize,
}

/// Which embedding provider fills and queries the code index.
///
/// A separate section from `[models]` rather than three more fields on it, and
/// the reason is that these values are not interchangeable with a completion
/// model's. `provider`, `model` and `dimensions` together *are* the index
/// partition key — [`EmbedSignature`](crate::index::types::EmbedSignature) —
/// so editing one of them does not reconfigure a call, it invalidates every
/// vector already written. Keeping them in their own block is what makes that
/// legible to whoever edits it.
///
/// Off by default. An embedding provider costs real money per indexed byte and
/// needs a key nobody has by accident, so a deployment that has not said
/// otherwise runs diff-only exactly as it did before this existed.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Embeddings {
    /// Whether an embedding provider is configured at all.
    ///
    /// `false` leaves the index cold, which every retrieval path already knows
    /// how to degrade through.
    pub enabled: bool,
    /// The provider, e.g. `voyage`. Also the first half of the signature.
    pub provider: String,
    /// The model id, e.g. `voyage-code-3`. The second half of the signature.
    pub model: String,
    /// Vector width. The third half of the signature, and it is carried
    /// separately because the search index declares it at creation time.
    pub dimensions: usize,
    /// Environment variable holding the provider's API key. Never the key.
    pub api_key_env: String,
    /// Override the provider's own API base URL. Empty means "the default",
    /// which is what every hosted deployment wants; it exists for a local
    /// OpenAI-compatible server.
    pub base_url: String,
    /// How many texts go in one embedding call.
    pub batch: usize,
    /// Client-side ceiling on provider requests per minute.
    ///
    /// Applied process-wide by the harness adapter. It is not politeness: a
    /// full index of a monorepo will otherwise walk straight into the
    /// provider's own limit and spend its time in backoff.
    pub requests_per_minute: u32,
    /// Hard USD ceiling for one indexing run of one repository.
    ///
    /// The counterpart to `models.budget_usd_per_pr`. Indexing a large
    /// repository is the single largest token count this program produces, so
    /// it gets a ceiling of its own rather than sharing the review's.
    pub budget_usd_per_index: f64,
}

impl Embeddings {
    /// The index partition key this configuration describes.
    ///
    /// Returns `None` when embeddings are off, which is the same thing every
    /// caller means by "there is no index to query".
    pub fn signature(&self) -> Option<crate::index::types::EmbedSignature> {
        self.enabled.then(|| {
            crate::index::types::EmbedSignature::new(
                self.provider.trim(),
                self.model.trim(),
                self.dimensions,
            )
        })
    }
}

/// What a review retrieves from the code index before it reads the diff.
///
/// Every dial here is a **budget**, and that is the point of the section
/// existing at all. Retrieval with no stated ceiling is the failure mode this
/// was written against: fifteen chunks of a few kilobytes look generous next to
/// a small pull request and are a rounding error next to a 300 KB diff, so the
/// retrieved context silently stops being context and becomes garnish. Stating
/// the budget in tokens means the share retrieval gets is a decision somebody
/// made rather than an accident of how big the diff happened to be.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Retrieval {
    /// Whether a review retrieves anything at all.
    ///
    /// Turning it off is a supported deployment, not a degradation: a review
    /// with no index behind it is exactly what tinysweeper did before this
    /// existed.
    pub enabled: bool,
    /// Character ceiling on the composed retrieval query.
    ///
    /// The query is built from the pull request rather than *being* the pull
    /// request, so embedding cost is constant no matter how large the diff is.
    pub query_chars: usize,
    /// Token ceiling on the retrieved context handed to one lane.
    pub context_tokens: usize,
    /// Hard cap on how many chunks reach the prompt, whatever the token budget
    /// would allow. A hundred two-line chunks are worse context than ten whole
    /// functions of the same size.
    pub max_chunks: usize,
    /// How many graph edges to walk out from the symbols the diff touches.
    ///
    /// Two, by default, and the count is not arbitrary: seeds are file nodes
    /// and the symbols named in hunk headings, so one hop reaches a changed
    /// file's own definitions and the second reaches whatever calls them —
    /// which is the caller a change breaks, the thing similarity search cannot
    /// find.
    pub graph_hops: u8,
    /// Hard ceiling on nodes returned by the walk, applied after it.
    pub max_graph_nodes: usize,
    /// How many dependents the blast-radius block may name.
    ///
    /// Separate from [`Retrieval::max_graph_nodes`] because it bounds a
    /// different thing: that cap is on the code a lane is *shown*, this one is
    /// on the list of names it is told depends on the change. A widely-imported
    /// file has more dependents than any reviewer can act on, and a hundred
    /// paths above a diff read as noise rather than as a warning. `0` turns the
    /// block off.
    pub max_impact: usize,
}

/// Model work that is not a lane.
///
/// Tiering is the one lever that changes what a review costs by an order of
/// magnitude, so it is stated as policy in one place rather than as a
/// `models.scan` reference scattered across call sites. Everything here is
/// mechanical — copying a span out of a diff, checking a claim against a single
/// document, pulling facts out of a document — and mechanical work does not get
/// the expensive tier. Reviewing does.
///
/// (open-code-review runs one model for everything. That is simpler and it is
/// the reason its cheap operations cost the same as its expensive ones.)
///
/// tinyagents ships `ModelRouter`/`WorkloadRoute` for the same idea, and it was
/// considered. It resolves aliases *inside* a tinyagents harness, complete with
/// capability gates and a `FallbackPolicy`; tinysweeper picks its tier before
/// the [`Model`](crate::ports::model::Model) port, in the default build, where
/// tinyagents is not linked at all. Adopting it would drag the harness into the
/// offline build to answer a question two string fields already answer. The
/// place it *would* pay for itself is `GatewayModel`'s hand-rolled fallback
/// chain, which is a separate change on the feature-gated side of the port.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Workload {
    /// Re-locating a quoted snippet in a diff (`src/position/relocate.rs`).
    Relocate,
    /// The falsification pass (`src/falsify`).
    Falsify,
    /// Extracting facts from a curated knowledge document.
    KnowledgeExtraction,
    /// Judging whether a reply settled a review thread (`src/threads`).
    ThreadReview,
}

/// Several reviewers on one lane's evidence.
///
/// Off by default and **not overridable by a reviewed repository** — every key
/// here spends the operator's money or decides what a model is told, which is
/// the same line `config::remote` draws around `[models]`. See
/// `docs/modules/council/README.md`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Council {
    /// Whether more than one reviewer runs at all.
    pub enabled: bool,
    /// Merge corroborating findings and raise their confidence.
    ///
    /// Separate from `enabled` so the merge can be measured on its own before
    /// a second agent is what is being judged.
    pub corroboration: bool,
    /// The reviewers, in the order they run.
    pub agents: Vec<CouncilAgent>,
}

/// One reviewer in the council.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CouncilAgent {
    /// Stable id, used in the cost line and the check-run summary.
    pub id: String,
    /// Which lanes this agent reviews. Empty means every enabled lane.
    pub lanes: Vec<LaneId>,
    /// A tier name (`scan`, `deep`) or an explicit model id. Absent inherits
    /// the lane's own model.
    pub model: Option<ModelRef>,
    /// A persona name from `council::persona::NAMES`. Absent is the lane's own
    /// prompt, unchanged.
    ///
    /// A **name**, never the text: repository prose reaches a prompt through
    /// exactly one door, and this is not it.
    pub persona: Option<String>,
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
    /// Refuse a pull request touching more than this many files.
    ///
    /// File count is the cheapest proxy for "a human should look at this":
    /// a change spread over thirty files is a refactor, whatever its line
    /// count. `0` refuses everything, which is the safe reading of a
    /// misconfigured cap rather than a special case meaning "unlimited".
    pub max_files: usize,
    /// Refuse a pull request adding and removing more than this many lines.
    pub max_changed_lines: u64,
    /// Refuse a pull request whose diff has more than this many hunks.
    ///
    /// Hunks, not lines: twenty scattered one-line edits are harder to be
    /// sure about than one new two-hundred-line file.
    pub max_hunks: usize,
    /// Refuse a pull request touching more than this many distinct directories.
    ///
    /// The blast radius across the tree. A change confined to one directory
    /// can break one thing; a change across eight has crossed module
    /// boundaries and wants a reviewer who knows all of them.
    pub max_directories: usize,
    /// Globs naming paths that always want a human.
    ///
    /// Anything that changes what runs, or who may run it: CI workflows,
    /// deploy configuration, authentication, the security boundary itself,
    /// and dependency manifests.
    pub sensitive_paths: Vec<String>,
    /// Whether a verified dependency-bump bot may merge manifest-only changes.
    pub allow_dependency_bumps: bool,
    /// The exact logins of the dependency bots that exemption applies to.
    ///
    /// Matched exactly, never by prefix, and only against an author GitHub
    /// itself reports as `type: "Bot"`.
    pub dependency_bots: Vec<String>,
    /// Globs naming the manifests and lockfiles a dependency bump may touch.
    ///
    /// The exemption holds only when *every* changed path matches one of
    /// these. One source file in the same pull request and it is an ordinary
    /// change again.
    pub dependency_paths: Vec<String>,
}

/// Issue triage: dedupe, classify, label, and either escalate with a
/// code-grounded plan or explain and close.
///
/// Like every other model-facing surface, triage *proposes* and the apply path
/// disposes. Nothing here closes an issue without the deterministic guards in
/// [`IssueClose`] agreeing.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Issues {
    /// Whether issue triage runs.
    pub enabled: bool,
    /// Which model tier triage runs on.
    pub model: Option<ModelRef>,
    /// Post a triage comment summarising the finding.
    pub comment: bool,
    /// Apply suggested labels, capped by `max_labels`.
    pub apply_labels: bool,
    /// Never apply more than this many labels to one issue.
    pub max_labels: usize,
    /// Set GitHub's native issue type from the classification.
    ///
    /// Only ever on an issue that carries no type: the type is a single field,
    /// so tinysweeper adding one is helpful and tinysweeper replacing one is
    /// destructive. An owner that defines no issue types is unaffected.
    pub apply_issue_type: bool,
    /// Labels tinysweeper may apply. Empty means any label already in the repo.
    pub allow_labels: Vec<String>,
    /// Never touch an issue carrying one of these.
    pub block_labels: Vec<String>,
    /// Search for and cross-link probable duplicates.
    pub dedupe: bool,
    /// Confidence required before two issues are called duplicates.
    pub dedupe_confidence_min: f64,
    /// When and whether triage may close an issue.
    pub close: IssueClose,
}

/// The deterministic guards on closing an issue.
///
/// Closing someone's issue is the most expensive mistake this bot can make, so
/// every guard here is an age or authorship check that no model output can
/// override.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct IssueClose {
    /// Whether triage may close anything at all.
    pub enabled: bool,
    /// Never close an issue younger than this, whatever the verdict.
    pub min_age_days: u32,
    /// Never close an issue with human activity in this many days.
    pub quiet_days: u32,
    /// Model confidence required before a close is even proposed.
    pub confidence_min: f64,
    /// Never close an issue carrying one of these labels.
    pub protected_labels: Vec<String>,
    /// Never close an issue opened by one of these users.
    pub protected_authors: Vec<String>,
    /// Propose the close as a comment and a label, but do not actually close.
    pub dry_run: bool,
}

/// Scheduled repository automations, run from a cron workflow.
///
/// These are deliberately deterministic-first: labelling and nudging are cheap
/// and predictable, and the model is only involved where judgement is genuinely
/// required.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Automation {
    /// Whether any automation runs.
    pub enabled: bool,
    /// Mark and eventually close inactive issues and pull requests.
    pub stale: Stale,
    /// Keep pull requests labelled by size, area and risk.
    pub labeler: Labeler,
    /// Re-run the gate and merge anything that has become eligible.
    pub merge_sweep: bool,
    /// Ask for a review when a pull request has sat unreviewed.
    pub nudge_after_days: Option<u32>,
}

/// Stale-item handling.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Stale {
    /// Whether stale handling runs.
    pub enabled: bool,
    /// Days of inactivity before an item is marked stale.
    pub days_until_stale: u32,
    /// Further days after marking before the item is closed. `None` never
    /// closes — it only marks, which is the safe default.
    pub days_until_close: Option<u32>,
    /// The label applied when marking.
    pub label: String,
    /// Never mark an item carrying one of these.
    pub exempt_labels: Vec<String>,
}

/// Automatic pull request labelling.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Labeler {
    /// Whether labelling runs.
    pub enabled: bool,
    /// Apply `size:xs` … `size:xl` from the diff's line count.
    pub size: bool,
    /// Map changed paths to area labels, e.g. `"src/server/**" = "area:server"`.
    pub area: BTreeMap<String, String>,
}

/// Sentry issue promotion: unresolved Sentry issues become GitHub issues,
/// deduplicated, PII-scrubbed, and linked back.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Sentry {
    /// Whether promotion runs.
    pub enabled: bool,
    /// Sentry organisation slug.
    pub org: Option<String>,
    /// Sentry projects to sweep.
    pub projects: Vec<String>,
    /// Environment variable holding the Sentry auth token. Never the token.
    pub token_env: String,
    /// Sentry API base URL, for self-hosted installations.
    pub base_url: String,
    /// Promote only issues seen at least this many times.
    pub min_events: u64,
    /// Promote only issues affecting at least this many users.
    pub min_users: u64,
    /// Ignore issues whose culprit matches one of these globs.
    pub ignore_culprits: Vec<String>,
    /// Labels applied to every promoted issue.
    pub labels: Vec<String>,
    /// Cap on issues promoted per run, so a Sentry spike cannot flood the
    /// tracker.
    pub max_per_run: usize,
    /// Comment the GitHub issue link back onto the Sentry issue.
    pub annotate_sentry: bool,
    /// Resolve the Sentry issue once the GitHub issue tracking it is
    /// **closed** — not when it is first promoted.
    ///
    /// The name is slightly misleading and the behaviour is deliberate.
    /// Resolving at promotion time would mark an error fixed the moment
    /// somebody noticed it: the Sentry issue would leave the unresolved list
    /// while the bug is still in production, and the next occurrence would
    /// have to reopen it. Tracked is not fixed. `sentry::link::resolve_if_fixed`
    /// is where the rule lives.
    pub resolve_when_tracked: bool,
    /// Redact anything matching these patterns before it reaches GitHub. This
    /// runs on top of the always-on secret scrubbing, never instead of it.
    pub scrub_patterns: Vec<String>,
    /// Where each project's issues are promoted.
    ///
    /// One deployment can watch several projects, and nothing else in this
    /// section says which repository each one belongs to. A project listed in
    /// [`Sentry::projects`] with no entry here is **skipped and logged**, never
    /// guessed: inferring a repository from a project slug is exactly the kind
    /// of plausible reasoning that opens issues in someone else's tracker.
    pub route: Vec<SentryRoute>,
}

/// One project-to-repository route: `[[sentry.route]]`.
///
/// Routing is the deployment's decision, never the reviewed repository's —
/// which is why `[sentry]` is on the not-overridable list in
/// [`crate::config::remote`] and must stay there.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SentryRoute {
    /// The Sentry project slug this route matches.
    pub project: String,
    /// The GitHub repository its issues are promoted into, as `owner/name`.
    pub repo: String,
    /// Labels applied on top of [`Sentry::labels`] for this project only.
    ///
    /// Additive rather than overriding, so a deployment-wide `sentry` label
    /// cannot be silently dropped by a per-route list that forgot it.
    pub labels: Vec<String>,
}

impl Sentry {
    /// The route for `project`, or `None` when nothing routes it.
    ///
    /// First match wins. Duplicate projects are rejected by
    /// `config::validate`, so in a valid configuration there is at most one.
    pub fn route_for(&self, project: &str) -> Option<&SentryRoute> {
        self.route.iter().find(|route| route.project == project)
    }

    /// Every label a promoted issue from `project` carries: the section-wide
    /// list plus the route's own, deduplicated and order-stable.
    pub fn labels_for(&self, project: &str) -> Vec<String> {
        let mut labels = self.labels.clone();
        if let Some(route) = self.route_for(project) {
            for label in &route.labels {
                if !labels.contains(label) {
                    labels.push(label.clone());
                }
            }
        }
        labels
    }
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
    ///
    /// From `strictness` unless the repository set it explicitly.
    pub fn severity_gate(&self) -> Severity {
        self.review
            .severity_gate
            .as_deref()
            .and_then(Severity::parse)
            .unwrap_or_else(|| self.strictness().severity)
    }

    /// The confidence a finding needs before it is posted.
    pub fn confidence_min(&self) -> f64 {
        self.review
            .confidence_min
            .unwrap_or_else(|| self.strictness().confidence)
    }

    /// The gates `review.strictness` implies.
    fn strictness(&self) -> Strictness {
        Strictness::for_level(self.review.strictness)
    }

    /// The lane-specific overrides for `lane`, if it has any.
    ///
    /// Keys are matched case-insensitively and untrimmed, because that is what
    /// validation accepts. Matching only the canonical spelling here would let
    /// `[lanes.Critique]` validate cleanly and then be silently ignored, which
    /// is the worst outcome available: the setting looks applied and is not.
    pub fn lane(&self, lane: LaneId) -> Option<&Lane> {
        self.lanes
            .iter()
            .find(|(key, _)| LaneId::parse(key) == Some(lane))
            .map(|(_, value)| value)
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

    /// Resolve a council agent to a concrete model id.
    ///
    /// The same three-way rule as [`Config::model_for`] — a tier name, an
    /// explicit id, or nothing — so there is one resolution rule in the
    /// codebase rather than three shapes of it. An agent that names no model
    /// inherits its lane's, which is what makes a one-agent council identical
    /// to no council.
    ///
    /// Deliberately **not** routed through [`Config::model_for_workload`]: that
    /// match is exhaustive over *mechanical* work and pins everything to the
    /// cheap tier, and a council agent is a reviewer.
    pub fn model_for_agent<'a>(&'a self, agent: &'a CouncilAgent, lane: LaneId) -> &'a str {
        match agent.model.as_ref().map(|r| r.0.as_str()) {
            Some("deep") => &self.models.deep,
            Some("scan") => &self.models.scan,
            Some(explicit) => explicit,
            None => self.model_for(lane),
        }
    }

    /// Resolve a non-lane [`Workload`] to a concrete model id.
    ///
    /// Every mechanical workload runs on the cheap tier, and the `match` is
    /// exhaustive on purpose: adding a workload should make someone state which
    /// tier it belongs to rather than inherit whichever one happens to be
    /// first. Repositories cannot override this the way they override a lane —
    /// paying deep-tier prices to copy a snippet out of a diff is not a
    /// trade-off worth exposing.
    pub fn model_for_workload(&self, workload: Workload) -> &str {
        match workload {
            Workload::Relocate
            | Workload::Falsify
            | Workload::KnowledgeExtraction
            | Workload::ThreadReview => &self.models.scan,
        }
    }

    /// Resolve the model issue triage runs on.
    ///
    /// Same three-way resolution as [`Config::model_for`] — a tier name, or an
    /// explicit id passed through untouched — but read from `[issues]`, which
    /// is not a lane and so has no entry in `review.lanes`.
    pub fn model_for_issues(&self) -> &str {
        match self.issues.model.as_ref().map(|r| r.0.as_str()) {
            Some("deep") => &self.models.deep,
            Some("scan") | None => &self.models.scan,
            Some(explicit) => explicit,
        }
    }

    /// The severity at which a review blocks the merge, if it ever does.
    pub fn request_changes_at(&self) -> Option<Severity> {
        let setting = self.review.request_changes_at.trim();
        if setting.eq_ignore_ascii_case("off") || setting.is_empty() {
            return None;
        }
        Severity::parse(setting)
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
        assert_eq!(LaneId::Security.check_name(), "tinysweeper/security");
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
        config.review.lanes = vec!["security".into(), "critique".into(), "security".into()];
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
    fn a_lane_key_is_matched_the_way_validation_accepts_it() {
        // Validation parses lane keys leniently, so lookup has to as well —
        // otherwise `[lanes.Critique]` passes `check` and is then ignored.
        let mut config = Config::default();
        config.models.deep = "expensive-model".into();
        config.lanes.insert(
            " Critique ".into(),
            Lane {
                model: Some(ModelRef("deep".into())),
                ..Lane::default()
            },
        );
        assert_eq!(config.model_for(LaneId::Critique), "expensive-model");
    }

    #[test]
    fn fail_on_defaults_to_high() {
        let config = Config::default();
        assert_eq!(config.fail_on(LaneId::Security), Severity::High);
    }
}
