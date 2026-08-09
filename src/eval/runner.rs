//! Driving the real review engine over the corpus.
//!
//! Always compiled: the replay path needs no key and no network, so a full
//! scoring run is an offline test. Only minting a live model is feature-gated,
//! and that happens in the caller.
//!
//! # The trap this exists to avoid
//!
//! Suppression, cross-push dedupe and prior-review loading make the review's
//! output depend on what it saw last time. A corpus run against a warm state
//! store, or with `review.incremental` left on, measures **run order** and
//! reports it as review quality — and it does so silently, because a suppressed
//! finding looks exactly like a finding that was never made.
//!
//! So every case gets a fresh [`MemoryState`] and `incremental` forced off.
//! That is load-bearing, not tidiness, and it is why the runner owns the config
//! rather than taking one already prepared.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use sha2::{Digest, Sha256};

use crate::app::review::{Proposal, review_with_state};
use crate::config::types::Config;
use crate::error::Result;
use crate::eval::corpus::{Corpus, LoadedCase};
use crate::eval::types::CaseScore;
use crate::forge::types::RepoId;
use crate::harness::cassette::{Cassette, Mode};
use crate::ports::model::Model;
use crate::state::memory::MemoryState;

/// How a corpus run behaves.
#[derive(Debug, Clone)]
pub struct RunOptions {
    /// Where to write proposals. Cassettes go under the corpus root.
    pub out: PathBuf,
    /// Record live answers, rather than replaying recorded ones.
    pub record: bool,
    /// Replay by call order on a key miss. Stamped into every report.
    pub loose: bool,
    /// Store prompts in the cassette. Off by default — a prompt embeds the
    /// reviewed repository's diff.
    pub record_prompts: bool,
    /// Stop the whole run at this many dollars. Checked between cases.
    ///
    /// Distinct from `models.budget_usd_per_pr`, which the engine already
    /// enforces per case: this is the ceiling on the *corpus*, so a bad prompt
    /// cannot turn one command into an unbounded bill.
    pub max_cost_usd: f64,
}

impl Default for RunOptions {
    fn default() -> Self {
        Self {
            out: PathBuf::from("evals/runs/latest"),
            record: false,
            loose: false,
            record_prompts: false,
            max_cost_usd: 5.0,
        }
    }
}

/// What one corpus run produced.
#[derive(Debug, Clone)]
pub struct RunOutcome {
    /// One score per case, in corpus order.
    pub scores: Vec<CaseScore>,
    /// Answers served by call order rather than by key, across the run.
    pub loose_replays: usize,
    /// A hash of the effective configuration, so two runs are comparable only
    /// when they were configured the same way.
    pub config_digest: String,
    /// Cases not run because the corpus ceiling was reached.
    pub skipped: Vec<String>,
}

/// Review every case, score it, and write what it produced.
///
/// `model` is the live provider when recording, and ignored when replaying —
/// the caller passes `None` on the offline path so no key is read.
pub async fn run(
    corpus: &Corpus,
    config: &Config,
    model: Option<Arc<dyn Model>>,
    options: &RunOptions,
) -> Result<RunOutcome> {
    let config = prepare(config);
    let config_digest = digest_of(&config);

    // A `record` run without a live model is a configuration error that would
    // fail every case identically; a per-case score would bury it as N `failed`
    // cases and hide *why*. Fail the run once, before any case is touched.
    if options.record && model.is_none() {
        return Err(crate::error::Error::config(
            "recording needs a live model; build with --features harness",
        ));
    }

    let mut scores = Vec::with_capacity(corpus.cases.len());
    let mut skipped = Vec::new();
    let mut loose_replays = 0usize;
    let mut spent = 0.0f64;

    for case in &corpus.cases {
        if spent >= options.max_cost_usd {
            skipped.push(case.case.id.clone());
            continue;
        }

        // A case whose cassette cannot be opened is scored as failed, not
        // dropped: the same rule `rescore` states below — a corpus that
        // silently scores fewer cases than it holds reports recall that is
        // wrong in the flattering direction.
        let cassette = match open_cassette(case, corpus, model.clone(), options) {
            Ok(cassette) => cassette,
            Err(err) => {
                scores.push(crate::eval::score::failed(
                    &case.case,
                    err.to_string(),
                    std::time::Duration::default(),
                ));
                continue;
            }
        };
        let started = Instant::now();
        let outcome = review_case(case, &with_lanes(&config, case), cassette.clone()).await;
        let wall = started.elapsed();

        if options.record {
            cassette.flush()?;
        }
        let strict_misses = cassette.strict_misses();
        loose_replays += cassette.loose_hits();

        let score = match outcome {
            Ok(proposal) => {
                spent += proposal.cost_usd;
                write_proposal(&options.out, &case.case.id, &proposal)?;
                if strict_misses > 0 {
                    // The review closed, but a strict replay that could not
                    // answer a call means a lane worked around the miss and
                    // reported "could not be reviewed". Scoring that proposal as
                    // if the answers were real would measure a prompt nobody
                    // asked — this is the staleness the corpus exists to make
                    // loud, so the case is failed instead.
                    crate::eval::score::failed(
                        &case.case,
                        format!(
                            "{} call(s) had no recorded answer; re-record the corpus with \
                             `eval run --record`, or replay loosely and accept the numbers \
                             describe the old prompt",
                            strict_misses
                        ),
                        wall,
                    )
                } else {
                    crate::eval::score::score(&case.case, &proposal, wall)
                }
            }
            // A case that fails is scored, not dropped: see `score::failed`.
            Err(err) => crate::eval::score::failed(&case.case, err.to_string(), wall),
        };
        scores.push(score);
    }

    Ok(RunOutcome {
        scores,
        loose_replays,
        config_digest,
        skipped,
    })
}

