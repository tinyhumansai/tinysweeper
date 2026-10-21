//! Recording and replaying model calls.
//!
//! Always compiled, and deliberately a decorator over the [`Model`] port rather
//! than a feature of the provider adapter. The port has one method, so wrapping
//! it costs nothing — and it buys the whole offline story: a corpus recorded
//! once can be re-scored a hundred times, on default features, with no key and
//! no network.
//!
//! # Why running and scoring are separated by a file
//!
//! A live run costs money. A scoring rule will be rewritten ten times before it
//! is right. Welding them together means every refinement of the matching rule
//! costs another five dollars and a fresh set of provider dice, which is how a
//! metric stops being iterated on. So the model's answers are durable, and
//! everything downstream of them is a pure function of what is on disk.
//!
//! # A key miss is loud on purpose
//!
//! The cassette key covers the model id, the schema name, the token ceiling,
//! an explicit per-request reasoning override, and every message. So any change to a prompt invalidates every cassette that
//! prompt produced — and that is the *point*. A scoring run that silently fell
//! back to a stale recording would report the old prompt's quality under the new
//! prompt's name, which is worse than no measurement at all. Strict replay says
//! so and stops.
//!
//! [`Mode::Loose`] exists for the narrow case where that is too strict — a
//! whitespace change, a reordered field — and it is stamped into the report so
//! nobody compares a loose run against a strict one without seeing it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::error::{Error, Result};
use crate::ports::model::{Message, Model, ModelRequest, ModelResponse, Spend, Usage};

/// One recorded call.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Take {
    /// The content hash of the request. See [`key`].
    pub key: String,
    /// The model the lane asked for.
    pub model_requested: String,
    /// The model that actually answered. A fallback makes these differ, and
    /// that difference is worth replaying too.
    pub model_answered: String,
    /// The schema the answer had to satisfy, for a human reading the file.
    pub schema_name: String,
    /// What it cost.
    pub usage: Usage,
    /// The structured answer.
    pub value: Value,
    /// Lookup policy, source revision and tree-query outcomes for reviews.
    /// Missing from historical completion-only recordings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_context: Option<ReviewContext>,
    /// The prompt, recorded only when explicitly asked for.
    ///
    /// Off by default because a prompt embeds the reviewed repository's diff,
    /// and a cassette committed from a private repository would carry that
    /// diff into git. Public-corpus cases can turn it on; it is the only way to
    /// debug a prompt change from a recording afterwards.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<Vec<RecordedMessage>>,
}

/// Repository context needed to replay a review without a live model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewContext {
    /// Effective lookup limits and feature flags for this review.
    pub policy: crate::config::types::LookupPolicy,
    /// The tree revision when the review was recorded, when known.
    pub revision: Option<String>,
    /// Repository operations completed during the review, in completion order.
    pub queries: Vec<RecordedQuery>,
}

/// A repository operation and a digest of its redacted result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordedQuery {
    /// The operation sent to the tree reader.
    pub query: RecordedTreeQuery,
    /// SHA-256 of a canonical `Found`, or `failed` for a returned error.
    pub result: String,
}

/// Serializable form of the read-only repository operations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "query", rename_all = "snake_case")]
pub enum RecordedTreeQuery {
    /// A normal repository lookup.
    Lookup(crate::ports::tree::Lookup),
    /// A bounded tree query.
    List { path: String, limit: u32 },
    /// A literal symbol query.
    Symbol { symbol: String, limit: u32 },
    /// A read against a historical commit.
    History {
        commit: String,
        path: String,
        start: u32,
        end: u32,
    },
}

/// One message of a recorded prompt.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecordedMessage {
    /// `system`, `user` or `assistant`.
    pub role: String,
    /// The text.
    pub content: String,
    /// Image URLs attached to a user message. Absent from every recording
    /// made before images existed, and from every text-only prompt since.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<String>,
}

