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
//! The cassette key covers the model id, the schema name, the token ceiling and
//! every message. So any change to a prompt invalidates every cassette that
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
use crate::ports::model::{Message, Model, ModelRequest, ModelResponse, Usage};

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
    /// The prompt, recorded only when explicitly asked for.
    ///
    /// Off by default because a prompt embeds the reviewed repository's diff,
    /// and a cassette committed from a private repository would carry that
    /// diff into git. Public-corpus cases can turn it on; it is the only way to
    /// debug a prompt change from a recording afterwards.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<Vec<RecordedMessage>>,
}

/// One message of a recorded prompt.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecordedMessage {
    /// `system`, `user` or `assistant`.
    pub role: String,
    /// The text.
    pub content: String,
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
    state: Mutex<Playback>,
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
            state: Mutex::new(Playback::default()),
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
            state: Mutex::new(Playback::default()),
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
            let response = inner.complete(request.clone()).await?;
            let take = Take {
                key,
                model_requested: request.model.clone(),
                model_answered: response.model.clone(),
                schema_name: request.schema_name.clone(),
                usage: response.usage,
                value: response.value.clone(),
                prompt: self.record_prompts.then(|| {
                    request
                        .messages
                        .iter()
                        .map(|message| RecordedMessage {
                            role: role_name(message).to_string(),
                            content: message.content.clone(),
                        })
                        .collect()
                }),
            };
            let mut state = self.state.lock().expect("cassette lock");
            state.recorded.push(take);
            state.served += 1;
            return Ok(response);
        }

        let mut state = self.state.lock().expect("cassette lock");
        let cursor = state.served;
        state.served += 1;

        if let Some(take) = self.takes.get(&key) {
            return Ok(replayed(take));
        }

        if self.mode == Mode::Strict {
            state.misses += 1;
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
            Some(take) => {
                state.loose_hits += 1;
                Ok(replayed(take))
            }
            None => Err(Error::Model(format!(
                "cassette in {} is exhausted: the run made {} calls, the recording holds {}",
                self.dir.display(),
                cursor + 1,
                self.ordered.len(),
            ))),
        }
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
