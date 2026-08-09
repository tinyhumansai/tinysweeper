//! Keeping the server's code index current. Requires `serve`.
//!
//! This is the module that turns the indexing stack on. Everything under
//! `crate::indexer` and `crate::index` has been complete and inert: nothing
//! implemented [`Embedder`](crate::ports::embed::Embedder) against a real
//! provider, so no server could fill an index and a retriever could only ever
//! report `Cold`. What was missing was this seam.
//!
//! # Indexing does not block the review
//!
//! A cold full index of a large repository is thousands of embedding calls and
//! minutes of wall clock. A review is expected within seconds. So indexing runs
//! as its own task with its own permit pool, and the review proceeds against
//! whatever index exists right now — which on the first push is none, and which
//! `crate::retrieve` already degrades through honestly. The alternative,
//! blocking the first review of a repository on its first full index, trades a
//! thin review for a late one, and a late review is the one nobody reads.
//!
//! # A refused claim is requeued, never waited on
//!
//! [`IndexManifest::claim`](crate::ports::manifest::IndexManifest::claim)
//! answers `Busy` when another worker holds the repository, and the answer is
//! *requeue*. A worker that blocked on it would be a worker not indexing
//! anything else, and with a bounded permit pool a few blocked workers are the
//! whole pool. Requeues are delayed, bounded, and give up quietly: the holder
//! is doing the work, so losing the race is not a failure.

use std::sync::Arc;

use crate::config::Config;
use crate::error::{Error, Result};
use crate::forge::RepoId;
use crate::graph::types::SourceFile;
use crate::index::mongo::MongoIndex;
use crate::index::types::EmbedSignature;
use crate::indexer::fetch::Checkout;
use crate::indexer::mongo::MongoManifest;
use crate::indexer::run::Indexer;
use crate::indexer::types::IndexOutcome;
use crate::ports::embed::Embedder;

/// Which host repositories are fetched from.
///
/// Overridable for GitHub Enterprise Server, which serves git from the same
/// host as its API rather than from github.com.
const GIT_HOST_ENV: &str = "TINYSWEEPER_GIT_HOST";

/// How many times a requeued repository is retried before it is left alone.
///
/// Bounded because the holder is doing the same work: giving up costs nothing
/// but the freshness of an index that is about to be refreshed anyway. The next
/// push tries again regardless.
const REQUEUE_ATTEMPTS: usize = 3;

/// How long to wait before retrying a repository another worker holds.
const REQUEUE_DELAY: std::time::Duration = std::time::Duration::from_secs(30);

/// Cap on the bytes of one file the graph builder will read.
///
/// The chunker has its own selector-level cap; this one bounds the *second*
/// pass, which holds every file's text in memory at once to resolve symbols
/// across the repository.
const MAX_GRAPH_FILE_BYTES: u64 = 512 * 1024;

/// The stores and the provider that indexing needs.
///
/// Held as one value so the server either has a complete indexing stack or
/// none: a deployment with an embedder and no database, or a database and no
/// embedder, can do nothing useful with either and should not have to be
/// handled at every call site.
pub struct IndexBackend {
    /// The provider-backed embedder. Its signature partitions the index.
    pub embedder: Arc<dyn Embedder>,
    /// Every retrieval store, over the same database as `server::store`.
    pub index: Arc<MongoIndex>,
    /// The freshness record and the claim.
    pub manifest: Arc<MongoManifest>,
    /// The partition key, cached from the embedder.
    pub signature: EmbedSignature,
}

impl std::fmt::Debug for IndexBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IndexBackend")
            .field("signature", &self.signature.key())
            .finish_non_exhaustive()
    }
}

impl IndexBackend {
    /// Open the indexing stack described by `[embeddings]`, if there is one.
    ///
    /// `Ok(None)` means the operator did not configure a provider, which is a
    /// supported deployment: reviews run diff-only. An error means one *was*
    /// configured and could not be opened, which is a mistake rather than a
    /// choice and must not be papered over as "retrieval off" — a silently
    /// unindexed reviewer still posts reviews, just worse ones.
    pub async fn open(config: &Config) -> Result<Option<Self>> {
        let Some(embedder) = crate::index::embedder_from_config(&config.embeddings)? else {
            return Ok(None);
        };
        let signature = embedder.signature();

        let index = MongoIndex::from_env().await?;
        // The boot assertion, on the collection reviews actually query. A
        // deployment whose mongot is missing must fail here rather than on a
        // contributor's pull request.
        index.prepare(&signature).await?;

        let manifest = MongoManifest::connect(&mongo_uri()?, &mongo_db()).await?;

        Ok(Some(Self {
            embedder,
            index: Arc::new(index),
            manifest: Arc::new(manifest),
            signature,
        }))
    }