/// How a [`Cassette`] behaves when it is asked for a call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Call the inner model and write every answer to disk.
    Record,
    /// Serve answers from disk. A key miss is an error naming the case.
    Strict,
    /// Serve answers from disk, falling back to call order on a key miss.
    ///
    /// Survives a cosmetic prompt edit without a re-record, and is a lie about
    /// determinism if it is ever compared against a strict run — which is why
    /// [`Cassette::loose_hits`] is reported.
    Loose,
}

/// A model that records what it was told, or replays what it was told before.
pub struct Cassette {
    inner: Option<Arc<dyn Model>>,
    dir: PathBuf,
    mode: Mode,
    /// Recorded answers by key, loaded once at construction.
    takes: BTreeMap<String, Take>,
    /// Every take in file order, for the loose fallback.
    ordered: Vec<Take>,
    state: Arc<Mutex<Playback>>,
    record_prompts: bool,
}

/// Mutable playback bookkeeping.
#[derive(Debug, Default)]
struct Playback {
    /// How many calls have been served, which is also the loose cursor.
    served: usize,
    /// How many were served by call order rather than by key.
    loose_hits: usize,
    /// How many strict replay could not serve by key.
    ///
    /// Separate from `loose_hits` because the two mean opposite things: a loose
    /// hit is an allowed fallback and is reported; a strict miss is the
    /// staleness the corpus exists to make loud.
    misses: usize,
    /// Known paid usage of every answer served this run, recorded or replayed.
    ///
    /// Accumulated at serve time rather than derived from the written files,
    /// so the figure is right even for a case that failed after its calls —
    /// the ones no proposal will ever account for.
    spend: Spend,
    /// A live refusal without reported usage cannot be assumed free.
    unreported_errors: usize,
    /// Takes recorded this run, in call order.
    recorded: Vec<Take>,
}

impl std::fmt::Debug for Cassette {
    /// Hand-written so the inner model — which holds an API key — is named
    /// rather than printed.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Cassette")
            .field("dir", &self.dir)
            .field("mode", &self.mode)
            .field("takes", &self.takes.len())
            .field("live", &self.inner.is_some())
            .finish()
    }
}

impl Cassette {
    /// Record every call `inner` answers into `dir`.
    pub fn record(inner: Arc<dyn Model>, dir: impl Into<PathBuf>) -> Self {
        Self {
            inner: Some(inner),
            dir: dir.into(),
            mode: Mode::Record,
            takes: BTreeMap::new(),
            ordered: Vec::new(),
            state: Arc::new(Mutex::new(Playback::default())),
            record_prompts: false,
        }
    }

    /// Replay the cassette in `dir`. No model is called and no key is read.
    pub fn replay(dir: impl Into<PathBuf>, mode: Mode) -> Result<Self> {
        let dir = dir.into();
        let ordered = load(&dir)?;
        let takes = ordered
            .iter()
            .map(|take| (take.key.clone(), take.clone()))
            .collect();
        Ok(Self {
            inner: None,
            dir,
            mode,
            takes,
            ordered,
            state: Arc::new(Mutex::new(Playback::default())),
            record_prompts: false,
        })
    }

    /// Also store the prompt text. See [`Take::prompt`] for why this is off by
    /// default.
    pub fn with_prompts(mut self, record_prompts: bool) -> Self {
        self.record_prompts = record_prompts;
        self
    }

    /// How many answers were served by call order rather than by key.
    ///
    /// Non-zero means the corpus is stale against the current prompts, and the
    /// numbers describe a prompt nobody is running.
    pub fn loose_hits(&self) -> usize {
        self.state.lock().expect("cassette lock").loose_hits
    }

    /// How many strict-replay calls had no recorded answer.
    ///
    /// Strict replay never recovers from a miss: the call simply errors. The
    /// error is *loud* where it happens, but a lane's fan-out can turn it into
    /// a neutral "could not be reviewed" summary — so the run also checks this
    /// count to decide a case that could not actually be replayed is failed.
    pub fn strict_misses(&self) -> usize {
        self.state.lock().expect("cassette lock").misses
    }

