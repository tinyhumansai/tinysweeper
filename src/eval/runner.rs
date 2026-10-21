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

use crate::app::review::{Proposal, review_with_tree};
use crate::config::types::Config;
use crate::error::Result;
use crate::eval::corpus::{Corpus, LoadedCase};
use crate::eval::types::{CaseScore, EffectiveCaseBudget, ExecutionLimits};
use crate::forge::types::RepoId;
use crate::harness::cassette::{Cassette, Mode};
use crate::ports::model::{Model, Spend, Usage};
use crate::ports::tree::TreeReader;
use crate::state::memory::MemoryState;

const DEFAULT_RECORDING_PHYSICAL_CALLS_PER_CASE: usize = 8;
const MAX_RECORDING_PHYSICAL_CALLS_PER_CASE: usize = 56;

/// Failure metadata is separate from labels, so re-scoring uses current ground truth.
#[derive(serde::Serialize, serde::Deserialize)]
struct RecordedFailure {
    reason: String,
    wall_secs: f64,
    usage: Usage,
    models: Vec<String>,
}

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
    /// Maximum physical model dispatches admitted for each recorded case.
    /// Recording requires an explicit bound; reservations persist across runs.
    pub max_physical_calls_per_case: Option<usize>,
    /// Stop admitting cases once known spend reaches this amount. Checked
    /// between cases, so the currently running case can take the total over it.
    ///
    /// Distinct from `models.budget_usd_per_pr`, which the engine applies
    /// within a case. This is not a provider-side or whole-run hard cap.
    pub max_cost_usd: f64,
    /// A checkout of the case's head, for the reviewer to look things up in
    /// while recording. What it reads is written into the fixture's
    /// `lookups` so the replay needs no checkout.
    pub tree: Option<PathBuf>,
}

