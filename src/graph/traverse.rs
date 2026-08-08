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
//! small, and a cap alone would truncate arbitrarily deep. Truncation is by
//! distance from the seeds, so what survives is the closest blast radius
//! rather than whatever the store happened to return first.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use crate::error::Result;
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
    if query.seeds.is_empty() || query.max_nodes == 0 {
        return Ok(Neighbourhood::default());
    }
    let raw = store
        .neighbours(repo_id, &query.seeds, query.hops, &query.kinds)
        .await?;
    Ok(cap(raw, &query.seeds, query.max_nodes))
}

/// Keep the `max_nodes` nodes closest to the seeds, then drop dangling edges.
///
/// Exposed separately from [`neighbours`] so the ranking is testable without a
/// store, and so a caller that already holds a neighbourhood can re-cap it.
pub fn cap(neighbourhood: Neighbourhood, seeds: &[String], max_nodes: usize) -> Neighbourhood {
    if neighbourhood.nodes.len() <= max_nodes {
        return neighbourhood;
    }

    let mut adjacency: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for edge in &neighbourhood.edges {
        adjacency
            .entry(edge.from.as_str())
            .or_default()
            .insert(edge.to.as_str());
        adjacency
            .entry(edge.to.as_str())
            .or_default()
            .insert(edge.from.as_str());
    }

    // Breadth-first from the seeds, so the surviving set is a connected shell
    // around the diff rather than an arbitrary prefix. Seeds are enqueued in
    // the caller's order and neighbours in sorted order, which makes the
    // truncation deterministic — a golden test on the result would otherwise
    // depend on hash iteration order.
    let mut distance: BTreeMap<&str, usize> = BTreeMap::new();
    let mut order: Vec<&str> = Vec::new();
    let mut queue: VecDeque<&str> = VecDeque::new();
    for seed in seeds {
        if distance.insert(seed.as_str(), 0).is_none() {
            order.push(seed.as_str());
            queue.push_back(seed.as_str());
        }
    }
    while let Some(current) = queue.pop_front() {
        let depth = distance[current];
        let Some(next) = adjacency.get(current) else {
            continue;
        };
        for neighbour in next {
            if distance.insert(neighbour, depth + 1).is_none() {
                order.push(neighbour);
                queue.push_back(neighbour);
            }
        }
    }

    let mut keep: BTreeSet<String> = BTreeSet::new();
    for id in order {
        if keep.len() == max_nodes {
            break;
        }
        keep.insert(id.to_string());
    }
    // Nodes the store returned that no edge reaches — an isolated seed, say —
    // fill any remaining room.
    for node in &neighbourhood.nodes {
        if keep.len() == max_nodes {
            break;
        }
        keep.insert(node.id.clone());
    }

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