/// Review one case against a fresh, empty state store.
async fn review_case(case: &LoadedCase, config: &Config, model: Arc<Cassette>) -> Result<Proposal> {
    let forge = case.forge();
    let store = MemoryState::new();
    let repo = RepoId::parse(&case.case.provenance.repo).unwrap_or_else(|| RepoId {
        owner: "corpus".into(),
        name: case.case.id.clone(),
    });

    review_with_state(
        &forge,
        model,
        config,
        &repo,
        case.fixture.pull_request.number,
        Some(&store),
    )
    .await
}

/// The configuration a corpus run is allowed to use.
///
/// One override, and it is the load-bearing one: `incremental` forced off so
/// the measures run order instead of review quality. The second pin the module
/// doc mentions — a fresh [`MemoryState`] per case — lives in `review_case`,
/// because it is a property of the state store rather than of the config.
fn prepare(config: &Config) -> Config {
    let mut config = config.clone();
    config.review.incremental = false;
    config
}

/// Build the cassette this case records into or replays from.
fn open_cassette(
    case: &LoadedCase,
    corpus: &Corpus,
    model: Option<Arc<dyn Model>>,
    options: &RunOptions,
) -> Result<Arc<Cassette>> {
    let dir = case.cassette_dir(&corpus.root);
    if options.record {
        let model = model.ok_or_else(|| {
            crate::error::Error::config(
                "recording needs a live model; build with --features harness",
            )
        })?;
        return Ok(Arc::new(
            Cassette::record(model, dir).with_prompts(options.record_prompts),
        ));
    }
    let mode = if options.loose {
        Mode::Loose
    } else {
        Mode::Strict
    };
    Ok(Arc::new(Cassette::replay(dir, mode)?))
}

/// Re-score proposals already on disk, without calling anything.
///
/// The free half of the loop. A matching rule gets rewritten many times before
/// it is right, and every rewrite has to cost nothing or it does not happen.
/// A case with no proposal in `out` is reported as such rather than skipped —
/// a corpus that silently scored fewer cases than it holds is a corpus whose
/// recall figure is wrong in the flattering direction.
pub fn rescore(corpus: &Corpus, out: &Path) -> Result<Vec<CaseScore>> {
    let mut scores = Vec::with_capacity(corpus.cases.len());
    for case in &corpus.cases {
        let path = out.join(&case.case.id).join("proposal.json");
        let score = match std::fs::read_to_string(&path) {
            Ok(raw) => match serde_json::from_str::<Proposal>(&raw) {
                Ok(proposal) => {
                    crate::eval::score::score(&case.case, &proposal, std::time::Duration::default())
                }
                Err(err) => crate::eval::score::failed(
                    &case.case,
                    format!("{}: not a proposal: {err}", path.display()),
                    std::time::Duration::default(),
                ),
            },
            Err(_) => crate::eval::score::failed(
                &case.case,
                format!("no proposal at {}; run `eval run` first", path.display()),
                std::time::Duration::default(),
            ),
        };
        scores.push(score);
    }
    Ok(scores)
}

