//! The change map: what this pull request touches, drawn.
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
//! The unit of the picture is a *directory*, chosen at the deepest level that
//! still fits — see [`group`]. Asking a model to name the "real" components
//! would produce better names and a worse artefact: unreproducible, and paid
//! for on every push.

pub mod group;
pub mod mermaid;
pub mod render;
pub mod types;

use std::collections::{BTreeMap, BTreeSet};

use crate::config::types::{Overview, Severity};
use crate::evidence::diff::FileDiff;
use crate::findings::types::Finding;
use crate::index::types::{EdgeKind, Neighbourhood};

pub use crate::overview::render::{MARKER, comment};
pub use crate::overview::types::{ChangeMap, Component, GraphStatus, Link, Role};

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
    let changed_paths: BTreeSet<String> = diffs.iter().map(|d| d.path.clone()).collect();

    // Depth comes from the *changed* paths alone. Letting the neighbourhood
    // vote would mean the same commit is drawn at a different grain depending
    // on how warm the index is, which makes two reviews of one commit
    // disagree about what its components are.
    let depth = group::choose_depth(&changed_paths, limits.max_components);

    let mut changed = changed_components(diffs, depth, limits.max_paths_per_component);
    mark_findings(&mut changed, findings, depth);

    let (impacted, graph) = match view {
        GraphView::Walked(walk) => (
            impacted_components(walk, &changed_paths, depth),
            status(walk),
        ),
        GraphView::Absent => (BTreeMap::new(), GraphStatus::Off),
        GraphView::Unavailable => (BTreeMap::new(), GraphStatus::Unavailable),
    };

    // Ranked before capping, and the two halves are ranked by different
    // things: a changed component matters in proportion to how much of it
    // moved, an impacted one in proportion to how much of it is exposed.
    let mut changed: Vec<Component> = changed.into_values().collect();
    changed.sort_by(|a, b| b.churn().cmp(&a.churn()).then(a.name.cmp(&b.name)));
    let mut impacted: Vec<Component> = impacted.into_values().collect();
    impacted.sort_by(|a, b| b.files.cmp(&a.files).then(a.name.cmp(&b.name)));

    let folded = changed.len().saturating_sub(limits.max_components)
        + impacted.len().saturating_sub(limits.max_impacted);
    changed.truncate(limits.max_components);
    impacted.truncate(limits.max_impacted);

    // Changed first, so indices in `links` are stable against the order the
    // renderer walks them and the diagram reads left to right from the change.
    let mut components = changed;
    components.extend(impacted);

    let links = match view {
        GraphView::Walked(walk) => links(walk, &components, depth, limits.max_links),
        GraphView::Absent | GraphView::Unavailable => Vec::new(),
    };

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

/// Fold the diff into one component per directory prefix.
fn changed_components(
    diffs: &[FileDiff],
    depth: usize,
    max_paths: usize,
) -> BTreeMap<String, Component> {
    let mut components: BTreeMap<String, Component> = BTreeMap::new();
    for diff in diffs {
        let name = group::component_of(&diff.path, depth);
        let component = components.entry(name.clone()).or_insert_with(|| Component {
            name,
            role: Role::Changed,
            files: 0,
            additions: 0,
            deletions: 0,
            findings: 0,
            worst: None,
            paths: Vec::new(),
        });
        component.files += 1;
        component.additions += diff.additions();
        component.deletions += diff.deletions();
        // `files` keeps counting past the cap, so the table can say "3 of 40"
        // rather than claiming the component holds three files.
        if component.paths.len() < max_paths {
            component.paths.push(diff.path.clone());
        }
    }
    components
}

/// Attribute findings to the component whose file they name.
fn mark_findings(components: &mut BTreeMap<String, Component>, findings: &[Finding], depth: usize) {
    for finding in findings {
        // A finding whose path names no changed file — the description lane's
        // `(pull request description)`, say — belongs to no component, and
        // inventing one for it would put a box on the diagram that no file is
        // in.
        let Some(component) = components.get_mut(&group::component_of(&finding.path, depth)) else {
            continue;
        };
        component.findings += 1;
        component.worst = Some(match component.worst {
            Some(worst) => worst.max(finding.severity),
            None => finding.severity,
        });
    }
}

/// Fold the walked nodes the change did *not* touch into components.
fn impacted_components(
    walk: &Neighbourhood,
    changed_paths: &BTreeSet<String>,
    depth: usize,
) -> BTreeMap<String, Component> {
    // Distinct files, not nodes: a file with forty symbols in the walk is one
    // file's worth of exposure, and counting its symbols would make whichever
    // file happens to be most densely parsed look like the biggest risk.
    let mut files_by_component: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for node in &walk.nodes {
        if changed_paths.contains(&node.path) {
            continue;
        }
        files_by_component
            .entry(group::component_of(&node.path, depth))
            .or_default()
            .insert(node.path.clone());
    }

    files_by_component
        .into_iter()
        .map(|(name, files)| {
            (
                name.clone(),
                Component {
                    name,
                    role: Role::Impacted,
                    files: files.len(),
                    additions: 0,
                    deletions: 0,
                    findings: 0,
                    worst: None,
                    paths: Vec::new(),
                },
            )
        })
        .collect()
}

/// Aggregate the walked edges into arrows between drawn components.
fn links(walk: &Neighbourhood, components: &[Component], depth: usize, max: usize) -> Vec<Link> {
    let index: BTreeMap<&str, usize> = components
        .iter()
        .enumerate()
        .map(|(i, c)| (c.name.as_str(), i))
        .collect();

    let mut weights: BTreeMap<(usize, usize), usize> = BTreeMap::new();
    for edge in &walk.edges {
        // `Defines` runs from a file to its own symbol, so it is a self-link by
        // construction and carries no information about what reaches what.
        if edge.kind == EdgeKind::Defines {
            continue;
        }
        let from = group::component_of(file_of(&edge.from), depth);
        let to = group::component_of(file_of(&edge.to), depth);
        if from == to {
            continue;
        }
        let (Some(&from), Some(&to)) = (index.get(from.as_str()), index.get(to.as_str())) else {
            continue;
        };
        *weights.entry((from, to)).or_default() += 1;
    }

    let mut links: Vec<Link> = weights
        .into_iter()
        .map(|((from, to), weight)| Link { from, to, weight })
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
