//! Keeping the server's memory engine fed. Requires `serve`.
//!
//! The counterpart of [`super::indexing`] for the memory layer: it is what
//! turns [`crate::memory`] on. Everything under it is complete and inert
//! without this seam — nothing implements [`Memory`] against a real engine
//! except [`CortexMemory`], and nothing feeds it a checkout except this.
//!
//! # Ingest reads the *base* branch
//!
//! The index is built from the pull request's head, because retrieval wants
//! the code the change lives in. Memory is built from the pull request's
//! base, deliberately: the conventions it holds are the policy the repository
//! has committed to, not the one a pull request proposes, and a memory a pull
//! request could write to before it merged would be a memory a contributor
//! could poison. A pull request that edits `AGENTS.md` is remembered once it
//! lands, on the next review of anything.
//!
//! # Ingest does not block the review
//!
//! For the same reason indexing does not: a full ingest of a large repository
//! is thousands of engine writes, each held until indexed, and a review is
//! expected in seconds. The review recalls whatever the engine holds right
//! now, and `crate::memory::recall` says so when that is nothing.
//!
//! # Freshness is per process
//!
//! One ingest per base tip per process, tracked in memory. The engine
//! deduplicates on content, so a second process ingesting the same tip pays
//! the network and writes nothing; a manifest for memory would save that cost
//! and nothing else, and is not worth a collection yet.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tokio::sync::Mutex as AsyncMutex;

use crate::config::Config;
use crate::error::Result;
use crate::forge::RepoId;
use crate::indexer::fetch::Checkout;
use crate::memory::cortex::CortexMemory;
use crate::memory::{IngestReport, Ingestor, Recaller};
use crate::ports::memory::Memory;

/// The engine, and what it has been fed.
pub struct MemoryBackend {
    /// The engine. `Arc` so a background ingest can hold it past the request.
    pub memory: Arc<CortexMemory>,
    /// The base revision each repository was last ingested at, this process.
    fresh: Mutex<HashMap<String, String>>,
    /// One async lock per repository, so that concurrent deliveries for the
    /// same base tip serialize on the checkout and ingest instead of each
    /// racing the freshness check and cloning independently.
    ///
    /// A plain `Mutex` will not do: the section it guards awaits a clone and
    /// an ingest, and holding a sync lock across an `.await` blocks the
    /// runtime thread rather than yielding it.
    ingesting: Mutex<HashMap<String, Arc<AsyncMutex<()>>>>,
}

impl std::fmt::Debug for MemoryBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryBackend")
            .field("engine", &self.memory.name())
            .finish_non_exhaustive()
    }
}

impl MemoryBackend {
    /// Open the engine described by `[memory]`, if there is one.
    ///
    /// `Ok(None)` means the operator did not turn memory on, which is a
    /// supported deployment. An error means one *was* configured and could
    /// not be reached, which is a mistake rather than a choice and must not
    /// be papered over as "memory off": a silently forgetful reviewer still
    /// posts reviews, just ones that repeat themselves.
    pub async fn open(config: &Config) -> Result<Option<Self>> {
        if !config.memory.enabled {
            return Ok(None);
        }
        let memory = CortexMemory::from_config(&config.memory)?;
        memory.health().await?;
        Ok(Some(Self {
            memory: Arc::new(memory),
            fresh: Mutex::new(HashMap::new()),
            ingesting: Mutex::new(HashMap::new()),
        }))
    }

    /// A recaller over the engine, for one review.
    pub fn recaller(&self) -> Recaller<'_> {
        Recaller::new(self.memory.as_ref())
    }

    /// Whether `repo` has been ingested at `revision` by this process.
    pub fn is_fresh(&self, repo: &str, revision: &str, config: &Config) -> bool {
        self.fresh
            .lock()
            .expect("freshness lock")
            .get(repo)
            .is_some_and(|known| *known == freshness_key(revision, config))
    }

    /// The per-repository lock that serializes `ensure_ingested`, creating it
    /// on first use.
    fn ingest_lock(&self, repo_id: &str) -> Arc<AsyncMutex<()>> {
        self.ingesting
            .lock()
            .expect("ingest lock table")
            .entry(repo_id.to_string())
            .or_insert_with(|| Arc::new(AsyncMutex::new(())))
            .clone()
    }

    /// Feed the engine `repo`'s tree at `revision`, fetching it if it must.
    ///
    /// `Ok(None)` when this process already did. The freshness check comes
    /// before the fetch so a busy repository's deliveries cost neither a
    /// clone nor a walk. Concurrent calls for the same repository serialize
    /// on [`Self::ingest_lock`]: without it, a burst of deliveries sharing a
    /// base tip could all observe a miss before any of them recorded the
    /// revision, and each would clone and ingest it independently.
    pub async fn ensure_ingested(
        &self,
        config: &Config,
        repo: &RepoId,
        revision: &str,
        token: &str,
    ) -> Result<Option<IngestReport>> {
        let repo_id = repo.to_string();
        if self.is_fresh(&repo_id, revision, config) {
            return Ok(None);
        }
        if !config.memory.ingest_code && !config.memory.ingest_conventions {
            return Ok(None);
        }
        let lock = self.ingest_lock(&repo_id);
        let _guard = lock.lock().await;
        // Re-check now that this call holds the repository's lock: another
        // task may have ingested this exact revision while this one waited.
        if self.is_fresh(&repo_id, revision, config) {
            return Ok(None);
        }
        // Read-only, like the index's checkout: the same boundary the review
        // runs against, and no write token anywhere near it.
        let checkout =
            Checkout::fetch(&super::indexing::git_host(), &repo_id, revision, token).await?;
        let ingestor = Ingestor::new(self.memory.as_ref(), &config.memory, &config.paths.ignore)?;
        let report = ingestor.ingest_checkout(&repo_id, checkout.path()).await?;
        tracing::info!(repo = %repo_id, revision, "memory ingested: {}", report.summary());
        self.fresh
            .lock()
            .expect("freshness lock")
            .insert(repo_id, freshness_key(revision, config));
        Ok(Some(report))
    }
}

