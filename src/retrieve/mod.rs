//! Retrieval: what a reviewer is shown besides the diff.
//!
//! Always compiled. Everything here goes through the [`Embedder`],
//! [`ChunkIndex`], [`GraphStore`] and [`IndexManifest`] ports, so the whole
//! pipeline runs offline against `crate::index::mock` and the default build
//! still links no database driver.
//!
//! Four steps, one module each:
//!
//! 1. [`query`] composes a bounded query out of the pull request — never the
//!    raw diff, so embedding cost does not scale with diff size.
//! 2. One hybrid `$rankFusion` query fuses a dense arm with a BM25 arm. The
//!    fusion is the search engine's job; hand-rolling reciprocal rank fusion
//!    here would mean over-fetching both arms and throwing away corpus IDF.
//! 3. [`expand`] walks the code graph out from the symbols the diff touches and
//!    fetches the chunks of what it reaches — the caller a change breaks, which
//!    similarity search will never return because it shares no vocabulary with
//!    the diff.
//! 4. [`assemble`] ranks, deduplicates and truncates to a stated token budget,
//!    and counts what it dropped.
//!
//! # Degrading honestly
//!
//! Nothing in this module returns an error to its caller. A cold index, a stale
//! one, an unreachable database or a deployment whose `mongot` is missing all
//! produce a [`RetrievedContext`] carrying a [`RetrievalStatus`] that says so,
//! and the review runs on the diff alone. Two properties matter and they are
//! not the same one:
//!
//! - Losing retrieval must not lose the review. Handing every contributor a way
//!   to break the bot by taking a database down would be worse than a thinner
//!   review.
//! - Losing retrieval must not be *silent*. A review running with less context
//!   than the operator believes it has is the failure this whole module is
//!   least able to detect from the outside, so it is stated in the check-run
//!   summary rather than logged.

pub mod assemble;
pub mod expand;
pub mod query;
pub mod types;

use crate::config::types::Config;
use crate::evidence::diff::FileDiff;
use crate::index::types::{EmbedSignature, HybridQuery};
use crate::indexer::types::IndexState;
use crate::ports::embed::Embedder;
use crate::ports::graph::GraphStore;
use crate::ports::index::ChunkIndex;
use crate::ports::manifest::IndexManifest;
use crate::ports::model::Spend;

pub use crate::retrieve::assemble::{Assembled, assemble};
pub use crate::retrieve::expand::{Expansion, seeds};
pub use crate::retrieve::query::build_retrieval_query;
pub use crate::retrieve::types::{Provenance, RetrievalStatus, RetrievedChunk, RetrievedContext};

/// How many hits the search arm asks for.
///
/// Deliberately more than reaches the prompt: dedupe and the token budget both
/// remove candidates, and an over-fetch is one cheap query rather than a second
/// round trip. Bounded all the same — `$rankFusion` pays for its limit.
const SEARCH_OVERFETCH: usize = 3;

/// The stores a review retrieves from.
///
/// A borrow bundle rather than an owning struct so the server can hand over
/// whatever it has open. The graph and the manifest are optional and mean
/// different things by their absence: no graph is *no expansion*, and no
/// manifest is *no freshness claim* — retrieval still runs, but it cannot say
/// whether what it found reflects the commit under review, and it does not
/// pretend otherwise.
pub struct Retriever<'a> {
    /// Embeds the composed query. Its signature partitions the index.
    pub embedder: &'a dyn Embedder,
    /// The chunk index the hybrid query runs against.
    pub index: &'a dyn ChunkIndex,
    /// The code graph, when one is available.
    pub graph: Option<&'a dyn GraphStore>,
    /// The freshness record, when one is available.
    pub manifest: Option<&'a dyn IndexManifest>,
}

impl<'a> Retriever<'a> {
    /// Bundle an embedder and an index, with no graph and no manifest.
    pub fn new(embedder: &'a dyn Embedder, index: &'a dyn ChunkIndex) -> Self {
        Self {
            embedder,
            index,
            graph: None,
            manifest: None,
        }
    }

    /// Attach a code graph, enabling expansion.
    pub fn with_graph(mut self, graph: &'a dyn GraphStore) -> Self {
        self.graph = Some(graph);
        self
    }

    /// Attach the freshness record, enabling the cold and stale verdicts.
    pub fn with_manifest(mut self, manifest: &'a dyn IndexManifest) -> Self {
        self.manifest = Some(manifest);
        self
    }

