//! The index-manifest port: what has been indexed, and who is indexing it.
//!
//! Always compiled; the MongoDB adapter is behind `serve`.
//!
//! The manifest answers two questions the [`ChunkIndex`](crate::ports::index)
//! deliberately does not.
//!
//! **"Have I already embedded this?"** Without a record of the chunk ids a file
//! produced last time, the only way to re-index is to embed everything and let
//! the upsert deduplicate — which costs the full embedding bill on every push
//! for a repository that changed one file. The manifest turns that into a set
//! difference.
//!
//! **"Is somebody else already doing this?"** Two workers indexing one
//! repository at once do not corrupt anything, but they do pay twice and they
//! race on the stale-chunk delete. The claim is a single atomic insert against
//! a unique index, and a refused claim is an answer — *requeue the job* — not
//! an error and never a wait, because a worker blocked on a lock is a worker
//! not reviewing anything else.

use async_trait::async_trait;

use crate::error::Result;
use crate::index::types::EmbedSignature;
use crate::indexer::types::{Claim, IndexLease, IndexedFile, RepoIndex, Settled};

/// A record of what has been indexed, and the claim that guards it.
#[async_trait]
pub trait IndexManifest: Send + Sync {
    /// Try to take the indexing claim for a repository.
    ///
    /// Returns [`Claim::Busy`] rather than blocking or erroring when another
    /// holder has it: the caller requeues.
    async fn claim(
        &self,
        repo_id: &str,
        signature: &EmbedSignature,
        holder: &str,
    ) -> Result<Claim>;

    /// Give the claim back, recording how the run ended.
    ///
    /// Must be called on the failure path too. A claim that is only released on
    /// success is a claim a crashed worker holds forever, which is why the
    /// backing store also expires it on a TTL.
    async fn release(&self, lease: &IndexLease, settled: &Settled) -> Result<()>;

    /// The current state of a repository's index.
    async fn state(&self, repo_id: &str, signature: &EmbedSignature) -> Result<RepoIndex>;

    /// What is on record for these paths.
    ///
    /// Paths with no record are simply absent from the result rather than
    /// present-and-empty: "never indexed" and "indexed to zero chunks" lead to
    /// the same work, and collapsing them keeps callers from having to care.
    async fn indexed(
        &self,
        repo_id: &str,
        signature: &EmbedSignature,
        paths: &[String],
    ) -> Result<Vec<IndexedFile>>;

    /// Replace the record for these files.
    async fn record(
        &self,
        repo_id: &str,
        signature: &EmbedSignature,
        files: &[IndexedFile],
    ) -> Result<()>;

    /// Drop the record for these paths, for files that no longer exist.
    async fn forget(
        &self,
        repo_id: &str,
        signature: &EmbedSignature,
        paths: &[String],
    ) -> Result<()>;
}