    /// How many calls were served.
    pub fn served(&self) -> usize {
        self.state.lock().expect("cassette lock").served
    }

    /// How much the answers served this run cost, recorded or replayed.
    ///
    /// Counted per served answer, so the figure covers cases that failed after
    /// their calls — the ones a proposal never exists for. The corpus ceiling
    /// keys off this so a run of failing cases cannot walk its budget.
    pub fn cost_usd(&self) -> f64 {
        self.state.lock().expect("cassette lock").spend.cost_usd()
    }

    /// Known served-response usage and actual answering models, including failed cases.
    pub fn spend(&self) -> Spend {
        self.state.lock().expect("cassette lock").spend.clone()
    }

    /// Whether a live refusal carried no usage, leaving any charge unknown.
    pub fn has_unreported_usage(&self) -> bool {
        self.state.lock().expect("cassette lock").unreported_errors > 0
    }

    /// Write everything recorded this run to disk, oldest call first.
    ///
    /// Called explicitly rather than on `Drop`: writing files from a destructor
    /// means an I/O failure has nowhere to be reported, and a corpus that
    /// silently failed to record is the one failure this must not have.
    pub fn flush(&self) -> Result<usize> {
        if self.mode != Mode::Record {
            return Ok(0);
        }
        let recorded = {
            let state = self.state.lock().expect("cassette lock");
            state.recorded.clone()
        };
        if recorded.is_empty() {
            return Ok(0);
        }

        std::fs::create_dir_all(&self.dir).map_err(|err| Error::path(&self.dir, err))?;
        for (index, take) in recorded.iter().enumerate() {
            // Sequence first so the directory reads in call order, key second
            // so two identical prompts in one run do not overwrite each other.
            let name = format!("{:04}-{}.json", index + 1, take.key);
            let path = self.dir.join(name);
            let json = serde_json::to_string_pretty(take)?;
            std::fs::write(&path, json).map_err(|err| Error::path(&path, err))?;
        }
        Ok(recorded.len())
    }

    /// Rebuild this recorder around a scoped inner model while retaining its
    /// recording cursor, takes, and output policy.
    fn with_inner(&self, inner: Arc<dyn Model>) -> Self {
        Self {
            inner: Some(inner),
            dir: self.dir.clone(),
            mode: self.mode,
            takes: self.takes.clone(),
            ordered: self.ordered.clone(),
            state: self.state.clone(),
            record_prompts: self.record_prompts,
        }
    }

    fn make_take(
        &self,
        key: String,
        request: &ModelRequest,
        response: &ModelResponse,
        review_context: Option<ReviewContext>,
    ) -> Take {
        Take {
            key,
            model_requested: request.model.clone(),
            model_answered: response.model.clone(),
            schema_name: request.schema_name.clone(),
            usage: response.usage,
            value: response.value.clone(),
            review_context,
            prompt: self.record_prompts.then(|| {
                request
                    .messages
                    .iter()
                    .map(|message| RecordedMessage {
                        role: role_name(message).to_string(),
                        content: message.content.clone(),
                        images: message.images.clone(),
                    })
                    .collect()
            }),
        }
    }

    fn record_error(&self, error: &Error) {
        let mut state = self.state.lock().expect("cassette lock");
        match error {
            Error::ModelExecutionRefused {
                usage,
                accounting_complete,
                ..
            } => {
                if let Some(usage) = usage {
                    state.spend.usage.add(**usage);
                }
                if !*accounting_complete {
                    state.unreported_errors += 1;
                }
            }
            _ => {
                if let Some(usage) = error.usage() {
                    state.spend.usage.add(usage);
                } else {
                    state.unreported_errors += 1;
                }
            }
        }
    }

    fn record_miss(&self) {
        self.state.lock().expect("cassette lock").misses += 1;
    }
}