    /// Bring `repo`'s index up to `revision`, fetching the tree if it must.
    ///
    /// The freshness check comes **before** the fetch, deliberately: an index
    /// that already reflects this commit must cost neither a clone nor a
    /// claim, and on a busy repository most deliveries are exactly that case.
    pub async fn ensure_indexed(
        &self,
        config: &Config,
        repo: &RepoId,
        revision: &str,
        token: &str,
    ) -> Result<IndexOutcome> {
        use crate::ports::manifest::IndexManifest;

        let repo_id = repo.to_string();
        if self
            .manifest
            .state(&repo_id, &self.signature)
            .await?
            .is_fresh(revision)
        {
            return Ok(IndexOutcome::AlreadyFresh);
        }

        // Read-only, and read-only on purpose: this is the same boundary the
        // review runs against. The write token is minted separately, in
        // `routes.rs`, after every model call has returned.
        let checkout = Checkout::fetch(&git_host(), &repo_id, revision, token).await?;

        let selector = crate::chunk::Selector::new(&config.paths.ignore)?;
        let indexer = Indexer::new(
            self.embedder.as_ref(),
            &self.index.code,
            self.manifest.as_ref(),
        )?
        .with_selector(selector)
        .with_batch(config.embeddings.batch)
        // The ceiling is per repository per run. Without it, one monorepo is an
        // unbounded bill discovered on an invoice.
        .with_budget(config.embeddings.budget_usd_per_index)
        .as_holder(format!("server-{}", std::process::id()));

        let outcome = indexer
            .index_repo(&repo_id, revision, checkout.path())
            .await?;

        if let IndexOutcome::Indexed(report) = &outcome {
            // The one line an operator needs to answer "what is this costing".
            // `summary()` names the partial case explicitly.
            tracing::info!(
                repo = %repo_id,
                signature = %self.signature,
                cost_usd = report.usage.cost_usd,
                tokens = report.usage.tokens,
                "indexed: {}",
                report.summary()
            );
            // The graph is what turns "code that reads like the diff" into "the
            // caller this change breaks", so it is rebuilt from the same
            // checkout rather than left to a second fetch.
            if let Err(err) = self.sync_graph(&repo_id, &checkout, config).await {
                // A graph failure costs expansion, not retrieval: the chunks are
                // already written and queryable. Failing the whole run here
                // would throw away an index that just cost money.
                tracing::warn!(%err, repo = %repo_id, "could not rebuild the code graph");
            }
        }

        Ok(outcome)
    }

    /// Rebuild the code graph from a checkout already on disk.
    async fn sync_graph(&self, repo_id: &str, checkout: &Checkout, config: &Config) -> Result<()> {
        let selector = crate::chunk::Selector::new(&config.paths.ignore)?;
        let selection = selector.walk(checkout.path())?;

        let mut files: Vec<SourceFile> = Vec::new();
        for path in &selection.selected {
            let full = checkout.path().join(path);
            let readable = std::fs::metadata(&full)
                .map(|meta| meta.len() <= MAX_GRAPH_FILE_BYTES)
                .unwrap_or(false);
            // Lossless or nothing: a file that is not UTF-8 has no symbols this
            // build can extract, and lossy-converting it would invent them.
            if readable && let Ok(text) = std::fs::read_to_string(&full) {
                files.push(SourceFile::new(path.clone(), text));
            }
        }

        let graph = crate::graph::build::build(repo_id, &files)?;
        let written = crate::graph::build::sync_all(&self.index.graph, repo_id, &graph).await?;
        tracing::info!(repo = repo_id, nodes = written, "code graph rebuilt");
        Ok(())
    }
}

/// Index `repo` in the background, requeueing rather than waiting on a claim.
///
/// Spawned by the review path and deliberately not awaited by it: see the
/// module docs. Errors are logged rather than propagated, because a failed
/// index degrades a review and must not fail one.
pub async fn index_in_background(
    backend: Arc<IndexBackend>,
    config: Arc<Config>,
    permits: Arc<tokio::sync::Semaphore>,
    repo: RepoId,
    revision: String,
    token: String,
) {
    let Ok(_permit) = permits.acquire_owned().await else {
        return;
    };

    let name = repo.to_string();
    with_requeue(&name, REQUEUE_ATTEMPTS, REQUEUE_DELAY, || {
        backend.ensure_indexed(&config, &repo, &revision, &token)
    })
    .await;
}

