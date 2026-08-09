//! Blast radius: which existing code a change puts at risk.
//!
//! Always compiled. Pure functions over a [`Neighbourhood`] that has already
//! been fetched, so nothing here costs a round trip and all of it is tested
//! offline.
//!
//! [`crate::retrieve::expand`] answers *what code should the reviewer see*, and
//! answers it by walking the graph in both directions — a callee is context
//! worth reading just as much as a caller. This module answers a narrower and
//! sharper question over the same walk: **what breaks if this change is
//! wrong**. That is inbound only. Whatever the diff calls does not change
//! because the diff changed; whatever calls *it* might.
//!
//! Two outputs, and the second is the one that is easy to get dishonestly
//! wrong. [`Impact::reached`] lists what depends on the change.
//! [`Impact::untested`] lists changed symbols that nothing exercises — and it
//! is emitted **only** for symbols the graph actually holds, because "no test
//! covers this" and "this symbol is new, or in a language we do not parse" are
//! different sentences and only the first is worth a reviewer's attention.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use crate::index::types::{EdgeKind, Neighbourhood, NodeKind};

/// The default ceiling on listed dependents.
///
/// Small on purpose, and for the same reason the traversal cap is: this is
/// prompt material sitting above a diff, and a widely-imported file has more
/// dependents than a reviewer can act on. Twenty names that matter beat two
/// hundred that scroll.
pub const DEFAULT_MAX_REACHED: usize = 20;

/// How a dependent relates to the change.
///
/// Ordered by how much a reviewer needs it, and the ordering is load-bearing:
/// it is what survives the cap. A test that exercises changed code is the most
/// useful thing the graph can name — it says where the change is verified, or
/// by its absence that it is not.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Relation {
    /// Exercises changed code.
    Test,
    /// Inherits from, implements, or embeds a changed declaration.
    Implementor,
    /// Calls or mentions a changed symbol.
    Caller,
    /// Imports a changed file without naming a symbol in it.
    Importer,
}

impl Relation {
    /// The phrase written into the prompt.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Test => "exercised by",
            Self::Implementor => "implemented by",
            Self::Caller => "called by",
            Self::Importer => "imported by",
        }
    }

    /// The relation an inbound edge of this kind establishes.
    ///
    /// `Defines` yields nothing: a file defining a changed symbol *is* the
    /// changed file, and listing it as its own dependent is noise.
    fn of(kind: EdgeKind) -> Option<Self> {
        match kind {
            EdgeKind::Tests => Some(Self::Test),
            EdgeKind::Extends => Some(Self::Implementor),
            EdgeKind::Calls | EdgeKind::References => Some(Self::Caller),
            EdgeKind::Imports => Some(Self::Importer),
            EdgeKind::Defines => None,
        }
    }
}

/// One node that depends on the change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Impacted {
    /// Node id: a path, or `path#symbol`.
    pub id: String,
    /// How it depends on the change.
    pub relation: Relation,
}

/// What a change reaches, and what nothing checks.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Impact {
    /// Dependents, strongest relation first, capped.
    pub reached: Vec<Impacted>,
    /// Changed symbols the graph holds and no test reaches.
    ///
    /// Empty is *not* the same as "everything is covered": a repository whose
    /// language we do not parse, or whose index is cold, contributes no seeds
    /// and therefore no gaps. The distinction is why this is reported next to
    /// [`Impact::reached`] rather than as a standalone verdict.
    pub untested: Vec<String>,
    /// Dependents the cap removed.
    pub truncated: usize,
}

impl Impact {
    /// Whether the graph found anything worth saying.
    pub fn is_empty(&self) -> bool {
        self.reached.is_empty() && self.untested.is_empty()
    }