impl Default for RunOptions {
    fn default() -> Self {
        Self {
            out: PathBuf::from("evals/runs/latest"),
            record: false,
            loose: false,
            record_prompts: false,
            max_physical_calls_per_case: Some(DEFAULT_RECORDING_PHYSICAL_CALLS_PER_CASE),
            max_cost_usd: 5.0,
            tree: None,
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
    /// A hash of score-affecting configuration inputs. Run and case bounds are
    /// persisted separately in `execution_limits`.
    pub config_digest: String,
    /// The selected cases and their effective run/admission limits.
    pub execution_limits: ExecutionLimits,
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
    if options.record
        && !matches!(
            options.max_physical_calls_per_case,
            Some(1..=MAX_RECORDING_PHYSICAL_CALLS_PER_CASE)
        )
    {
        return Err(crate::error::Error::config(format!(
            "recording requires --max-physical-calls-per-case between 1 and {MAX_RECORDING_PHYSICAL_CALLS_PER_CASE}"
        )));
    }

    let mut execution_limits = effective_execution_limits(corpus, &config, options);

    // `--tree` names one checkout on disk, but every case in the run would
    // otherwise be handed the same one regardless of which repository or
    // head it actually reviews. Recording several cases that way answers
    // every case's lookups from one case's tree, feeding the wrong evidence
    // into unrelated fixtures and corrupting their replay permanently. Fail
    // before any case runs rather than after some fixtures are already
    // overwritten.
    if options.tree.is_some() && corpus.cases.len() > 1 {
        return Err(crate::error::Error::config(
            "--tree applies to one case; pass --case",
        ));
    }

    let mut scores = Vec::with_capacity(corpus.cases.len());
    let mut skipped = Vec::new();
    let mut loose_replays = 0usize;
    let mut spent = 0.0f64;
    let mut unknown_charge = false;

    for case in &corpus.cases {
        if unknown_charge || spent >= options.max_cost_usd {
            skipped.push(case.case.id.clone());
            continue;
        }

        let reservation = if options.record {
            Some(crate::eval::recording_budget::Reservation::begin(
                &corpus.root.join("recording-attempts").join(&case.case.id),
                options
                    .max_physical_calls_per_case
                    .expect("recording cap validated"),
                // The selected CLI ceiling is cumulative across reruns; the
                // hard maximum only bounds which ceiling the operator may set.
                options
                    .max_physical_calls_per_case
                    .expect("recording cap validated"),
            )?)
        } else {
            None
        };
        if let Some(reservation) = &reservation {
            let budget = execution_limits
                .effective_case_budgets
                .iter_mut()
                .find(|budget| budget.case_id == case.case.id)
                .expect("selected case has an execution budget");
            budget.max_physical_calls_per_case = Some(reservation.reserved_calls());
            archive_prior_takes(
                &case.cassette_dir(&corpus.root),
                reservation.directory(),
                reservation.id(),
            )?;
        }

        // A case whose cassette cannot be opened is scored as failed, not
        // dropped: the same rule `rescore` states below — a corpus that
        // silently scores fewer cases than it holds reports recall that is
        // wrong in the flattering direction.
        let cassette = match open_cassette(case, corpus, model.clone(), options) {
            Ok(cassette) => cassette,
            Err(err) => {
                scores.push(persist_failure_with_spend(
                    case,
                    &options.out,
                    err.to_string(),
                    std::time::Duration::default(),
                    Spend::default(),
                )?);
                continue;
            }
        };
        let started = Instant::now();
        // Lookups replay from the fixture, or record into it. A live tree
        // without `--record` would answer lookups the cassette never saw,
        // which is a strict miss dressed as a review.
        let dir_tree = options
            .tree
            .as_ref()
            .filter(|_| options.record)
            // A path supplied on disk is not proof that its contents are the
            // fixture's head. Leave the reader's revision unknown; callers
            // that verify a checkout can provide a reader with a real pin.
            .map(crate::ports::tree::DirTree::new);
        let recorded = crate::ports::tree::MockTree::from_recorded(case.fixture.lookups.clone());
        let recording = dir_tree
            .as_ref()
            .map(|dir| crate::ports::tree::RecordingTree::new(dir as &dyn TreeReader));
        let tree: &dyn TreeReader = match &recording {
            Some(recording) => recording,
            None => &recorded,
        };
        let mut case_config = with_lanes(&config, case);
        if let Some(budget) = execution_limits
            .effective_case_budgets
            .iter()
            .find(|budget| budget.case_id == case.case.id)
            .and_then(|budget| budget.budget_usd)
        {
            case_config.models.budget_usd_per_pr = budget;
        }
        let (outcome, physical_calls) = review_case(
            case,
            &case_config,
            cassette.clone(),
            tree,
            reservation.as_ref().map(|r| r.reserved_calls()),
        )
        .await;
        if let Some(reservation) = reservation {
            reservation.finish(physical_calls)?;
        }
        let wall = started.elapsed();

        if let Some(recording) = &recording {
            let learned = recording.recorded();
            if !learned.is_empty() {
                let mut fixture = case.fixture.clone();
                fixture.lookups.extend(learned);
                let path = case
                    .path
                    .parent()
                    .unwrap_or(&case.path)
                    .join(&case.case.fixture);
                std::fs::write(&path, serde_json::to_string_pretty(&fixture)? + "\n").map_err(
                    |e| crate::error::Error::Path {
                        path: path.clone(),
                        message: e.to_string(),
                    },
                )?;
            }
        }

        if options.record {
            cassette.flush()?;
        }
        let strict_misses = cassette.strict_misses();
        loose_replays += cassette.loose_hits();
        // Spend is what actually left the account, not what a successful
        // proposal reported: a case that failed after its calls has still spent
        // them, and charges must not walk the corpus ceiling on the strength of
        // never having produced a proposal to sum. Recorded usage replayed
        // verbatim, so the ceil works offline too.
        spent += cassette.cost_usd();
        // A refusal without usage may still have incurred a provider charge.
        // Stop before admitting another case instead of spending against an
        // unknowable remainder of the shared ceiling.
        unknown_charge |= cassette.has_unreported_usage();

        let score = match outcome {
            Ok(proposal) => {
                if strict_misses > 0 {
                    // The review closed, but a strict replay that could not
                    // answer a call means a lane worked around the miss and
                    // reported "could not be reviewed". Scoring that proposal as
                    // if the answers were real would measure a prompt nobody
                    // asked — this is the staleness the corpus exists to make
                    // loud. No proposal is written: a proposal on disk is what a
                    // later `eval score` trusts, and re-scoring a stale replay
                    // as a normal result would silently undo the failure. A
                    // marker is persisted instead, which `rescore` reproduces.
                    let reason = format!(
                        "{} call(s) had no recorded answer; re-record the corpus with \
                         `eval run --record`, or replay loosely and accept the numbers \
                         describe the old prompt",
                        strict_misses
                    );
                    persist_failed_case(case, &options.out, reason, wall, &cassette)?
                } else {
                    write_proposal(&options.out, &case.case.id, &proposal)?;
                    crate::eval::score::score(&case.case, &proposal, wall)
                }
            }
            // A case that fails is scored, not dropped: see `score::failed`.
            Err(err) => persist_failed_case(case, &options.out, err.to_string(), wall, &cassette)?,
        };
        scores.push(score);
    }

    Ok(RunOutcome {
        scores,
        loose_replays,
        config_digest,
        execution_limits,
        skipped,
    })
}

fn effective_execution_limits(
    corpus: &Corpus,
    config: &Config,
    options: &RunOptions,
) -> ExecutionLimits {
    let selected_case_ids = corpus
        .cases
        .iter()
        .map(|case| case.case.id.clone())
        .collect();
    let effective_case_budgets = corpus
        .cases
        .iter()
        .map(|case| EffectiveCaseBudget {
            case_id: case.case.id.clone(),
            budget_usd: (options.record && !corpus.cases.is_empty()).then(|| {
                config
                    .models
                    .budget_usd_per_pr
                    .min(case.case.budget.max_cost_usd)
                    .min(options.max_cost_usd / corpus.cases.len() as f64)
            }),
            max_physical_calls_per_case: None,
        })
        .collect();
    ExecutionLimits {
        selected_case_ids,
        max_cost_usd: options.max_cost_usd,
        max_physical_calls_per_case: options.max_physical_calls_per_case,
        recording: options.record,
        loose_replay: options.loose,
        effective_case_budgets,
    }
}

/// Review one case against a fresh, empty state store.
async fn review_case(
    case: &LoadedCase,
    config: &Config,
    model: Arc<Cassette>,
    tree: &dyn TreeReader,
    max_physical_calls: Option<usize>,
) -> (Result<Proposal>, usize) {
    let forge = case.forge();
    let store = MemoryState::new();
    let repo = RepoId::parse(&case.case.provenance.repo).unwrap_or_else(|| RepoId {
        owner: "corpus".into(),
        name: case.case.id.clone(),
    });

    if let Some(max_calls) = max_physical_calls {
        let started_at = tokio::time::Instant::now();
        let execution = Arc::new(crate::app::review_execution::ReviewRunContext::new(
            crate::app::review_execution::ReviewExecutionOptions {
                started_at,
                hard_deadline: started_at + std::time::Duration::from_secs(900),
                #[cfg(test)]
                mode: crate::lanes::large_pr::ReviewMode::Normal,
                max_calls: Some(max_calls),
                soft_deadline_percent: config.review.large_pr.soft_deadline_percent,
                drain_seconds: config.review.large_pr.drain_seconds,
            },
        ));
        let binding = execution.bind_model(model, config.models.budget_usd_per_pr);
        #[cfg(not(test))]
        if !binding.execution_control_bound {
            return (Err(crate::error::Error::ReviewExecutionUnavailable), 0);
        }
        let outcome = crate::app::review::review_with_tree_observed_at_head_in_run(
            &forge,
            binding.model.clone(),
            config,
            &repo,
            case.fixture.pull_request.number,
            Some(&store),
            None,
            None,
            Some(tree),
            None,
            execution.clone(),
            None,
        )
        .await;
        let calls = execution.execution().snapshot().committed_calls;
        (outcome, calls)
    } else {
        (
            review_with_tree(
                &forge,
                model,
                config,
                &repo,
                case.fixture.pull_request.number,
                Some(&store),
                None,
                None,
                Some(tree),
            )
            .await,
            0,
        )
    }
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
    // The committed corpus measures review lanes. Summary generation has its
    // own schema and cassettes; including it here would make every historical
    // lane cassette require an unrelated trailing response.
    config.summary.enabled = false;
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

/// Move prior takes out of the active replay directory before a new recording.
///
/// This runs only after the persistent physical-call reservation succeeds, so
/// a rejected rerun never changes the cassette. The attempt archive lives next
/// to the append-only audit and is not loaded by cassette replay.
fn archive_prior_takes(
    cassette_dir: &std::path::Path,
    audit_dir: &std::path::Path,
    id: &str,
) -> Result<()> {
    let entries = match std::fs::read_dir(cassette_dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(crate::error::Error::path(cassette_dir, err)),
    };
    let mut takes = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|err| crate::error::Error::path(cassette_dir, err))?;
        let path = entry.path();
        if path.is_file()
            && path
                .extension()
                .is_some_and(|extension| extension == "json")
        {
            takes.push(path);
        }
    }
    if takes.is_empty() {
        return Ok(());
    }
    takes.sort();
    let archive_dir = audit_dir.join("prior-takes").join(id);
    std::fs::create_dir_all(&archive_dir)
        .map_err(|err| crate::error::Error::path(&archive_dir, err))?;
    for source in takes {
        let destination = archive_dir.join(source.file_name().expect("take path has a filename"));
        std::fs::rename(&source, &destination)
            .map_err(|err| crate::error::Error::path(&source, err))?;
    }
    Ok(())
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
        let dir = out.join(&case.case.id);
        // A case that failed stays failed. `run` persists a strict-miss replay
        // as a `failure.json` marker rather than a proposal, so re-scoring must
        // reproduce that failure instead of trusting a proposal that was never
        // emitted — or `eval score` would silently turn a stale replay into a
        // normal scorecard entry.
        let score = match std::fs::read_to_string(dir.join("failure.json")) {
            Ok(raw) => {
                match serde_json::from_str::<RecordedFailure>(&raw) {
                    Ok(failure)
                        if std::time::Duration::try_from_secs_f64(failure.wall_secs).is_ok() =>
                    {
                        crate::eval::score::failed_with_spend(
                            &case.case,
                            failure.reason,
                            std::time::Duration::try_from_secs_f64(failure.wall_secs)
                                .expect("validated failure duration"),
                            Spend {
                                usage: failure.usage,
                                models: failure.models,
                            },
                        )
                    }
                    _ => {
                        // Older runs persisted just the reason, without known usage.
                        let reason = serde_json::from_str::<String>(&raw).unwrap_or_else(|_| {
                            format!("the run failed for an unreadable reason: {raw}")
                        });
                        crate::eval::score::failed(
                            &case.case,
                            reason,
                            std::time::Duration::default(),
                        )
                    }
                }
            }
            Err(_) => {
                let path = dir.join("proposal.json");
                match std::fs::read_to_string(&path) {
                    Ok(raw) => match serde_json::from_str::<Proposal>(&raw) {
                        Ok(proposal) => crate::eval::score::score(
                            &case.case,
                            &proposal,
                            std::time::Duration::default(),
                        ),
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
                }
            }
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
    // A successful run supersedes a previously recorded failure of the case.
    let _ = std::fs::remove_file(dir.join("failure.json"));
    let path = dir.join("proposal.json");
    let json = serde_json::to_string_pretty(proposal)?;
    std::fs::write(&path, json).map_err(|err| crate::error::Error::path(&path, err))
}

/// Persist that a case failed, where a later `eval score` can reproduce it.
///
/// The mirror of [`write_proposal`], and the two are mutually exclusive on
/// disk. A strict replay that could not answer a call is not a measurement, so
/// it must not leave a proposal behind that a later `eval score` would trust
/// as one — the failure is what belongs there, and `rescore` prefers it.
fn write_failure(out: &Path, id: &str, failure: &RecordedFailure) -> Result<()> {
    let dir = out.join(id);
    std::fs::create_dir_all(&dir).map_err(|err| crate::error::Error::path(&dir, err))?;
    let _ = std::fs::remove_file(dir.join("proposal.json"));
    let path = dir.join("failure.json");
    std::fs::write(&path, serde_json::to_string_pretty(failure)?)
        .map_err(|err| crate::error::Error::path(&path, err))
}

/// Keep failure, known paid usage, and wall time together; never price an unknown refusal.
fn persist_failed_case(
    case: &LoadedCase,
    out: &Path,
    mut reason: String,
    wall: std::time::Duration,
    cassette: &Cassette,
) -> Result<CaseScore> {
    let spend = cassette.spend();
    if cassette.has_unreported_usage() {
        reason.push_str("; cost includes known reported usage only; charges for model calls without reported usage are unknown");
    }
    persist_failure_with_spend(case, out, reason, wall, spend)
}

/// A failure before dispatch has known zero usage and still supersedes an old proposal.
fn persist_failure_with_spend(
    case: &LoadedCase,
    out: &Path,
    reason: String,
    wall: std::time::Duration,
    spend: Spend,
) -> Result<CaseScore> {
    write_failure(
        out,
        &case.case.id,
        &RecordedFailure {
            reason: reason.clone(),
            wall_secs: wall.as_secs_f64(),
            usage: spend.usage,
            models: spend.models.clone(),
        },
    )?;
    Ok(crate::eval::score::failed_with_spend(
        &case.case, reason, wall, spend,
    ))
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
    field(
        b"reviewer_reasoning_effort\0",
        models.reviewer_reasoning_effort.as_bytes(),
    );
    field(
        b"falsify_reasoning_effort\0",
        models.falsify_reasoning_effort.as_bytes(),
    );
    field(
        b"summary_reasoning_effort\0",
        models.summary_reasoning_effort.as_bytes(),
    );
    // Preserve historical one-shot digests, but never compare an agentic
    // tool run or different physical-call admission bounds as the same run.
    if models.agentic_reviewers {
        field(b"agentic_reviewers\0", &[1]);
    }
    for (model, bound) in &models.budget_prices {
        field(b"budget_price_model\0", model.as_bytes());
        field(b"budget_price_input\0", &bound.input.to_le_bytes());
        field(b"budget_price_cached\0", &bound.cached.to_le_bytes());
        field(b"budget_price_output\0", &bound.output.to_le_bytes());
    }
    // A per-model route moves a score the same way the global ceiling does —
    // a ceiling of 0 completes a review that 16k truncates — and pins the
    // call to an endpoint with its own price. Two runs with the same tier
    // names and different routes are not the same run. Hashed in config
    // order: the order is what `Models::route_for` searches.
    for route in &models.routes {
        field(b"route\0", route.model.as_bytes());
        field(b"route_order\0", route.order.join(",").as_bytes());
        field(
            b"route_allow_fallbacks\0",
            &[u8::from(route.allow_fallbacks)],
        );
        field(
            b"route_max_tokens\0",
            format!("{:?}", route.max_tokens).as_bytes(),
        );
    }
    // What a reviewer may read before it answers moves every score: the turns
    // it takes, the evidence it sees, and the cost. Two runs under different
    // lookup policies are not the same run.
    let lookup = &config.lookup;
    field(b"lookup.enabled\0", &[u8::from(lookup.enabled)]);
    field(b"lookup.rounds\0", &[lookup.rounds]);
    field(b"lookup.per_round\0", &[lookup.per_round]);
    field(b"lookup.max_chars\0", &lookup.max_chars.to_le_bytes());
    // The per-PR ceiling moves a score too: exceeding it is `Error::Budget`,
    // which the runner scores as a failed case. A run configured with a $0.50
    // ceiling and one with a $50.00 ceiling are not the same run.
    field(
        b"budget_usd_per_pr\0",
        &models.budget_usd_per_pr.to_le_bytes(),
    );
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
        // The selectors decide which prompt is actually built, so they move a
        // score: `rules` names the rule document whose text is inlined into
        // `instructions`, and `lanes` decides which lanes are handed those
        // injected instructions at all. Two runs that differ only in a
        // selector are configured differently and must not silently compare.
        if let Some(rules) = &instruction.rules {
            field(b"rules\0", rules.as_bytes());
        }
        // `merge` decides whether the next matching entry is appended too, so
        // the same table renders two different prompts with it on and off.
        field(b"merge\0", &[u8::from(instruction.merge)]);
        field(
            b"lanes\0",
            instruction
                .lanes
                .iter()
                .map(|lane| lane.as_str())
                .collect::<Vec<_>>()
                .join(",")
                .as_bytes(),
        );
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