/// Run `attempt` until it stops answering [`IndexOutcome::Requeue`].
///
/// Split out of [`index_in_background`] so the contention behaviour is testable
/// without a database: what matters here is that a refused claim *retries after
/// a delay* and eventually gives up, rather than blocking a worker on a lock,
/// and that is a property of this loop rather than of MongoDB.
async fn with_requeue<F, Fut>(
    repo: &str,
    attempts: usize,
    delay: std::time::Duration,
    mut attempt: F,
) -> Option<IndexOutcome>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<IndexOutcome>>,
{
    for round in 1..=attempts {
        match attempt().await {
            Ok(IndexOutcome::AlreadyFresh) => {
                tracing::debug!(repo, "the index already reflects this commit");
                return Some(IndexOutcome::AlreadyFresh);
            }
            Ok(outcome @ IndexOutcome::Indexed(_)) => return Some(outcome),
            Ok(IndexOutcome::Requeue { holder }) => {
                if round == attempts {
                    // Not an error. The holder is doing this work; the only cost
                    // of giving up is freshness, and the next push retries.
                    tracing::info!(
                        repo,
                        ?holder,
                        "another worker still holds the index claim; leaving it to them"
                    );
                    return Some(IndexOutcome::Requeue { holder });
                }
                tracing::debug!(repo, ?holder, round, "index claim held; requeueing");
                tokio::time::sleep(delay).await;
            }
            Err(err) => {
                tracing::warn!(%err, repo, "indexing failed; the review degrades");
                return None;
            }
        }
    }
    None
}

/// The git host repositories are fetched from.
fn git_host() -> String {
    std::env::var(GIT_HOST_ENV)
        .ok()
        .map(|host| {
            host.trim()
                .trim_start_matches("https://")
                .trim_end_matches('/')
                .to_string()
        })
        .filter(|host| !host.is_empty())
        .unwrap_or_else(|| "github.com".to_string())
}

fn mongo_uri() -> Result<String> {
    std::env::var("TINYSWEEPER_MONGODB_URI")
        .map_err(|_| Error::Forge("TINYSWEEPER_MONGODB_URI is not set".into()))
}

fn mongo_db() -> String {
    std::env::var("TINYSWEEPER_MONGODB_DB").unwrap_or_else(|_| "tinysweeper".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_git_host_defaults_to_github_and_tolerates_a_scheme() {
        // Safe to set: these tests do not run concurrently with anything that
        // reads the variable, and an Enterprise operator pasting a URL rather
        // than a hostname is the mistake worth absorbing.
        unsafe { std::env::remove_var(GIT_HOST_ENV) };
        assert_eq!(git_host(), "github.com");

        unsafe { std::env::set_var(GIT_HOST_ENV, "https://ghe.example.com/") };
        assert_eq!(git_host(), "ghe.example.com");
        unsafe { std::env::remove_var(GIT_HOST_ENV) };
    }

    #[tokio::test]
    async fn a_contended_claim_is_retried_and_then_left_to_its_holder() {
        // The convention this enforces: a refused claim is *requeued*, never
        // waited on. A worker that blocked here would be a worker not indexing
        // anything else, and with a bounded pool a few of those are the pool.
        let calls = std::sync::atomic::AtomicUsize::new(0);
        let outcome = with_requeue("o/r", 3, std::time::Duration::ZERO, || {
            calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            std::future::ready(Ok(IndexOutcome::Requeue {
                holder: Some("other-worker".into()),
            }))
        })
        .await;

        assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 3);
        assert!(matches!(outcome, Some(IndexOutcome::Requeue { .. })));
    }

    #[tokio::test]
    async fn a_claim_that_frees_up_is_taken_on_the_next_round() {
        let calls = std::sync::atomic::AtomicUsize::new(0);
        let outcome = with_requeue("o/r", 5, std::time::Duration::ZERO, || {
            let round = calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            std::future::ready(Ok(if round < 2 {
                IndexOutcome::Requeue { holder: None }
            } else {
                IndexOutcome::Indexed(crate::indexer::types::IndexReport::default())
            }))
        })
        .await;

        assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 3);
        assert!(matches!(outcome, Some(IndexOutcome::Indexed(_))));
    }

    #[tokio::test]
    async fn a_fresh_index_costs_exactly_one_round() {
        let calls = std::sync::atomic::AtomicUsize::new(0);
        with_requeue("o/r", 3, std::time::Duration::ZERO, || {
            calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            std::future::ready(Ok(IndexOutcome::AlreadyFresh))
        })
        .await;
        assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn a_failed_index_is_not_retried_as_though_it_were_contention() {
        // Retrying a provider outage three times would triple the failure and
        // hold the permit for it. Contention is the only retryable answer.
        let calls = std::sync::atomic::AtomicUsize::new(0);
        let outcome = with_requeue("o/r", 3, std::time::Duration::ZERO, || {
            calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            std::future::ready(Err(Error::Forge("the provider is down".into())))
        })
        .await;
        assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 1);
        assert!(outcome.is_none());
    }

    #[tokio::test]
    async fn no_embedding_provider_means_no_backend_rather_than_an_error() {
        // The supported deployment: reviews run diff-only, exactly as they did
        // before an index existed.
        let config = Config::default();
        assert!(
            IndexBackend::open(&config)
                .await
                .expect("a disabled section is a choice")
                .is_none()
        );
    }
}
