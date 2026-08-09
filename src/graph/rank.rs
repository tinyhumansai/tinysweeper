//! Personalised PageRank over a neighbourhood: which of these nodes matter.
//!
//! Always compiled. A pure function over values — no ports, no store, no
//! async — because this decides what a reviewer gets to read and is therefore
//! the part of traversal most worth testing directly.
//!
//! ## Why distance is not enough
//!
//! [`traverse::cap`](super::traverse::cap) used to keep the nodes *closest* to
//! the seeds and break ties by id. One hop out that is defensible. Two hops out
//! of a widely imported module, every candidate sits at the same distance and
//! the tie-break — alphabetical order — silently decides what the reviewer
//! sees. `a_client.ts` survives and `zod_schema.ts` does not, for no reason
//! connected to the change.
//!
//! Personalised PageRank replaces the tie-break with a reason. All the restart
//! mass goes to the seeds, so score still decays with distance, but at equal
//! distance a node reached by *many* paths from the diff outranks one reached
//! by a single edge. That is the question a reviewer is actually asking: not
//! "what is nearby" but "what is most implicated".
//!
//! The idea is aider's `repomap.py`, which ranks a whole repository this way to
//! build its map. Two things it does are deliberately **not** copied:
//!
//! * **Reference multiplicity.** aider scales an edge by `sqrt(num_refs)`.
//!   [`GraphEdge`] is deduplicated by id in
//!   [`build`](super::build), so the count is not in the data and inventing it
//!   here would be a lie. Recovering it means a schema change, and it is not
//!   obviously worth one.
//! * **Rarity weighting.** aider boosts identifiers defined in few files and
//!   damps ones defined everywhere (`new`, `get`, `len`). tinysweeper already
//!   does something stricter and earlier: `build::target_for` refuses to emit
//!   an edge at all for a usage it cannot attribute to one definition, so the
//!   common-identifier noise aider down-weights never becomes an edge here.
//!
//! What *is* copied is the algorithm and the damping factor.

use std::collections::{BTreeMap, BTreeSet};

use crate::index::types::{EdgeKind, Neighbourhood};

/// The share of a node's score that flows along its edges.
///
/// 0.85 is the value from the original PageRank paper and the one aider uses.
/// The complement, 0.15, restarts at the seeds — which for a personalised walk
/// is what bounds how far the diff's importance can travel. Raising it widens
/// the blast radius; there is no reason to depart from the standard value
/// without evidence from a real corpus.
pub const DAMPING: f64 = 0.85;

/// Power-iteration ceiling.
///
/// A neighbourhood is capped at a few hundred nodes, where the iteration
/// converges well inside this. The bound exists so a pathological graph cannot
/// stall a review, not because it is expected to bind.
const MAX_ITERATIONS: usize = 64;

/// Convergence threshold on the L1 change between iterations.
const TOLERANCE: f64 = 1e-9;

/// How much of a node's importance each kind of edge carries.
///
/// These are a ranking judgement, not a measurement, and the ordering is the
/// load-bearing part rather than the exact values.
///
/// * [`EdgeKind::Calls`] is the strongest evidence that changing one node
///   breaks the other, so it anchors the scale at 1.
/// * [`EdgeKind::References`] is a real dependency but a weaker one: a mention
///   survives many changes a call site would not.
/// * [`EdgeKind::Imports`] is file-level and therefore coarse — it says the
///   file depends on the module, not on the symbol that actually changed.
/// * [`EdgeKind::Defines`] is *containment*, not dependency. It has to be
///   small: it is the edge from a file to every symbol in it, so at any larger
///   weight a single seeded file sprays its mass over every unrelated symbol
///   that happens to share the file, and the ranking collapses back into "big
///   files win".
fn weight(kind: EdgeKind) -> f64 {
    match kind {
        EdgeKind::Calls => 1.0,
        EdgeKind::References => 0.6,
        EdgeKind::Imports => 0.5,
        EdgeKind::Defines => 0.3,
    }
}