/// The freshness cache key for `revision` under `config`.
///
/// Keyed on the *effective ingestion policy*, not the revision alone: if the
/// first delivery for a base SHA races a transient failure to fetch the
/// repository's own configuration overlay and falls back to the
/// deployment's, ingesting under that fallback and marking the plain
/// revision fresh would make a later delivery — one that *did* load the
/// repository's real `paths.ignore` — skip re-ingesting, leaving paths the
/// repository explicitly excluded stored and recallable until the base
/// branch moves again. `paths.ignore`, `memory.ingest_code`,
/// `memory.ingest_conventions`, and `memory.convention_files` are exactly
/// what `Ingestor::new` and the selector it builds are constructed from.
fn freshness_key(revision: &str, config: &Config) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    config.paths.ignore.hash(&mut hasher);
    config.memory.ingest_code.hash(&mut hasher);
    config.memory.ingest_conventions.hash(&mut hasher);
    config.memory.convention_files.hash(&mut hasher);
    format!("{revision}#{:x}", hasher.finish())
}

/// Ingest `repo` in the background.
///
/// Spawned by the review path and deliberately not awaited by it: see the
/// module docs. Errors are logged rather than propagated, because a failed
/// ingest degrades a review and must not fail one. Shares the index permit
/// pool, so a delivery burst cannot turn into a burst of clones.
pub async fn ingest_in_background(
    backend: Arc<MemoryBackend>,
    config: Arc<Config>,
    permits: Arc<tokio::sync::Semaphore>,
    repo: RepoId,
    revision: String,
    token: String,
) {
    let Ok(_permit) = permits.acquire_owned().await else {
        return;
    };
    if let Err(err) = backend
        .ensure_ingested(&config, &repo, &revision, &token)
        .await
    {
        tracing::warn!(%err, repo = %repo, "memory ingest failed; the next review recalls what the engine already holds");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_disabled_memory_opens_to_nothing() {
        let config: Config = crate::config::DEFAULTS
            .parse::<toml::Table>()
            .unwrap()
            .try_into()
            .unwrap();
        assert!(!config.memory.enabled);
        assert!(MemoryBackend::open(&config).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn ingest_lock_is_shared_per_repository_and_serializes_holders() {
        // Regression for the freshness race: two concurrent `ensure_ingested`
        // calls for the same repository must contend on the *same* lock, not
        // each get their own, or the check-then-ingest window stays racy.
        // `CortexMemory::new` performs no I/O — it only builds an HTTP client
        // — so it is safe to construct directly here without a real engine.
        let backend = MemoryBackend {
            memory: Arc::new(
                CortexMemory::new("http://127.0.0.1:1", "test-key").expect("client builds"),
            ),
            fresh: Mutex::new(HashMap::new()),
            ingesting: Mutex::new(HashMap::new()),
        };

        let a = backend.ingest_lock("o/same");
        let b = backend.ingest_lock("o/same");
        assert!(
            Arc::ptr_eq(&a, &b),
            "the same repository must contend on one lock"
        );

        let other = backend.ingest_lock("o/other");
        assert!(
            !Arc::ptr_eq(&a, &other),
            "a different repository must not share the lock"
        );

        // While `a` is held, a second acquisition on the same lock must wait
        // for it to be released rather than resolving immediately.
        let guard = a.lock().await;
        let held = Arc::clone(&b);
        let waiter = tokio::spawn(async move {
            let _second_guard = held.lock().await;
        });
        // Give the spawned task a chance to run and observe it is still
        // blocked on the held lock.
        tokio::task::yield_now().await;
        assert!(!waiter.is_finished(), "the second acquisition must block");
        drop(guard);
        waiter.await.expect("the waiter completes once released");
    }

    #[tokio::test]
    async fn an_enabled_memory_with_no_key_refuses_to_open() {
        let mut config: Config = crate::config::DEFAULTS
            .parse::<toml::Table>()
            .unwrap()
            .try_into()
            .unwrap();
        config.memory.enabled = true;
        config.memory.endpoint = "http://127.0.0.1:1".into();
        config.memory.api_key_env = "TINYSWEEPER_TEST_NO_SUCH_CORTEX_KEY".into();
        let err = MemoryBackend::open(&config).await.unwrap_err();
        assert!(err.to_string().contains("is not set"), "{err}");
    }
}
