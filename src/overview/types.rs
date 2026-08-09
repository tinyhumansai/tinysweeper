//! The core types of the change map: components, the links between them, and
//! how much of the picture the graph was actually able to supply.
//!
//! Always compiled. Nothing here reads a store or renders anything — `build`
//! fills these in from a diff and a neighbourhood, and `mermaid` / `render`
//! turn them into a comment.

use serde::{Deserialize, Serialize};

use crate::config::types::Severity;

/// What a component's relationship to the change is.
///
/// The distinction is the whole point of the picture. A reviewer can see the
/// changed files in the diff; what they cannot see is which *untouched* parts
/// of the repository sit on the other end of an import from them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// The pull request edits files here.
    Changed,
    /// Untouched, but the graph reaches it from a changed file.
    Impacted,
}

/// One box in the diagram: a directory's worth of files, treated as a unit.
///
/// A component is a *directory*, not a semantic module. Nothing here knows
/// what a repository considers a component, and inventing one from a model's
/// opinion would make the picture unreproducible between runs of the same
/// commit. A directory is a claim the repository itself made.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Component {
    /// The directory this component stands for, or `(root)` for files with no
    /// directory at all.
    pub name: String,
    /// Whether the change edits this component or merely reaches it.
    pub role: Role,
    /// How many files of this component the pull request changed.
    pub files: usize,
    /// Lines added across those files.
    pub additions: usize,
    /// Lines removed across those files.
    pub deletions: usize,
    /// How many findings this review raised against files here.
    pub findings: usize,
    /// The worst severity among them, when there are any.
    pub worst: Option<Severity>,
    /// The changed paths, for the table under the diagram. Capped by
    /// [`crate::config::types::Overview::max_paths_per_component`]; `files` is
    /// the untruncated count.
    pub paths: Vec<String>,
}

impl Component {
    /// Total churn, which is how components are ranked when there are too many
    /// to draw.
    pub fn churn(&self) -> usize {
        self.additions + self.deletions
    }
}

/// An aggregated edge between two components.
///
/// Aggregated on purpose: drawing one arrow per import edge produces a hairball
/// at about thirty files, and the count carries the same information in a form
/// that survives being looked at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Link {
    /// Index into [`ChangeMap::components`] of the importing side.
    pub from: usize,
    /// Index into [`ChangeMap::components`] of the imported side.
    pub to: usize,
    /// How many underlying graph edges this arrow stands for.
    pub weight: usize,
}

/// How much of the picture the code graph supplied.
///
/// Stated rather than inferred from an empty `links` list, because the three
/// cases below need three different things done about them and they are
/// indistinguishable from the outside. This is the same rule
/// [`crate::retrieve`] follows: a degraded run must not be a silent one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GraphStatus {
    /// No graph store was attached — a forge-only deployment, or an install
    /// with no index. The map is the diff's own shape and nothing more.
    Off,
    /// A graph was attached but knew nothing about the changed files. Normal
    /// for a pull request that only adds files, and a symptom of a cold index
    /// otherwise.
    Cold,
    /// The walk ran and reached this many nodes.
    Walked {
        /// Nodes reached, before they were folded into components.
        nodes: usize,
    },
}

/// The finished map, ready to render.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangeMap {
    /// Every component, changed ones first, each side ordered by churn.
    pub components: Vec<Component>,
    /// Arrows between them.
    pub links: Vec<Link>,
    /// Files the pull request changed, across every component including the
    /// ones that did not fit.
    pub files: usize,
    /// Lines added, across every changed file.
    pub additions: usize,
    /// Lines removed, across every changed file.
    pub deletions: usize,
    /// Components that were left out to keep the diagram legible.
    ///
    /// Reported rather than dropped quietly: a picture that silently omits
    /// half the change is worse than no picture, because it reads as complete.
    pub folded: usize,
    /// What the graph contributed.
    pub graph: GraphStatus,
}

impl ChangeMap {
    /// Whether there is anything worth drawing.
    ///
    /// A single-component change with no links is a box on its own, which
    /// tells a reviewer nothing they did not get from the file list.
    pub fn worth_drawing(&self) -> bool {
        self.components.len() > 1 || !self.links.is_empty()
    }

    /// The components the change edits.
    pub fn changed(&self) -> impl Iterator<Item = &Component> {
        self.components.iter().filter(|c| c.role == Role::Changed)
    }

    /// The components it only reaches.
    pub fn impacted(&self) -> impl Iterator<Item = &Component> {
        self.components.iter().filter(|c| c.role == Role::Impacted)
    }
}
