//! The chunk-index port.
//!
//! Always compiled; the MongoDB adapter is behind `serve`.
//!
//! The shape here is deliberately wider than the two-method `add`/`query` seam
//! a vector store usually exposes. That shape only supports building an index
//! once. tinysweeper re-indexes on every push, so the operations that actually
//! decide whether the thing is usable are the destructive ones:
//! [`ChunkIndex::delete_repo`] when a repository is removed or its chunking
//! changes, and [`ChunkIndex::delete_paths`] before re-adding the files a push
//! touched. Without the second, an incremental re-index leaves the old chunks
//! of every edited file in place and retrieval starts quoting code that is no
//! longer there.

use async_trait::async_trait;

use crate::error::Result;
use crate::index::types::{EmbedSignature, EmbeddedChunk, HybridQuery, ScoredChunk};

/// A searchable store of embedded code chunks.
#[async_trait]
pub trait ChunkIndex: Send + Sync {
    /// Prepare the index to serve `signature`.
    ///
    /// Creates whatever the backend needs — vector and lexical indexes sized to
    /// the signature's dimensionality — and **verifies that hybrid search is
    /// actually available**. This is called at startup precisely so that a
    /// deployment missing search support fails there rather than at query time,
    /// which would mean failing on a contributor's pull request.
    async fn prepare(&self, signature: &EmbedSignature) -> Result<()>;

    /// Insert or replace a batch of chunks, returning how many were written.
    ///
    /// Idempotent on [`Chunk::id`](crate::index::types::Chunk::id): upserting
    /// the same span twice is one row.
    async fn upsert(&self, signature: &EmbedSignature, chunks: &[EmbeddedChunk]) -> Result<u64>;

    /// Remove every chunk of a repository, returning how many went.
    async fn delete_repo(&self, repo_id: &str) -> Result<u64>;

    /// Remove every chunk of the given paths within a repository.
    ///
    /// The incremental re-index primitive: delete the paths a push touched,
    /// then upsert their new chunks. An empty `paths` deletes nothing rather
    /// than everything — the destructive reading of an empty list is never the
    /// one a caller meant.
    async fn delete_paths(&self, repo_id: &str, paths: &[String]) -> Result<u64>;

    /// Run a hybrid dense + lexical query, best first.
    async fn query(&self, query: &HybridQuery) -> Result<Vec<ScoredChunk>>;
}