/// Write a scorecard where `eval report` can find it.
pub fn write_scorecard(out: &Path, card: &crate::eval::types::Scorecard) -> Result<PathBuf> {
    std::fs::create_dir_all(out).map_err(|err| crate::error::Error::path(out, err))?;
    let path = out.join("scorecard.json");
    let json = serde_json::to_string_pretty(card)?;
    std::fs::write(&path, json).map_err(|err| crate::error::Error::path(&path, err))?;
    Ok(path)
}

/// Read a scorecard written by an earlier run.
pub fn read_scorecard(path: &Path) -> Result<crate::eval::types::Scorecard> {
    let raw = std::fs::read_to_string(path).map_err(|err| crate::error::Error::path(path, err))?;
    serde_json::from_str(&raw)
        .map_err(|err| crate::error::Error::path(path, format!("not a scorecard: {err}")))
}

/// Apply the case's lane selection on top of the run configuration.
pub fn with_lanes(config: &Config, case: &LoadedCase) -> Config {
    let mut config = config.clone();
    if !case.case.lanes.is_empty() {
        config.review.lanes = case.case.lanes.clone();
    }
    config
}

/// Write a proposal where a later `eval score` can find it.
fn write_proposal(out: &Path, id: &str, proposal: &Proposal) -> Result<()> {
    let dir = out.join(id);
    std::fs::create_dir_all(&dir).map_err(|err| crate::error::Error::path(&dir, err))?;
    let path = dir.join("proposal.json");
    let json = serde_json::to_string_pretty(proposal)?;
    std::fs::write(&path, json).map_err(|err| crate::error::Error::path(&path, err))
}

/// A short hash of everything about the configuration that can move a score.
///
/// Stamped into the report so `eval report --baseline` can refuse to compare a
/// run against one configured differently. Comparing a `strictness = 3` run
/// against a `strictness = 2` baseline and reading the difference as a prompt
/// improvement is the mistake this makes impossible.
pub fn digest_of(config: &Config) -> String {
    let mut hasher = Sha256::new();
    // A label per field, and a `\0` after every value. Without both, two
    // different configurations can hash as one byte stream — `scan: "ab",
    // deep: "c"` versus `scan: "a", deep: "bc"` — and the report would call a
    // differently-configured run comparable.
    let mut field = |label: &[u8], value: &[u8]| {
        hasher.update(label);
        hasher.update(value);
        hasher.update(b"\0");
    };
    let review = &config.review;
    field(b"lanes\0", review.lanes.join(",").as_bytes());
    field(b"strictness\0", &review.strictness.to_le_bytes());
    field(
        b"severity_gate\0",
        format!("{:?}", review.severity_gate).as_bytes(),
    );
    field(
        b"confidence_min\0",
        format!("{:?}", review.confidence_min).as_bytes(),
    );
    field(b"max_comments\0", &review.max_comments.to_le_bytes());
    let models = &config.models;
    field(b"scan\0", models.scan.as_bytes());
    field(b"deep\0", models.deep.as_bytes());
    field(b"fallback\0", models.fallback.join(",").as_bytes());
    field(b"max_tokens\0", &models.max_tokens.to_le_bytes());
    field(b"reasoning_effort\0", models.reasoning_effort.as_bytes());
    // Per-lane overrides move `Config::model_for` — a lane pinned to a cheaper
    // or stronger model answers differently — so a score made with them is not
    // the same run as one without. `BTreeMap` iterates in id order, keeping
    // the hash stable.
    for (id, lane) in &config.lanes {
        field(b"lane\0", id.as_bytes());
        field(b"lane_model\0", format!("{:?}", lane.model).as_bytes());
        field(b"lane_fail_on\0", format!("{:?}", lane.fail_on).as_bytes());
        field(
            b"lane_rulepack\0",
            format!("{:?}", lane.secret_rulepack).as_bytes(),
        );
        field(
            b"lane_max_blob\0",
            format!("{:?}", lane.max_blob_bytes).as_bytes(),
        );
    }
    for instruction in &config.path_instructions {
        field(b"glob\0", instruction.glob.as_bytes());
        field(b"instructions\0", instruction.instructions.as_bytes());
    }
    hasher
        .finalize()
        .iter()
        .take(8)
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[cfg(test)]
#[path = "runner_test.rs"]
mod tests;