    /// Derive the impact of `seeds` from a neighbourhood already walked.
    ///
    /// Inbound edges only — see the module docs. Seeds are excluded from their
    /// own blast radius, which also handles the mutual case: two changed files
    /// that call each other are both already in front of the reviewer.
    pub fn of(neighbourhood: &Neighbourhood, seeds: &[String], max_reached: usize) -> Self {
        let seeded: BTreeSet<&str> = seeds.iter().map(String::as_str).collect();

        // Strongest relation per node. A test that calls changed code produces
        // both a `tests` and a `calls` edge, and listing it twice would spend
        // the cap saying one thing.
        let mut strongest: BTreeMap<&str, Relation> = BTreeMap::new();
        // Changed symbols something inbound reaches from a test.
        let mut covered: BTreeSet<&str> = BTreeSet::new();

        for edge in &neighbourhood.edges {
            if !seeded.contains(edge.to.as_str()) || seeded.contains(edge.from.as_str()) {
                continue;
            }
            let Some(relation) = Relation::of(edge.kind) else {
                continue;
            };
            if relation == Relation::Test {
                covered.insert(edge.to.as_str());
            }
            strongest
                .entry(edge.from.as_str())
                .and_modify(|current| *current = (*current).min(relation))
                .or_insert(relation);
        }

        let total = strongest.len();
        let mut reached = Self::select(strongest, max_reached);
        // Relation first, then id, so the block reads grouped. Ties break the
        // same way every run — a golden test on the rendered prompt would
        // otherwise depend on map iteration order.
        reached.sort_by(|a, b| (a.relation, &a.id).cmp(&(b.relation, &b.id)));
        let truncated = total.saturating_sub(reached.len());

        // Only symbols the graph *holds*. A seed guessed from a hunk heading
        // that names nothing, or a file added by this pull request, is absent
        // from the walk — and reporting it as untested would tell a reviewer a
        // symbol has no coverage when what happened is that we never saw it.
        let known: BTreeSet<&str> = neighbourhood
            .nodes
            .iter()
            .filter(|node| node.kind == NodeKind::Symbol)
            .map(|node| node.id.as_str())
            .collect();
        let untested: Vec<String> = seeds
            .iter()
            .filter(|seed| known.contains(seed.as_str()) && !covered.contains(seed.as_str()))
            .cloned()
            .collect();

        Self {
            reached,
            untested,
            truncated,
        }
    }

    /// Fill the cap round-robin across relations rather than in priority order.
    ///
    /// Strict priority order looks right and fails on the commonest shape there
    /// is: a well-tested function with twenty-six test callers spends the whole
    /// block naming tests and never mentions the production code that would
    /// break. A reviewer needs some of each far more than all of the first, so
    /// each relation takes a turn, and only relations that have run out give up
    /// their share. Within a relation, ids go in sorted order.
    fn select(strongest: BTreeMap<&str, Relation>, max_reached: usize) -> Vec<Impacted> {
        let mut queues: BTreeMap<Relation, Vec<&str>> = BTreeMap::new();
        for (id, relation) in strongest {
            queues.entry(relation).or_default().push(id);
        }
        let mut out = Vec::with_capacity(max_reached);
        let mut taken = 0;
        while out.len() < max_reached {
            let before = out.len();
            for (relation, ids) in queues.iter_mut() {
                if out.len() == max_reached {
                    break;
                }
                if let Some(id) = ids.get(taken) {
                    out.push(Impacted {
                        id: (*id).to_string(),
                        relation: *relation,
                    });
                }
            }
            // A whole round in which no queue still had an entry means every
            // relation is exhausted; without this the loop would spin.
            if out.len() == before {
                break;
            }
            taken += 1;
        }
        out
    }

    /// The block a lane is shown, or an empty string when there is nothing.
    ///
    /// Written as comment lines rather than prose because it sits in the same
    /// suffix as the retrieved chunks, which are code. A reviewer model reading
    /// down the prompt should not have to work out where the repository's own
    /// content stops and ours starts.
    pub fn render(&self) -> String {
        if self.is_empty() {
            return String::new();
        }
        let mut out = String::with_capacity(self.reached.len() * 64 + 256);
        if !self.reached.is_empty() {
            out.push_str("// blast radius — existing code that depends on this change:\n");
            for entry in &self.reached {
                let _ = writeln!(out, "//   {} {}", entry.relation.as_str(), entry.id);
            }
            if self.truncated > 0 {
                let _ = writeln!(out, "//   … and {} more not shown", self.truncated);
            }
        }
        if !self.untested.is_empty() {
            out.push_str(
                "// no test in the index exercises these changed symbols (they exist in the \
                 graph; nothing calls them from a test):\n",
            );
            for id in &self.untested {
                let _ = writeln!(out, "//   {id}");
            }
        }
        out.push('\n');
        out
    }
}

#[cfg(test)]
#[path = "impact_test.rs"]
mod tests;
