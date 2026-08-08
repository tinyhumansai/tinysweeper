//! Ranking, dedupe and the token budget.
//!
//! Always compiled and free of ports: this is the part of retrieval most likely
//! to be quietly wrong, so it is a pure function over values.
//!
//! Three decisions, in order.
//!
//! **Interleaving.** The two arms are merged two search hits to one graph hit
//! rather than concatenated. Concatenation in either order is a silent
//! preference: search-first means a diff with plenty of lexical neighbours
//! never spends a token on the caller it breaks, and graph-first means a
//! widely-imported file's neighbourhood crowds out everything relevant. The
//! ratio favours similarity because most of the time it is right, while
//! guaranteeing the graph a third of the budget because when it is right it is
//! the only arm that could have been.
//!
//! **Dedupe by overlap, not by identity.** The same function comes back from
//! both arms under two different line ranges — chunk boundaries move when a
//! file is re-chunked — so `(path, start, end)` equality would let it through
//! twice.
//!
//! **A stated budget.** Retrieval with no ceiling is retrieval whose share of
//! the prompt is decided by whatever the diff happened to weigh. What was
//! dropped is counted and reported, because a filter nobody can see the effect
//! of is one nobody can tell is misconfigured.

use crate::index::types::{Chunk, ScoredChunk};
use crate::retrieve::types::{Provenance, RetrievedChunk};

/// How many search hits are taken between graph hits.
///
/// Three would starve the graph on a diff with a rich lexical neighbourhood;
/// one would give an arm with no score of its own equal billing with a ranked
/// one. Two is the compromise, and it is a ratio rather than a reserved slice
/// so that an empty arm costs nothing.
const SEARCH_PER_GRAPH: usize = 2;

/// What assembly produced.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Assembled {
    /// The chunks that fit, best first.
    pub chunks: Vec<RetrievedChunk>,
    /// Candidates removed by dedupe, the chunk cap or the token budget.
    pub dropped: usize,
    /// Estimated tokens the retained chunks cost.
    pub tokens: usize,
}

/// Merge, deduplicate and truncate the two arms to a token budget.
///
/// `changed` are the paths of the pull request's own files. They are dropped
/// outright: the lane is already reading their diff, and re-stating it as
/// "related code" spends the budget on the one thing the prompt definitely
/// already contains.
pub fn assemble(
    hits: Vec<ScoredChunk>,
    reached: Vec<Chunk>,
    changed: &[String],
    max_chunks: usize,
    token_budget: usize,
) -> Assembled {
    let search: Vec<RetrievedChunk> = hits
        .into_iter()
        .filter(|hit| !changed.contains(&hit.chunk.path))
        .map(|hit| RetrievedChunk {
            chunk: hit.chunk,
            score: hit.score,
            provenance: Provenance::Search,
        })
        .collect();
    let graph: Vec<RetrievedChunk> = reached
        .into_iter()
        .filter(|chunk| !changed.contains(&chunk.path))
        .map(|chunk| RetrievedChunk {
            chunk,
            // The graph does not rank: reaching a node is not a degree of
            // resemblance, and inventing a score for it would make the two arms
            // look comparable when they are not.
            score: 0.0,
            provenance: Provenance::Graph,
        })
        .collect();

    let candidates = interleave(search, graph);
    let total = candidates.len();

    let mut kept: Vec<RetrievedChunk> = Vec::new();
    let mut tokens = 0usize;
    for candidate in candidates {
        if kept.len() == max_chunks {
            break;
        }
        if kept.iter().any(|existing| existing.overlaps(&candidate)) {
            continue;
        }
        let cost = candidate.tokens();
        if tokens + cost > token_budget {
            // Skipped rather than stopped: a single oversized chunk must not
            // shut out the smaller ones ranked behind it.
            continue;
        }
        tokens += cost;
        kept.push(candidate);
    }

    Assembled {
        dropped: total - kept.len(),
        tokens,
        chunks: kept,
    }
}

/// Merge the two arms, [`SEARCH_PER_GRAPH`] search hits to one graph hit.
///
/// Whichever arm runs out first, the other's remainder is appended: the ratio
/// is a guarantee for the smaller arm, not a cap on the larger one.
fn interleave(search: Vec<RetrievedChunk>, graph: Vec<RetrievedChunk>) -> Vec<RetrievedChunk> {
    let mut merged = Vec::with_capacity(search.len() + graph.len());
    let mut search = search.into_iter();
    let mut graph = graph.into_iter();
    loop {
        let mut moved = false;
        for _ in 0..SEARCH_PER_GRAPH {
            if let Some(hit) = search.next() {
                merged.push(hit);
                moved = true;
            }
        }
        if let Some(hit) = graph.next() {
            merged.push(hit);
            moved = true;
        }
        if !moved {
            break;
        }
    }
    merged
}

#[cfg(test)]
#[path = "assemble_test.rs"]
mod tests;