fn cassette_miss(dir: &Path, request: &ModelRequest, key: &str) -> Error {
    tracing::warn!(schema = %request.schema_name, model = %request.model, %key, "cassette miss");
    Error::Model(format!(
        "cassette miss in {}: no recorded answer for a `{}` call to `{}` (key {key}). \
         The prompt changed since this was recorded — re-record the corpus, or replay \
         loosely and accept that the numbers describe the old prompt.",
        dir.display(),
        request.schema_name,
        request.model,
    ))
}

fn review_key(
    request: &ModelRequest,
    revision: Option<&str>,
    policy: &crate::config::types::LookupPolicy,
) -> Result<String> {
    let policy = serde_json::to_value(policy)?;
    let mut hasher = Sha256::new();
    hasher.update(b"tinysweeper-cassette-review-v1\0");
    hasher.update(key(request).as_bytes());
    hasher.update(b"\0policy\0");
    hasher.update(canonical_schema(&policy).to_string().as_bytes());
    hasher.update(b"\0revision\0");
    if let Some(revision) = revision {
        hasher.update(b"known\0");
        hasher.update(revision.as_bytes());
    } else {
        hasher.update(b"unknown");
    }
    Ok(hex_digest(&hasher.finalize()))
}

fn hex_digest(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

struct QueryRecordingTree<'a> {
    inner: &'a dyn crate::ports::tree::TreeReader,
    recorded: Mutex<Vec<RecordedQuery>>,
}

impl<'a> QueryRecordingTree<'a> {
    fn new(inner: &'a dyn crate::ports::tree::TreeReader) -> Self {
        Self {
            inner,
            recorded: Mutex::new(Vec::new()),
        }
    }

    fn recorded(&self) -> Vec<RecordedQuery> {
        self.recorded
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn record(&self, query: RecordedTreeQuery, result: &Result<crate::ports::tree::Found>) {
        let fingerprint = match result {
            Ok(found) => {
                let canonical = canonical_schema(
                    &serde_json::to_value(found).expect("Found always serializes"),
                );
                hex_digest(&Sha256::digest(
                    serde_json::to_vec(&canonical).expect("canonical Found serializes"),
                ))
            }
            Err(_) => "failed".into(),
        };
        self.recorded
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(RecordedQuery {
                query,
                result: fingerprint,
            });
    }
}

#[async_trait]
impl crate::ports::tree::TreeReader for QueryRecordingTree<'_> {
    async fn lookup(
        &self,
        lookup: &crate::ports::tree::Lookup,
    ) -> Result<crate::ports::tree::Found> {
        let result = self.inner.lookup(lookup).await;
        self.record(RecordedTreeQuery::Lookup(lookup.clone()), &result);
        result
    }

    async fn explore(
        &self,
        query: &crate::ports::tree::TreeQuery,
    ) -> Result<crate::ports::tree::Found> {
        let recorded = match query {
            crate::ports::tree::TreeQuery::List { path, limit } => RecordedTreeQuery::List {
                path: path.clone(),
                limit: *limit,
            },
            crate::ports::tree::TreeQuery::Symbol { symbol, limit } => RecordedTreeQuery::Symbol {
                symbol: symbol.clone(),
                limit: *limit,
            },
            crate::ports::tree::TreeQuery::History {
                commit,
                path,
                start,
                end,
            } => RecordedTreeQuery::History {
                commit: commit.clone(),
                path: path.clone(),
                start: *start,
                end: *end,
            },
        };
        let result = self.inner.explore(query).await;
        self.record(recorded, &result);
        result
    }

    fn describe(&self) -> String {
        self.inner.describe()
    }

    fn revision(&self) -> Option<String> {
        self.inner.revision()
    }
}

struct QueryReplayTree<'a> {
    inner: &'a dyn crate::ports::tree::TreeReader,
}

impl<'a> QueryReplayTree<'a> {
    fn new(inner: &'a dyn crate::ports::tree::TreeReader) -> Self {
        Self { inner }
    }

