//! The change flow: how edited behaviours connect to the surrounding system.
//!
//! Always compiled. It reads a diff, an optional graph neighbourhood and the
//! findings the review already produced, and returns a [`ChangeMap`] that
//! [`render::comment`] turns into one durable pull request comment.
//!
//! Three properties are deliberate and load-bearing:
//!
//! - **No model call.** Every number in the picture is arithmetic over the diff
//!   and the stored graph. A drawing that changes between two runs of the same
//!   commit is not a drawing of the commit, and a diagram nobody can reproduce
//!   is one nobody can check. It also means the map costs nothing, so it can be
//!   posted on every push rather than rationed.
//! - **No untrusted text.** Nothing from the pull request's title, body or
//!   comments reaches the diagram. Paths do, and they are attacker-controlled —
//!   a contributor chooses their own filenames — so every label goes through
//!   [`mermaid::label`] and every node id is generated here, never derived from
//!   a path. See the module doc on [`mermaid`] for what that stops.
//! - **It degrades honestly.** With no graph store attached there are no
//!   arrows, only the change's own shape; the comment says so rather than
//!   letting an absent code graph read as a change that reaches nothing. Same
//!   rule as [`crate::retrieve`].
//!
//! The unit of the picture is a named symbol already present in the repository
//! graph. Hunk headings identify which symbols changed; typed graph edges say
//! whether surrounding behaviour calls, uses, implements, or tests them.

pub mod mermaid;
pub mod render;
pub mod types;

use std::collections::{BTreeMap, BTreeSet};

use crate::config::types::{Overview, Severity};
use crate::evidence::diff::FileDiff;
use crate::findings::types::Finding;
use crate::index::types::{EdgeKind, Neighbourhood};

pub use crate::overview::render::{MARKER, comment};
pub use crate::overview::types::{ChangeMap, Component, FlowRelation, GraphStatus, Link, Role};

/// What the caller was able to get out of the code graph.
///
/// Three cases rather than an `Option`, because "there is no graph here",
/// "the graph is there and knows nothing about these files" and "the graph is
/// there and did not answer" need three different things done about them, and
/// only the last one is somebody's bug. Collapsing them was the first version
/// of this and it made an outage indistinguishable from a supported offline
/// deployment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GraphView<'a> {
    /// No graph store is attached to this deployment at all.
    Absent,
    /// A graph is attached, but the walk failed.
    Unavailable,
    /// The walk ran; this is what it reached.
    Walked(&'a Neighbourhood),
}

/// Build the map for one review.
///
/// `findings` are the ones that survived filtering, so the diagram marks what
/// the review will actually say rather than what a lane first proposed.
pub fn build(
    diffs: &[FileDiff],
    findings: &[Finding],
    view: GraphView<'_>,
    limits: &Overview,
) -> ChangeMap {
    let changed_ids: BTreeSet<String> = crate::retrieve::seeds(diffs)
        .into_iter()
        .filter(|seed| seed.contains('#'))
        .collect();
    let mut changed = changed_components(&changed_ids);
    mark_findings(&mut changed, findings);

    let (impacted, graph) = match view {
        GraphView::Walked(walk) => (impacted_components(walk, &changed_ids), status(walk)),
        GraphView::Absent => (BTreeMap::new(), GraphStatus::Off),
        GraphView::Unavailable => (BTreeMap::new(), GraphStatus::Unavailable),
    };

    // Stable ordering keeps the same commit byte-identical between runs.
    let mut changed: Vec<Component> = changed.into_values().collect();
    changed.sort_by(|a, b| a.name.cmp(&b.name));
    let mut impacted: Vec<Component> = impacted.into_values().collect();
    impacted.sort_by(|a, b| b.files.cmp(&a.files).then(a.name.cmp(&b.name)));

    let mut folded = changed.len().saturating_sub(limits.max_components)
        + impacted.len().saturating_sub(limits.max_impacted);
    changed.truncate(limits.max_components);
    impacted.truncate(limits.max_impacted);

    // Changed first, so indices in `links` are stable against the order the
    // renderer walks them and the diagram reads left to right from the change.
    let mut components = changed;
    components.extend(impacted);

    let links = match view {
        GraphView::Walked(walk) => links(walk, &components, limits.max_links),
        GraphView::Absent | GraphView::Unavailable => Vec::new(),
    };
    let (kept, links, unlinked) = connected_only(components, links);
    components = kept;
    folded += unlinked;

    ChangeMap {
        files: diffs.len(),
        additions: diffs.iter().map(FileDiff::additions).sum(),
        deletions: diffs.iter().map(FileDiff::deletions).sum(),
        components,
        links,
        folded,
        graph,
    }
}

/// Remove names that survived a node cap but no surviving relationship uses.
///
/// A box without an arrow is an inventory item, not part of a flow. Link caps
/// can otherwise strand nodes after their only relationship is truncated.
fn connected_only(
    components: Vec<Component>,
    mut links: Vec<Link>,
) -> (Vec<Component>, Vec<Link>, usize) {
    let mut used = vec![false; components.len()];
    for link in &links {
        used[link.from] = true;
        used[link.to] = true;
    }

    let mut remap = vec![None; components.len()];
    let mut kept = Vec::new();
    for (old, component) in components.into_iter().enumerate() {
        if used[old] {
            remap[old] = Some(kept.len());
            kept.push(component);
        }
    }
    for link in &mut links {
        link.from = remap[link.from].expect("a link marks its source as used");
        link.to = remap[link.to].expect("a link marks its target as used");
    }
    let removed = used.iter().filter(|&&is_used| !is_used).count();
    (kept, links, removed)
}

