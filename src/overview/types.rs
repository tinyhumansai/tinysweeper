//! The core types of the change flow: named behaviours, the links between
//! them, and how much of the picture the graph was actually able to supply.
//!
//! Always compiled. Nothing here reads a store or renders anything — `build`
//! fills these in from a diff and a neighbourhood, and `mermaid` / `render`
//! turn them into a comment.

use serde::{Deserialize, Serialize};

use crate::config::types::Severity;

/// What a behaviour's relationship to the change is.
///
/// The distinction is the whole point of the picture. A reviewer can see the
/// changed files in the diff; what they cannot see is which *untouched* parts
/// of the repository sit on the other end of an import from them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// The pull request edits this behaviour.
    Changed,
    /// Untouched behaviour connected to one the pull request edits.
    Impacted,
}

/// One named behaviour in the diagram.
///
/// `name` is the graph's stable symbol id (`path#symbol`). Renderers show only
/// the symbol half: the path keeps same-named symbols distinct without turning
/// the diagram back into a file inventory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Component {
    /// Stable graph node id for the behaviour.
    pub name: String,
    /// Whether the change edits this behaviour or it supplies flow context.
    pub role: Role,
    /// Legacy count retained in the proposal wire shape. New maps do not render
    /// it because files are not the unit of the flow.
    pub files: usize,
    /// Legacy churn retained for proposal compatibility.
    pub additions: usize,
    /// Legacy churn retained for proposal compatibility.
    pub deletions: usize,
    /// How many findings this review raised against files here.
    pub findings: usize,
    /// The worst severity among them, when there are any.
    pub worst: Option<Severity>,
    /// The source path that owns the behaviour, used only to attach findings.
    /// It is deliberately not rendered.
    pub paths: Vec<String>,
}

impl Component {
    /// Legacy churn total.
    pub fn churn(&self) -> usize {
        self.additions + self.deletions
    }
}

/// An aggregated edge between two behaviours.
///
/// Aggregated on purpose: drawing one arrow per import edge produces a hairball
/// at about thirty files, and the count carries the same information in a form
/// that survives being looked at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Link {
    /// Index into [`ChangeMap::components`] of the source behaviour.
    pub from: usize,
    /// Index into [`ChangeMap::components`] of the target behaviour.
    pub to: usize,
    /// The semantic relationship carried by the arrow.
    #[serde(default)]
    pub relation: FlowRelation,
    /// How many underlying graph edges this arrow stands for.
    pub weight: usize,
}

/// A human-readable relationship between two behaviours.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FlowRelation {
    /// The source invokes the target.
    Calls,
    /// The source uses the target without invoking it.
    #[default]
    Uses,
    /// The source implements, extends, or embeds the target.
    Implements,
    /// The source test exercises the target.
    Tests,
}

impl FlowRelation {
    /// Text placed on an arrow in the flowchart.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Calls => "calls",
            Self::Uses => "uses",
            Self::Implements => "implements",
            Self::Tests => "tests",
        }
    }
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
    /// A graph is attached, but the walk could not be run — the store was
    /// unreachable, or it refused the query.
    ///
    /// Distinct from [`GraphStatus::Cold`] and from [`GraphStatus::Off`]
    /// because it is the only one of the three that is a *fault*: the other two
    /// are deployments working as configured, and folding an outage into either
    /// would make the one case somebody has to fix the one case nobody is told
    /// about.
    Unavailable,
    /// A graph was attached but knew nothing about the changed files. Normal
    /// for a pull request that only adds files, and a symptom of a cold index
    /// otherwise.
    Cold,
    /// The walk ran and reached this many nodes.
    Walked {
        /// Nodes reached, before they were folded into behaviours.
        nodes: usize,
    },
}

/// The finished map, ready to render.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangeMap {
    /// Every named behaviour, changed ones first.
    pub components: Vec<Component>,
    /// Arrows between them.
    pub links: Vec<Link>,
    /// Legacy diff total retained in the proposal wire shape.
    pub files: usize,
    /// Legacy diff total retained in the proposal wire shape.
    pub additions: usize,
    /// Legacy diff total retained in the proposal wire shape.
    pub deletions: usize,
    /// Behaviours that were left out to keep the diagram legible.
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
    /// A set of disconnected names is not a flow. Only draw when the graph can
    /// explain at least one relationship.
    pub fn worth_drawing(&self) -> bool {
        !self.links.is_empty()
    }

    /// The behaviours the change edits.
    pub fn changed(&self) -> impl Iterator<Item = &Component> {
        self.components.iter().filter(|c| c.role == Role::Changed)
    }

    /// The surrounding behaviours shown for context.
    pub fn impacted(&self) -> impl Iterator<Item = &Component> {
        self.components.iter().filter(|c| c.role == Role::Impacted)
    }
}
