//! Graph expansion: the chunks a change *reaches*, not the ones it resembles.
//!
//! Always compiled; it goes through the [`GraphStore`] and [`ChunkIndex`] ports
//! and is exercised end to end against the offline mocks.
//!
//! This is the step similarity search cannot do. A function's caller two files
//! away shares no vocabulary with the diff — that is the normal case, not an
//! edge case — so it never comes back from a hybrid query however the weights
//! are tuned. Extracting imports at index time and never traversing them at
//! review time gets a repository graph that answers no question anybody asked.
//!
//! Three bounds, and none of them is redundant: hops bound the shape of the
//! walk, the node cap bounds its size, and the chunk cap bounds what is
//! actually fetched. Two hops out of a widely imported module is most of the
//! repository, and a prompt containing the repository is worse than one
//! containing nothing.

use std::collections::BTreeSet;

use crate::config::types::Retrieval;
use crate::error::Result;
use crate::evidence::diff::FileDiff;
use crate::graph::impact::Impact;
use crate::graph::traverse::{self, NeighbourQuery};
use crate::index::types::{Chunk, EdgeKind, EmbedSignature};
use crate::ports::graph::GraphStore;
use crate::ports::index::ChunkIndex;

/// The node ids a diff seeds the walk with.
///
/// Two kinds, because they reach different things in one hop. The **file** node
/// reaches whatever imports the file and whatever it imports. The
/// **`path#symbol`** node reaches that symbol's callers directly, which is the
/// blast radius a reviewer actually wants and is two hops away from a file
/// seed.
///
/// Symbol names come from the hunk headings git already wrote — no parse, no
/// checkout, and available on the forge-only review path where the file
/// contents are not. A guess that names nothing is harmless: the store skips a
/// seed it does not have, because a pull request that adds a file names paths
/// the graph has never seen and that is normal.
pub fn seeds(diffs: &[FileDiff]) -> Vec<String> {
    let mut seeds = Vec::new();
    let mut seen = BTreeSet::new();
    for diff in diffs {
        if seen.insert(diff.path.clone()) {
            seeds.push(diff.path.clone());
        }
        for hunk in &diff.hunks {
            let Some(symbol) = symbol_of(&hunk.heading) else {
                continue;
            };
            let id = format!("{}#{symbol}", diff.path);
            if seen.insert(id.clone()) {
                seeds.push(id);
            }
        }
    }
    seeds
}

/// The symbol a hunk heading names, if it names one.
///
/// Git's heading is the enclosing definition line — `pub fn settle(order: &O)`,
/// `class Ledger:`, `func (s *Server) HandleRequest(w Writer)` — so the name
/// wanted is the first identifier **immediately** followed by `(`, and the last
/// identifier overall when the line has no call syntax at all.
///
/// "Immediately" is what makes the Go case work: `func (s *Server) Handle(` has
/// a parenthesis right after `func `, and the naive "everything before the
/// first `(`" reading picks the keyword `func` for every method in the
/// language. Written as a heuristic on purpose — running a grammar over a
/// heading fragment would need the file, and the file is exactly what the
/// forge-only review path does not have.
fn symbol_of(heading: &str) -> Option<String> {
    let heading = heading.trim();
    if heading.is_empty() {
        return None;
    }

    let is_name = |c: char| c.is_alphanumeric() || c == '_';
    let mut current = String::new();
    let mut last = None;
    for character in heading.chars() {
        if is_name(character) {
            current.push(character);
            continue;
        }
        if character == '(' && !current.is_empty() {
            return named(&current);
        }
        if !current.is_empty() {
            last = Some(std::mem::take(&mut current));
        }
    }
    let candidate = if current.is_empty() { last? } else { current };
    named(&candidate)
}

/// Whether a candidate name is worth seeding the graph with.
fn named(candidate: &str) -> Option<String> {
    // A one-character name is more likely to be a stray `>` fragment or a
    // generic parameter than a definition worth seeding.
    (candidate.len() > 1 && !candidate.chars().all(|c| c.is_ascii_digit()))
        .then(|| candidate.to_string())
}

/// What the walk found.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Expansion {
    /// Chunks of the files the walk reached, excluding the changed files
    /// themselves.
    pub chunks: Vec<Chunk>,
    /// How many nodes the walk reached, before chunks were fetched.
    ///
    /// Reported separately so "the graph found nothing" and "the graph found
    /// plenty but none of it is indexed" are distinguishable, which are two
    /// very different things to have to fix.
    pub nodes: usize,
    /// What depends on the change, and what no test reaches.
    ///
    /// Derived from the same walk rather than a second query: the edges are
    /// already in hand and the only thing left to decide is direction.
    pub impact: Impact,
}

/// Walk out from the diff and fetch the chunks of what it reaches.
///
/// The three bounds arrive as the `[retrieval]` config block rather than as
/// loose arguments, so a caller cannot pass the node cap where the chunk cap
/// goes — they are all `usize` and the compiler would not have noticed.
///
/// Files the pull request itself changed are excluded: the lane is already
/// looking at their diff, and spending the context budget on code that is
/// directly above it in the prompt buys nothing.
pub async fn expand(
    graph: &dyn GraphStore,
    index: &dyn ChunkIndex,
    signature: &EmbedSignature,
    repo_id: &str,
    diffs: &[FileDiff],
    bounds: &Retrieval,
) -> Result<Expansion> {
    let (hops, max_nodes, max_chunks) =
        (bounds.graph_hops, bounds.max_graph_nodes, bounds.max_chunks);
    if hops == 0 || max_nodes == 0 || max_chunks == 0 {
        return Ok(Expansion::default());
    }

    let seeds = seeds(diffs);
    if seeds.is_empty() {
        return Ok(Expansion::default());
    }

    let query = NeighbourQuery::new(seeds)
        .hops(hops)
        .kinds(EdgeKind::ALL)
        .max_nodes(max_nodes);
    let walked = traverse::walk(graph, repo_id, &query).await?;

    // Before the cap. The blast radius is a handful of names and the cap exists
    // to bound how much *code* reaches the prompt; letting one bound the other
    // would drop dependents to make room for chunks.
    let impact = Impact::of(&walked, &query.seeds, bounds.max_impact);
    let neighbourhood = traverse::cap(walked, &query.seeds, max_nodes);

    let changed: BTreeSet<&str> = diffs.iter().map(|diff| diff.path.as_str()).collect();
    let mut paths: Vec<String> = neighbourhood
        .nodes
        .iter()
        .map(|node| node.path.clone())
        .filter(|path| !changed.contains(path.as_str()))
        .collect();
    paths.sort_unstable();
    paths.dedup();

    let chunks = index
        .chunks_in_paths(signature, repo_id, &paths, max_chunks)
        .await?;

    Ok(Expansion {
        chunks,
        nodes: neighbourhood.nodes.len(),
        impact,
    })
}

#[cfg(test)]
#[path = "expand_test.rs"]
mod tests;