    async fn reissue(&self, expected: &RecordedQuery) -> Result<()> {
        use crate::ports::tree::TreeQuery;

        let result = match &expected.query {
            RecordedTreeQuery::Lookup(lookup) => self.inner.lookup(lookup).await,
            RecordedTreeQuery::List { path, limit } => {
                self.inner
                    .explore(&TreeQuery::List {
                        path: path.clone(),
                        limit: *limit,
                    })
                    .await
            }
            RecordedTreeQuery::Symbol { symbol, limit } => {
                self.inner
                    .explore(&TreeQuery::Symbol {
                        symbol: symbol.clone(),
                        limit: *limit,
                    })
                    .await
            }
            RecordedTreeQuery::History {
                commit,
                path,
                start,
                end,
            } => {
                self.inner
                    .explore(&TreeQuery::History {
                        commit: commit.clone(),
                        path: path.clone(),
                        start: *start,
                        end: *end,
                    })
                    .await
            }
        };
        let matches = match (expected.result.as_str(), result) {
            ("failed", Err(_)) => true,
            ("failed", Ok(_)) => false,
            (expected_digest, Ok(found)) => {
                let canonical = canonical_schema(
                    &serde_json::to_value(found).expect("Found always serializes"),
                );
                let actual = hex_digest(&Sha256::digest(
                    serde_json::to_vec(&canonical).expect("canonical Found serializes"),
                ));
                actual == expected_digest
            }
            (_, Err(_)) => false,
        };
        if matches {
            Ok(())
        } else {
            Err(Error::Model("cassette review source changed".into()))
        }
    }
}

#[async_trait]
impl Model for Cassette {
    async fn complete(&self, request: ModelRequest) -> Result<ModelResponse> {
        let key = key(&request);

        if self.mode == Mode::Record {
            let inner = self
                .inner
                .as_ref()
                .ok_or_else(|| Error::Model("cassette is recording with no model".into()))?;
            let response = match inner.complete(request.clone()).await {
                Ok(response) => response,
                Err(error) => {
                    self.record_error(&error);
                    return Err(error);
                }
            };
            let take = self.make_take(key, &request, &response, None);
            let mut state = self.state.lock().expect("cassette lock");
            state.recorded.push(take);
            state.served += 1;
            state.spend.record(&response.model, response.usage);
            return Ok(response);
        }

        let mut state = self.state.lock().expect("cassette lock");
        let cursor = state.served;
        state.served += 1;

        if let Some(take) = self.takes.get(&key) {
            state.spend.record(&take.model_answered, take.usage);
            return Ok(replayed(take));
        }

        if self.mode == Mode::Strict {
            state.misses += 1;
            // Named at warn level as well as returned, because the error is
            // aggregated by the time a human reads it and "which call" is the
            // first thing they need in order to re-record the right thing.
            tracing::warn!(
                schema = %request.schema_name,
                model = %request.model,
                %key,
                "cassette miss"
            );
            return Err(Error::Model(format!(
                "cassette miss in {}: no recorded answer for a `{}` call to `{}` (key {key}). \
                 The prompt changed since this was recorded — re-record the corpus, or replay \
                 loosely and accept that the numbers describe the old prompt.",
                self.dir.display(),
                request.schema_name,
                request.model,
            )));
        }

        // Loose: fall back to call order, which survives a cosmetic edit.
        match self.ordered.get(cursor) {
            Some(take) if take.review_context.is_none() => {
                state.loose_hits += 1;
                state.spend.record(&take.model_answered, take.usage);
                Ok(replayed(take))
            }
            Some(_) => {
                state.misses += 1;
                Err(Error::Model(
                    "cassette method mismatch: completion cannot replay a repository-aware review"
                        .into(),
                ))
            }
            None => Err(Error::Model(format!(
                "cassette in {} is exhausted: the run made {} calls, the recording holds {}",
                self.dir.display(),
                cursor + 1,
                self.ordered.len(),
            ))),
        }
    }

