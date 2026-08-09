//! Bounded traversal over the stored graph.
//!
//! Always compiled.
//!
//! This is the half of the workstream that makes the graph part of a *review*
//! rather than a picture. Retrieval seeds it with the symbols a diff touches
//! and asks what else is implicated; everything here exists to make sure the
//! answer fits in a prompt.
//!
//! Two bounds, and they are not redundant. Hops bound the *shape* of the walk;
//! the node cap bounds its *size*. Two hops out of a widely imported module is
//! most of the repository, so a hop limit alone does not keep the result
//! small, and a cap alone would truncate arbitrarily deep. What survives the
//! cap is decided by [`rank`](super::rank), not by distance alone — see that
//! module for why distance ran out of road.

use std::collections::BTreeSet;

use crate::error::Result;
use crate::graph::rank::rank;
use crate::index::types::{EdgeKind, GraphEdge, GraphNode, Neighbourhood};
use crate::ports::graph::GraphStore;

/// The default node ceiling.
///
/// Chosen to be small: a neighbourhood is context for a prompt that already
/// contains a diff, and past a couple of hundred nodes the marginal node
/// displaces something the reviewer needed.
pub const DEFAULT_MAX_NODES: usize = 200;

/// The default hop count.
///
/// One hop is the direct callers and callees. Two is where a graph starts
/// returning the whole repository, so it is a deliberate opt-in.
pub const DEFAULT_HOPS: u8 = 1;

/// A bounded neighbourhood request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NeighbourQuery {
    /// Node ids to start from: file paths, or `path#symbol` for symbols.
    /// Seeds that do not exist are skipped, not an error.
    pub seeds: Vec<String>,
    /// How many edges to walk.
    pub hops: u8,
    /// Which edge kinds may be traversed.
    pub kinds: Vec<EdgeKind>,
    /// Hard ceiling on returned nodes, applied after the walk.
    pub max_nodes: usize,
}

impl NeighbourQuery {
    /// A one-hop, all-kinds query from `seeds`.
    pub fn new(seeds: impl IntoIterator<Item = String>) -> Self {
        Self {
            seeds: seeds.into_iter().collect(),
            hops: DEFAULT_HOPS,
            kinds: EdgeKind::ALL.to_vec(),
            max_nodes: DEFAULT_MAX_NODES,
        }
    }

    /// Walk `hops` edges instead of one.
    pub fn hops(mut self, hops: u8) -> Self {
        self.hops = hops;
        self
    }

    /// Restrict the walk to these edge kinds.
    pub fn kinds(mut self, kinds: impl IntoIterator<Item = EdgeKind>) -> Self {
        self.kinds = kinds.into_iter().collect();
        self
    }

    /// Change the node ceiling.
    pub fn max_nodes(mut self, max_nodes: usize) -> Self {
        self.max_nodes = max_nodes;
        self
    }
}

/// Walk the graph and return a capped neighbourhood.
pub async fn neighbours(
    store: &dyn GraphStore,
    repo_id: &str,
    query: &NeighbourQuery,
) -> Result<Neighbourhood> {
    if query.max_nodes == 0 {
        return Ok(Neighbourhood::default());
    }
    let raw = walk(store, repo_id, query).await?;
    Ok(cap(raw, &query.seeds, query.max_nodes))
}

/// Walk the graph without applying the node cap.
///
/// Exposed for the one caller that needs the whole walk: the blast radius is a
/// list of *names*, not of code, and it costs a line each. Deriving it from the
/// capped neighbourhood would silently drop dependents to make room for chunks
/// nobody asked to see — and the count of what was dropped would be wrong too,
/// which is worse than the omission.
pub async fn walk(
    store: &dyn GraphStore,
    repo_id: &str,
    query: &NeighbourQuery,
) -> Result<Neighbourhood> {
    if query.seeds.is_empty() {
        return Ok(Neighbourhood::default());
    }
    store
        .neighbours(repo_id, &query.seeds, query.hops, &query.kinds)
        .await
}

/// Keep the `max_nodes` highest-ranked nodes, then drop dangling edges.
///
/// Exposed separately from [`neighbours`] so the ranking is testable without a
/// store, and so a caller that already holds a neighbourhood can re-cap it.
///
/// Ranking is [`rank`], which is personalised on the seeds and therefore still
/// decays with distance — the closest blast radius survives, as it did under
/// the previous breadth-first truncation. What changed is the tie: at equal
/// distance the node the diff reaches by more paths now wins, where before the
/// winner was whichever id sorted first.
///
/// Node order in the result is left as the store gave it. The cap decides
/// *membership*; a caller that also wants the order — [`expand`] does, to spend
/// a chunk budget — should call [`rank`] itself rather than infer it from here.
///
/// [`expand`]: crate::retrieve::expand::expand
pub fn cap(neighbourhood: Neighbourhood, seeds: &[String], max_nodes: usize) -> Neighbourhood {
    if neighbourhood.nodes.len() <= max_nodes {
        return neighbourhood;
    }

    let keep: BTreeSet<String> = rank(&neighbourhood, seeds)
        .into_iter()
        .take(max_nodes)
        .map(|(id, _)| id)
        .collect();

    let nodes: Vec<GraphNode> = neighbourhood
        .nodes
        .into_iter()
        .filter(|n| keep.contains(&n.id))
        .collect();
    let edges: Vec<GraphEdge> = neighbourhood
        .edges
        .into_iter()
        .filter(|e| keep.contains(&e.from) && keep.contains(&e.to))
        .collect();
    Neighbourhood { nodes, edges }
}

#[cfg(test)]
#[path = "traverse_test.rs"]
mod tests;