/// Turn changed symbol seeds into the green nodes in the flow.
fn changed_components(changed_ids: &BTreeSet<String>) -> BTreeMap<String, Component> {
    let mut components: BTreeMap<String, Component> = BTreeMap::new();
    for id in changed_ids {
        let path = file_of(id).to_string();
        components.insert(
            id.clone(),
            Component {
                name: id.clone(),
                role: Role::Changed,
                files: 1,
                additions: 0,
                deletions: 0,
                findings: 0,
                worst: None,
                paths: vec![path],
            },
        );
    }
    components
}

/// Attribute findings to changed behaviours in the file they name.
fn mark_findings(components: &mut BTreeMap<String, Component>, findings: &[Finding]) {
    for finding in findings {
        for component in components
            .values_mut()
            .filter(|component| component.paths.iter().any(|path| path == &finding.path))
        {
            component.findings += 1;
            component.worst = Some(match component.worst {
                Some(worst) => worst.max(finding.severity),
                None => finding.severity,
            });
        }
    }
}

/// Collect untouched symbols connected to the changed behaviours.
fn impacted_components(
    walk: &Neighbourhood,
    changed_ids: &BTreeSet<String>,
) -> BTreeMap<String, Component> {
    let connected: BTreeSet<&str> = walk
        .edges
        .iter()
        .filter(|edge| relation(edge.kind).is_some())
        .flat_map(|edge| [edge.from.as_str(), edge.to.as_str()])
        .collect();
    let mut components = BTreeMap::new();
    for node in &walk.nodes {
        if node.kind != crate::index::types::NodeKind::Symbol
            || changed_ids.contains(&node.id)
            || !connected.contains(node.id.as_str())
        {
            continue;
        }
        let degree = walk
            .edges
            .iter()
            .filter(|edge| edge.from == node.id || edge.to == node.id)
            .filter(|edge| relation(edge.kind).is_some())
            .count();
        components.insert(
            node.id.clone(),
            Component {
                name: node.id.clone(),
                role: Role::Impacted,
                files: degree,
                additions: 0,
                deletions: 0,
                findings: 0,
                worst: None,
                paths: vec![node.path.clone()],
            },
        );
    }
    components
}

/// Aggregate typed symbol relationships into human-readable flow arrows.
fn links(walk: &Neighbourhood, components: &[Component], max: usize) -> Vec<Link> {
    let index: BTreeMap<&str, usize> = components
        .iter()
        .enumerate()
        .map(|(i, c)| (c.name.as_str(), i))
        .collect();

    let mut weights: BTreeMap<(usize, usize, FlowRelation), usize> = BTreeMap::new();
    for edge in &walk.edges {
        let Some(relation) = relation(edge.kind) else {
            continue;
        };
        let (Some(&from), Some(&to)) = (index.get(edge.from.as_str()), index.get(edge.to.as_str()))
        else {
            continue;
        };
        *weights.entry((from, to, relation)).or_default() += 1;
    }

    let mut links: Vec<Link> = weights
        .into_iter()
        .map(|((from, to, relation), weight)| Link {
            from,
            to,
            relation,
            weight,
        })
        .collect();
    // Heaviest first, so capping keeps the arrows that carry the most.
    links.sort_by(|a, b| {
        b.weight
            .cmp(&a.weight)
            .then(a.from.cmp(&b.from))
            .then(a.to.cmp(&b.to))
    });
    links.truncate(max);
    links
}

/// Graph relationships that describe behaviour rather than file layout.
fn relation(kind: EdgeKind) -> Option<FlowRelation> {
    match kind {
        EdgeKind::Calls => Some(FlowRelation::Calls),
        EdgeKind::References => Some(FlowRelation::Uses),
        EdgeKind::Extends => Some(FlowRelation::Implements),
        EdgeKind::Tests => Some(FlowRelation::Tests),
        EdgeKind::Imports | EdgeKind::Defines => None,
    }
}

/// The file half of a node id: `src/a.rs#foo` is a symbol in `src/a.rs`.
fn file_of(node_id: &str) -> &str {
    node_id.split_once('#').map_or(node_id, |(path, _)| path)
}

/// What the walk contributed, for the honesty line under the diagram.
fn status(walk: &Neighbourhood) -> GraphStatus {
    if walk.nodes.is_empty() {
        GraphStatus::Cold
    } else {
        GraphStatus::Walked {
            nodes: walk.nodes.len(),
        }
    }
}

/// The worst severity anywhere on the map, for the headline.
pub fn worst(map: &ChangeMap) -> Option<Severity> {
    map.components.iter().filter_map(|c| c.worst).max()
}

#[cfg(test)]
#[path = "test.rs"]
mod tests;
