//! The graph a lane's reviewers run as.
//!
//! `src/council` decides *who* reviews and what becomes of their findings. This
//! module is *how they run*: one `agent` node per reviewer, all of them
//! concurrent successors of the trigger, joined by a `merge` barrier.
//!
//! ```text
//!   evidence ─┬─ agent: reviewer-a ─┐
//!             ├─ agent: reviewer-b ─┼─ merge ─► one answer per reviewer
//!             └─ agent: reviewer-c ─┘
//! ```
//!
//! ## Why a graph rather than a loop
//!
//! A council multiplies calls: files × reviewers. Run serially — which is what
//! it was, because a budget can only be checked once a call has returned — a
//! three-agent council on a twenty-file pull request is sixty round trips end
//! to end. The ceiling now lives in [`crate::flows::caps::ModelCapability`],
//! which refuses a call however many are in flight, so the width is free to be
//! real.
//!
//! ## What is deliberately *not* here
//!
//! There is no verification round. An earlier version of this module ran one:
//! every finding put to independent judges, majority keeps it. `src/falsify`
//! argues at length why that shape is wrong — a checker that sees less than the
//! reviewer did rejects whatever it cannot confirm, which deletes exactly the
//! findings that needed context to notice — and it is right. Removal is
//! `falsify`'s job, it rejects only what it can *prove* wrong, and it fails
//! open. Agreement between reviewers is a ranking signal, handled by
//! `council::merge`, and it never removes anything here.

use serde_json::{Value, json};
use tinyflows::model::{Edge, Node, NodeKind, WorkflowGraph};

use crate::config::types::LaneId;

/// How many files a lane reviews at once.
///
/// Inherited from the semaphore this replaced. The number is about spend and
/// provider rate limits, not CPU: these tasks are almost entirely waiting on a
/// model.
pub const MAX_CONCURRENT_FILES: usize = 8;

/// One model call the graph should make.
///
/// Assembled by the lane, because prompt layering is the lane's business and
/// which half of it is cacheable is `harness::prompt`'s — see its module docs
/// before moving anything between `system` and `prompt`.
#[derive(Debug, Clone)]
pub struct Call {
    /// The reviewer's id. Becomes the node id, so it is what a failure names.
    pub id: String,
    /// The model id, already resolved from tier by `council::reviewers`.
    pub model: String,
    /// The cacheable prefix.
    pub system: String,
    /// The volatile suffix: the evidence.
    pub prompt: String,
    /// The schema name this answer is reported under.
    pub schema_name: String,
}

/// The node id one call's answer lands under.
///
/// A reviewer id is operator-supplied config, so it is not assumed to be a
/// legal node id: anything outside `[a-z0-9_]` becomes `_`. Collisions are not
/// a concern because `config::validate` rejects a council with duplicate agent
/// ids, and a graph with two nodes of one id would fail to compile anyway.
pub fn node_id(reviewer_id: &str) -> String {
    let cleaned: String = reviewer_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect();

    format!("reviewer_{cleaned}")
}

/// Build the graph that asks every reviewer at once.
pub fn council_graph(lane: LaneId, calls: &[Call], schema: &Value) -> WorkflowGraph {
    let mut nodes = vec![Node {
        id: "trigger".into(),
        kind: NodeKind::Trigger,
        type_version: 1,
        name: "evidence".into(),
        config: Value::Null,
        ports: Vec::new(),
        position: None,
    }];
    let mut edges = Vec::new();

    for call in calls {
        let id = node_id(&call.id);

        nodes.push(Node {
            id: id.clone(),
            kind: NodeKind::Agent,
            type_version: 1,
            name: call.schema_name.clone(),
            config: json!({
                "model": call.model,
                "system": call.system,
                "prompt": call.prompt,
                "schema": schema,
                "schema_name": call.schema_name,
                // A reviewer that fails must not fail the council. The engine's
                // default is `stop`, which would lose every other reviewer's
                // work to one provider timeout — and a lane that returns
                // nothing is indistinguishable from a lane that found nothing.
                // The runner notices the missing output and reports it.
                "on_error": "continue",
            }),
            ports: Vec::new(),
            position: None,
        });

        edges.push(Edge {
            from_node: "trigger".into(),
            from_port: "main".into(),
            to_node: id.clone(),
            to_port: "main".into(),
        });
        edges.push(Edge {
            from_node: id,
            from_port: "main".into(),
            to_node: "council".into(),
            to_port: "main".into(),
        });
    }

    // The fan-in barrier. Present even for a single reviewer so the shape of a
    // solo run and a council run is the same one — which is the same reason
    // `council::reviewers` always returns at least one reviewer rather than
    // branching.
    nodes.push(Node {
        id: "council".into(),
        kind: NodeKind::Merge,
        type_version: 1,
        name: "council".into(),
        config: json!({ "mode": "append" }),
        ports: Vec::new(),
        position: None,
    });

    WorkflowGraph {
        name: format!("{}-council", lane.as_str()),
        nodes,
        edges,
        ..WorkflowGraph::default()
    }
}

#[cfg(test)]
#[path = "panel_test.rs"]
mod tests;