/// Rank every node in `neighbourhood`, best first.
///
/// Ties break on node id so the result is reproducible: a golden test over a
/// prompt built from this must not depend on float ordering or on the order the
/// store happened to return rows in.
///
/// Edges are walked in **both** directions. A directed walk would have to pick
/// one of the two questions a review asks — "what does this change depend on"
/// (forwards) or "what breaks when it changes" (backwards) — and the second is
/// the one that only the graph can answer, so neither may be dropped. Direction
/// is instead expressed through [`weight`].
///
/// Seeds are pinned above everything else, whatever they score. On an
/// undirected walk a degree-1 node loses to its degree-2 neighbour — mass piles
/// up where the edges are — so a seed at the end of a call chain scores below
/// the file it calls. That is a fair statement about connectivity and a useless
/// one about review: the seeds *are* the change, and a cap that drops one to
/// keep a neighbour has truncated the thing it was asked about. Pinning states
/// that as policy instead of tuning [`DAMPING`] until it happens to fall out.
///
/// Returns an empty vector for an empty neighbourhood. Seeds that are not in
/// `neighbourhood` are ignored; if *no* seed is present the walk restarts
/// uniformly, which degrades to plain centrality rather than to nothing.
pub fn rank(neighbourhood: &Neighbourhood, seeds: &[String]) -> Vec<(String, f64)> {
    let ids: Vec<&str> = neighbourhood.nodes.iter().map(|n| n.id.as_str()).collect();
    if ids.is_empty() {
        return Vec::new();
    }
    let index: BTreeMap<&str, usize> = ids.iter().enumerate().map(|(i, id)| (*id, i)).collect();
    let n = ids.len();

    // Weighted undirected adjacency, and each node's total outgoing weight so
    // the transition matrix can be normalised.
    let mut adjacency: Vec<Vec<(usize, f64)>> = vec![Vec::new(); n];
    let mut outgoing = vec![0.0_f64; n];
    for edge in &neighbourhood.edges {
        let (Some(&from), Some(&to)) = (index.get(edge.from.as_str()), index.get(edge.to.as_str()))
        else {
            // An edge whose endpoint the cap already dropped. Skipping it keeps
            // the walk consistent with the node set the caller will render.
            continue;
        };
        if from == to {
            continue;
        }
        let w = weight(edge.kind);
        adjacency[from].push((to, w));
        adjacency[to].push((from, w));
        outgoing[from] += w;
        outgoing[to] += w;
    }

    let restart = personalisation(&ids, &index, seeds);
    let mut score = restart.clone();
    let mut next = vec![0.0_f64; n];

    for _ in 0..MAX_ITERATIONS {
        next.iter_mut().for_each(|s| *s = 0.0);

        // Mass sitting on nodes with no edges has nowhere to flow. Left there
        // it would leak out of the distribution every iteration, so it is
        // redistributed over the restart vector — the standard dangling-node
        // handling, and the reason the scores still sum to one.
        let mut dangling = 0.0;
        for i in 0..n {
            if outgoing[i] == 0.0 {
                dangling += score[i];
                continue;
            }
            let share = score[i] / outgoing[i];
            for &(to, w) in &adjacency[i] {
                next[to] += share * w;
            }
        }

        let mut delta = 0.0;
        for i in 0..n {
            let updated =
                (1.0 - DAMPING) * restart[i] + DAMPING * (next[i] + dangling * restart[i]);
            delta += (updated - score[i]).abs();
            score[i] = updated;
        }
        if delta < TOLERANCE {
            break;
        }
    }

    let pinned: BTreeSet<&str> = seeds
        .iter()
        .map(String::as_str)
        .filter(|seed| index.contains_key(seed))
        .collect();
    let mut ranked: Vec<(String, f64)> = ids
        .iter()
        .enumerate()
        .map(|(i, id)| ((*id).to_string(), score[i]))
        .collect();
    // Seeds first, then score, then id. `total_cmp` rather than
    // `partial_cmp().unwrap()`: the scores are finite by construction, but a
    // NaN here would panic in production for a ranking that could simply have
    // been arbitrary.
    ranked.sort_by(|a, b| {
        pinned
            .contains(b.0.as_str())
            .cmp(&pinned.contains(a.0.as_str()))
            .then_with(|| b.1.total_cmp(&a.1))
            .then_with(|| a.0.cmp(&b.0))
    });
    ranked
}

/// The restart distribution: all of it on the seeds, or uniform if none exist.
fn personalisation(ids: &[&str], index: &BTreeMap<&str, usize>, seeds: &[String]) -> Vec<f64> {
    let mut present: Vec<usize> = seeds
        .iter()
        .filter_map(|seed| index.get(seed.as_str()).copied())
        .collect();
    present.sort_unstable();
    present.dedup();

    let mut restart = vec![0.0_f64; ids.len()];
    if present.is_empty() {
        let share = 1.0 / ids.len() as f64;
        restart.iter_mut().for_each(|r| *r = share);
        return restart;
    }
    let share = 1.0 / present.len() as f64;
    for i in present {
        restart[i] = share;
    }
    restart
}

#[cfg(test)]
#[path = "rank_test.rs"]
mod tests;