    fn complete_reports_physical_calls(&self) -> bool {
        self.inner
            .as_ref()
            .is_some_and(|inner| inner.complete_reports_physical_calls())
    }

    fn review_reports_physical_calls(&self) -> bool {
        self.inner
            .as_ref()
            .is_some_and(|inner| inner.review_reports_physical_calls())
    }

    fn reports_raw_completion_usage(&self) -> bool {
        self.mode == Mode::Record
            && self
                .inner
                .as_ref()
                .is_some_and(|inner| inner.reports_raw_completion_usage())
    }

    async fn review(
        &self,
        request: ModelRequest,
        tree: &dyn crate::ports::tree::TreeReader,
        policy: &crate::config::types::LookupPolicy,
    ) -> Result<ModelResponse> {
        let revision = tree.revision();
        let key = review_key(&request, revision.as_deref(), policy)?;
        if self.mode == Mode::Record {
            let inner = self
                .inner
                .as_ref()
                .ok_or_else(|| Error::Model("cassette is recording with no model".into()))?;
            let traced = QueryRecordingTree::new(tree);
            let response = match inner.review(request.clone(), &traced, policy).await {
                Ok(response) => response,
                Err(error) => {
                    self.record_error(&error);
                    return Err(error);
                }
            };
            let take = self.make_take(
                key,
                &request,
                &response,
                Some(ReviewContext {
                    policy: policy.clone(),
                    revision,
                    queries: traced.recorded(),
                }),
            );
            let mut state = self.state.lock().expect("cassette lock");
            state.recorded.push(take);
            state.served += 1;
            state.spend.record(&response.model, response.usage);
            return Ok(response);
        }

        let (take, loose) = {
            let mut state = self.state.lock().expect("cassette lock");
            let cursor = state.served;
            state.served += 1;
            if let Some(take) = self.takes.get(&key) {
                (Some(take.clone()), false)
            } else if self.mode == Mode::Loose {
                match self.ordered.get(cursor) {
                    Some(take) if take.review_context.is_some() => (Some(take.clone()), true),
                    _ => (None, false),
                }
            } else {
                (None, false)
            }
        };
        let Some(take) = take else {
            self.record_miss();
            return Err(cassette_miss(&self.dir, &request, &key));
        };
        let Some(context) = &take.review_context else {
            self.record_miss();
            return Err(Error::Model(
                "cassette method mismatch: review needs a repository-aware take".into(),
            ));
        };

        let replay_tree = QueryReplayTree::new(tree);
        for query in &context.queries {
            replay_tree.reissue(query).await.map_err(|_| {
                self.record_miss();
                Error::Model("cassette review source or query sequence changed".into())
            })?;
        }
        let mut state = self.state.lock().expect("cassette lock");
        if loose {
            state.loose_hits += 1;
        }
        state.spend.record(&take.model_answered, take.usage);
        Ok(replayed(&take))
    }

    fn scoped_budget(&self, budget_usd: f64) -> Option<Arc<dyn Model>> {
        self.inner
            .as_ref()?
            .scoped_budget(budget_usd)
            .map(|inner| Arc::new(self.with_inner(inner)) as Arc<dyn Model>)
    }

    fn with_review_budget(&self, limit_usd: f64) -> Option<Arc<dyn Model>> {
        self.inner
            .as_ref()?
            .with_review_budget(limit_usd)
            .map(|inner| Arc::new(self.with_inner(inner)) as Arc<dyn Model>)
    }

    fn with_execution_control(
        &self,
        control: Arc<dyn crate::ports::model_execution::ModelExecutionControl>,
    ) -> Option<Arc<dyn Model>> {
        self.inner
            .as_ref()?
            .with_execution_control(control)
            .map(|inner| Arc::new(self.with_inner(inner)) as Arc<dyn Model>)
    }
}

