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
        }))
    }

    /// A recaller over the engine, for one review.
    pub fn recaller(&self) -> Recaller<'_> {
        Recaller::new(self.memory.as_ref())
    }

    /// Whether `repo` has been ingested at `revision` by this process.
    pub fn is_fresh(&self, repo: &str, revision: &str) -> bool {
        self.fresh
            .lock()
            .expect("freshness lock")
            .get(repo)
            .is_some_and(|known| known == revision)
    }

    /// Feed the engine `repo`'s tree at `revision`, fetching it if it must.
    ///
    /// `Ok(None)` when this process already did. The freshness check comes
    /// before the fetch so a busy repository's deliveries cost neither a
    /// clone nor a walk.
    pub async fn ensure_ingested(
        &self,
        config: &Config,
        repo: &RepoId,
        revision: &str,
        token: &str,
    ) -> Result<Option<IngestReport>> {
        let repo_id = repo.to_string();
        if self.is_fresh(&repo_id, revision) {
            return Ok(None);
        }
        if !config.memory.ingest_code && !config.memory.ingest_conventions {
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
            .insert(repo_id, revision.to_string());
        Ok(Some(report))
    }
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
