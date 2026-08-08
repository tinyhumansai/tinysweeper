//! The code-graph port.
//!
//! Always compiled; the MongoDB adapter is behind `serve`.
//!
//! Retrieval by similarity finds code that *reads* like the query. It does not
//! find the caller three files away that a change breaks, because that caller
//! shares no vocabulary with the diff. The graph is the other half: seed it
//! with the files a pull request touched and walk outwards, so the reviewer
//! sees the blast radius rather than only the lexical neighbourhood.
//!
//! Hops are capped by the caller and by the implementation. Two hops out of a
//! widely-imported module is most of the repository, and a prompt that contains
//! most of the repository is worse than one that contains nothing.

use async_trait::async_trait;

use crate::error::Result;
use crate::index::types::{EdgeKind, GraphEdge, GraphNode, Neighbourhood};

/// A store of code-structure nodes and edges.
#[async_trait]
pub trait GraphStore: Send + Sync {
    /// Prepare the store: create the indexes traversal and deletion rely on.
    async fn prepare(&self) -> Result<()>;

    /// Insert or replace nodes, returning how many were written.
    async fn upsert_nodes(&self, nodes: &[GraphNode]) -> Result<u64>;

    /// Insert or replace edges, returning how many were written.
    async fn upsert_edges(&self, edges: &[GraphEdge]) -> Result<u64>;

    /// Remove a repository's whole graph, returning how many rows went.
    async fn delete_repo(&self, repo_id: &str) -> Result<u64>;

    /// Remove the nodes belonging to the given paths and every edge touching
    /// one of them.
    ///
    /// The incremental counterpart to [`GraphStore::upsert_nodes`]: a re-parsed
    /// file must not leave its previous symbols or incoming relationships
    /// behind. An empty `paths` deletes nothing.
    async fn delete_paths(&self, repo_id: &str, paths: &[String]) -> Result<u64>;

    /// Walk `hops` edges out from `seeds`.
    ///
    /// `kinds` filters which edges may be traversed; pass
    /// [`EdgeKind::ALL`](crate::index::types::EdgeKind::ALL) for an unfiltered
    /// walk. Edges are followed in both directions — the callers of a changed
    /// function matter at least as much as its callees.
    ///
    /// Seeds that do not exist are skipped rather than an error: a pull request
    /// that adds a file names a path the graph has never seen, and that is
    /// normal.
    async fn neighbours(
        &self,
        repo_id: &str,
        seeds: &[String],
        hops: u8,
        kinds: &[EdgeKind],
    ) -> Result<Neighbourhood>;
}