/// Rebuild a response from a recorded take.
///
/// Usage is replayed verbatim rather than re-derived through
/// `crate::harness::pricing`, so an offline re-score reproduces the cost figure
/// the live run actually paid — including for a model the price table has since
/// changed, or never knew.
fn replayed(take: &Take) -> ModelResponse {
    ModelResponse {
        value: take.value.clone(),
        model: take.model_answered.clone(),
        usage: take.usage,
    }
}

/// The content hash identifying one request.
///
/// Covers everything that can change an answer: which model, what schema, how
/// many tokens it may spend, and every byte of every message. Sixteen hex
/// characters, hand-rolled the way `Finding::fingerprint` is — sha2 0.11
/// returns an array that does not implement `LowerHex`, and a hex crate is not
/// worth a dependency in the offline default build.
pub fn key(request: &ModelRequest) -> String {
    let mut hasher = Sha256::new();
    hasher.update(request.model.as_bytes());
    hasher.update(b"\0");
    hasher.update(request.schema_name.as_bytes());
    hasher.update(b"\0");
    if let Some(effort) = &request.reasoning_effort {
        hasher.update(b"reasoning-effort\0");
        hasher.update(effort.as_bytes());
        hasher.update(b"\0");
    }
    // The schema is the output contract, not just its name a provider wants:
    // two responses with the same name but different properties are different
    // answers. `serde_json`'s key ordering is a feature flag, not a property:
    // the `serve` build pulls in `bson`, which turns on `preserve_order`, and
    // the same schema then serializes its map keys in insertion order — the
    // schema bytes differ between two builds of this very crate that agree on
    // every affordance of the contract. Hash a canonical form instead: map
    // keys sorted at every depth, so the key is the schema no matter which
    // serde_json this build links.
    hasher.update(canonical_schema(&request.schema).to_string().as_bytes());
    hasher.update(b"\0");
    hasher.update(request.max_tokens.to_le_bytes());
    for message in &request.messages {
        hasher.update(b"\0");
        hasher.update(role_name(message).as_bytes());
        hasher.update(b"\0");
        hasher.update(message.content.as_bytes());
        // Only when present, so every recording made before images existed
        // keeps the key it was filed under. Two caption requests that differ
        // only in which screenshot they show must still hash apart.
        for image in &message.images {
            hasher.update(b"\0img\0");
            hasher.update(image.as_bytes());
        }
    }
    hasher
        .finalize()
        .iter()
        .take(8)
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// The wire name of a message's role.
fn role_name(message: &Message) -> &'static str {
    use crate::ports::model::Role;
    match message.role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
    }
}

/// A schema with its object keys sorted at every depth.
///
/// `serde_json`'s key ordering is a library feature (`preserve_order`), not a
/// property of the value — the `serve` build enables it via `bson`, and the
/// same schema then stringifies differently. The cassette key hashes the
/// schema, so it must hash the *contract*, which canonical sorting makes
/// independent of which serde_json is linked.
fn canonical_schema(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let sorted = keys
                .into_iter()
                .map(|key| (key.clone(), canonical_schema(&map[key])));
            serde_json::Value::Object(sorted.collect())
        }
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.iter().map(canonical_schema).collect())
        }
        other => other.clone(),
    }
}

/// Read every take in `dir`, in filename order.
fn load(dir: &Path) -> Result<Vec<Take>> {
    let entries = std::fs::read_dir(dir).map_err(|err| {
        Error::path(
            dir,
            format!("no cassette here ({err}); record one with `eval run --record`"),
        )
    })?;

    let mut paths: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .collect();
    // Filename order is call order, because the sequence number leads.
    paths.sort();

    let mut takes = Vec::with_capacity(paths.len());
    for path in paths {
        let raw = std::fs::read_to_string(&path).map_err(|err| Error::path(&path, err))?;
        let take: Take = serde_json::from_str(&raw)
            .map_err(|err| Error::path(&path, format!("not a cassette take: {err}")))?;
        takes.push(take);
    }
    Ok(takes)
}

#[cfg(test)]
#[path = "cassette_test.rs"]
mod tests;