    /// Gather the context for one review.
    ///
    /// Returns the [`Spend`] alongside, because embedding the query is a real
    /// charge and it goes through `crate::harness::pricing` like every other
    /// one. Retrieval that could produce context without a price would be a
    /// hole in exactly the budget the [`Embedder`] port already closed for
    /// indexing.
    pub async fn retrieve(
        &self,
        config: &Config,
        repo_id: &str,
        title: &str,
        head_sha: &str,
        diffs: &[FileDiff],
    ) -> (RetrievedContext, Spend) {
        let mut spend = Spend::default();
        if !config.retrieval.enabled {
            return (RetrievedContext::off(), spend);
        }

        let signature = self.embedder.signature();

        // Freshness first: a cold index cannot be made warm by querying it, and
        // the query costs money.
        let freshness = self.freshness(repo_id, &signature, head_sha).await;
        if freshness == Some(RetrievalStatus::Cold) {
            return (RetrievedContext::degraded(RetrievalStatus::Cold), spend);
        }

        let text = build_retrieval_query(title, diffs, config.retrieval.query_chars);
        if text.trim().is_empty() {
            return (RetrievedContext::degraded(RetrievalStatus::Ready), spend);
        }

        let embedded = match self.embedder.embed_query(&text).await {
            Ok(embedded) => embedded,
            Err(err) => return (unavailable(err), spend),
        };
        // Billed before anything can go wrong downstream: the call happened
        // whether or not its vector was ever used.
        spend.record(&signature.price_key(), embedded.usage);
        let vector = match embedded.into_query_vector() {
            Ok(vector) => vector,
            Err(err) => return (unavailable(err), spend),
        };

        let hits = match self
            .index
            .query(
                &HybridQuery::new(signature.clone(), text, vector)
                    .in_repo(repo_id)
                    .limit(config.retrieval.max_chunks * SEARCH_OVERFETCH),
            )
            .await
        {
            Ok(hits) => hits,
            // The `mongot`-is-missing case lands here, carrying the adapter's
            // own explanation of what the operator has to fix.
            Err(err) => return (unavailable(err), spend),
        };

        // Expansion is best-effort on top of a query that already succeeded: a
        // graph store that will not answer costs the blast radius, not the
        // review, and the status already reflects whether the index is whole.
        let expansion = match self.graph {
            Some(graph) => expand::expand(
                graph,
                self.index,
                &signature,
                repo_id,
                diffs,
                &config.retrieval,
            )
            .await
            .unwrap_or_else(|err| {
                tracing::warn!(%err, "graph expansion failed; retrieval falls back to search only");
                Expansion::default()
            }),
            None => Expansion::default(),
        };

        let changed: Vec<String> = diffs.iter().map(|diff| diff.path.clone()).collect();
        let assembled = assemble(
            hits,
            expansion.chunks,
            &changed,
            config.retrieval.max_chunks,
            config.retrieval.context_tokens,
        );

        (
            RetrievedContext {
                status: freshness.unwrap_or(RetrievalStatus::Ready),
                chunks: assembled.chunks,
                dropped: assembled.dropped,
                tokens: assembled.tokens,
                graph_nodes: expansion.nodes,
                impact: expansion.impact,
            },
            spend,
        )
    }

    /// What the manifest says about this repository's index.
    ///
    /// `None` means *no claim*: either no manifest is attached or it could not
    /// be read, and in both cases retrieval proceeds without asserting the
    /// index is fresh. A missing freshness record is not evidence of staleness
    /// and must not be reported as if it were — an operator who chases a stale
    /// index that is actually fine stops believing the notice.
    async fn freshness(
        &self,
        repo_id: &str,
        signature: &EmbedSignature,
        head_sha: &str,
    ) -> Option<RetrievalStatus> {
        let manifest = self.manifest?;
        let state = match manifest.state(repo_id, signature).await {
            Ok(state) => state,
            Err(err) => {
                tracing::warn!(%err, "could not read the index manifest; freshness is unknown");
                return None;
            }
        };

        match state.state {
            // Never indexed under this signature. Swapping the embedding model
            // lands here too, and correctly so: none of the old vectors are
            // usable, so the index really is cold.
            IndexState::Absent => Some(RetrievalStatus::Cold),
            _ if state.chunks == 0 => Some(RetrievalStatus::Cold),
            IndexState::Ready if state.revision.as_deref() == Some(head_sha) => {
                Some(RetrievalStatus::Ready)
            }
            // Ready-but-behind, mid-run, and failed-with-partial-results are one
            // answer to a reviewer: what came back is real code that may not be
            // this code.
            _ => Some(RetrievalStatus::Stale {
                indexed: state.revision.clone(),
            }),
        }
    }
}

/// The status for a failure that cost the review its context.
fn unavailable(err: crate::error::Error) -> RetrievedContext {
    tracing::warn!(%err, "retrieval unavailable; reviewing from the diff alone");
    RetrievedContext::degraded(RetrievalStatus::Unavailable {
        reason: err.to_string(),
    })
}

#[cfg(test)]
#[path = "retrieve_test.rs"]
mod tests;
