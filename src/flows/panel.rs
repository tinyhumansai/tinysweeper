//! The panel: what graph a lane actually runs.
//!
//! One review unit — a file for `critique` and `security`, the whole pull
//! request for `tests` and `description` — becomes this shape:
//!
//! ```text
//!   trigger
//!     ├─ agent lens:<a> ─┐
//!     ├─ agent lens:<b> ─┼─ merge ─► (proposals settled in Rust)
//!     └─ agent lens:<c> ─┘
//! ```
//!
//! The lenses are concurrent successors of one port, which is tinyflows' graph
//! fan-out, and `merge` is the fan-in barrier that waits for all of them. The
//! verify round is a second graph over the proposals — built once their number
//! is known, which is why it cannot be one graph with the propose round.
//!
//! ## Why the lens set is per-lane data
//!
//! A lane's lenses are the lane's subject matter split into readings that do
//! not overlap. Getting that split wrong is expensive in both directions: two
//! lenses that overlap pay twice for one opinion, and a gap between them is a
//! blind spot nothing reports. So they live beside the lane's prompt rather
//! than being generated, and each carries the sentence that tells the model
//! what it alone is responsible for.

use serde_json::{Value, json};
use tinyflows::model::{Edge, Node, NodeKind, WorkflowGraph};

use crate::config::types::LaneId;
use crate::flows::tier::Tier;

/// How many files a lane reviews at once.
///
/// Inherited from the semaphore this replaced. The number is about spend and
/// provider rate limits, not CPU: these tasks are almost entirely waiting on a
/// model.
pub const MAX_CONCURRENT_FILES: usize = 8;

/// How many verifiers judge one proposal.
///
/// Odd, so a majority always exists — see `consensus::settle`, where a tie
/// drops the finding. Three is the smallest odd number greater than one, and
/// each is a `flash` call against a single proposal, so the round costs a small
/// fraction of the propose round it is filtering.
pub const VERIFIERS: usize = 3;

/// One reading of the evidence.
pub struct Lens {
    /// Short, stable id. Appears in node ids and in finding attribution, so it
    /// is part of the golden tests' surface.
    pub id: &'static str,
    /// What this lens alone is responsible for noticing. Appended to the lane's
    /// system prompt.
    pub charter: &'static str,
}

/// The lenses a lane's panel is split into.
///
/// Every set below partitions its lane's subject matter: no two lenses are
/// asked to look for the same thing, because a panel that agrees by
/// construction has bought nothing.
pub fn lenses(lane: LaneId) -> &'static [Lens] {
    match lane {
        LaneId::Critique => &[
            Lens {
                id: "correctness",
                charter: "You are reading for logic that is wrong: an off-by-one, an inverted \
                          condition, a case the code does not handle, a value that can be null \
                          or empty where the code assumes it cannot. Ignore style, naming, \
                          performance and test coverage entirely — other reviewers own those.",
            },
            Lens {
                id: "contracts",
                charter: "You are reading for broken agreements between callers and callees: a \
                          changed signature whose callers were not updated, an error now \
                          swallowed, a return value whose meaning shifted, a resource acquired \
                          and not released. Ignore internal logic, style and tests entirely — \
                          other reviewers own those.",
            },
            Lens {
                id: "concurrency",
                charter: "You are reading for state that two things can touch at once, and for \
                          ordering the code assumes but does not enforce: a check separated \
                          from the action it guards, a lock not held across a read-then-write, \
                          state shared across an await point. If the change has no concurrency, \
                          report nothing rather than reaching.",
            },
        ],
        LaneId::Security => &[
            Lens {
                id: "input",
                charter: "You are reading for untrusted input reaching a dangerous sink: a \
                          query, a command, a path, a deserializer, a template. Trace where the \
                          value came from before deciding it is safe.",
            },
            Lens {
                id: "authz",
                charter: "You are reading for who is allowed to do what: an authorisation check \
                          that moved, weakened or disappeared, a permission widened, a secret \
                          or token handled somewhere it was not before.",
            },
            Lens {
                id: "adjudicate",
                charter: "You are adjudicating the deterministic scanner findings supplied as \
                          evidence: for each, say whether it is real in this context and why. \
                          You may not remove one — a scanner match is a fact and your verdict \
                          adds context to it. Report new findings only where you see something \
                          the scanners could not.",
            },
        ],
        LaneId::Tests => &[
            Lens {
                id: "coverage",
                charter: "You are reading for changed behaviour that no test exercises: a new \
                          branch, a new error path, a boundary the change introduced.",
            },
            Lens {
                id: "honesty",
                charter: "You are reading the tests themselves for assertions that would pass \
                          whatever the code did: a test asserting on a mock it configured, an \
                          assertion on a value the test computed the same way the code does, a \
                          test with no assertion at all.",
            },
        ],
        LaneId::Description => &[Lens {
            id: "description",
            charter: "You are judging whether the pull request's own description accounts for \
                      what the diff actually does.",
        }],
        // `commits` makes no model call at all — its verdict is a regular
        // expression's. It has no panel, and asking for one is a bug in the
        // caller rather than something to paper over with a default lens.
        LaneId::Commits => &[],
    }
}

/// Build one `agent` node.
///
/// `config` is handed to [`crate::flows::caps::ModelCapability`] verbatim — this
/// crate authors both ends of that contract, so the keys written here are the
/// keys read there.
fn agent_node(id: &str, tier: Tier, system: &str, prompt: &str, schema: Value, name: &str) -> Node {
    Node {
        id: id.to_string(),
        kind: NodeKind::Agent,
        type_version: 1,
        name: name.to_string(),
        config: json!({
            "tier": tier.as_str(),
            "system": system,
            "prompt": prompt,
            "schema": schema,
            "schema_name": name,
        }),
        ports: Vec::new(),
        position: None,
    }
}

/// The propose round: every lens over one unit of evidence, concurrently.
///
/// `system_of` is called once per lens to compose the lane's own prompt prefix
/// with that lens's charter. It takes the lens rather than a pre-built string so
/// the cacheable prefix is assembled by `harness::prompt`, which is the only
/// thing that knows what is safe to put in it.
pub fn propose_graph(
    lane: LaneId,
    schema: Value,
    suffix: &str,
    mut system_of: impl FnMut(&Lens) -> String,
) -> WorkflowGraph {
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

    for lens in lenses(lane) {
        let id = format!("lens_{}", lens.id);
        nodes.push(agent_node(
            &id,
            // Every panellist runs on `flash`. The panel is the quality
            // mechanism now; paying `deep` prices for each member would make it
            // strictly more expensive than the single call it replaced.
            Tier::Flash,
            &system_of(lens),
            suffix,
            schema.clone(),
            &format!("tinysweeper_{}_{}", lane_slug(lane), lens.id),
        ));
        edges.push(Edge {
            from_node: "trigger".into(),
            from_port: "main".into(),
            to_node: id.clone(),
            to_port: "main".into(),
        });
        edges.push(Edge {
            from_node: id,
            from_port: "main".into(),
            to_node: "panel".into(),
            to_port: "main".into(),
        });
    }

    // The fan-in barrier. Present even for a one-lens lane so the node a run
    // reads its results from has one name across every lane.
    nodes.push(Node {
        id: "panel".into(),
        kind: NodeKind::Merge,
        type_version: 1,
        name: "panel".into(),
        config: json!({ "mode": "append" }),
        ports: Vec::new(),
        position: None,
    });

    WorkflowGraph {
        name: format!("{}-propose", lane_slug(lane)),
        nodes,
        edges,
        ..WorkflowGraph::default()
    }
}

/// The verify round: independent judges over one proposal.
///
/// Deliberately a separate graph. The number of proposals is not known until
/// the propose round has finished, and a graph whose width depends on an
/// earlier node's output is exactly the case `execution: per_item` exists for —
/// but each verifier needs the *proposal text* in its prompt, which is prompt
/// assembly rather than item mapping, so it is built here instead.
pub fn verify_graph(lane: LaneId, schema: Value, system: &str, prompt: &str) -> WorkflowGraph {
    let mut nodes = vec![Node {
        id: "trigger".into(),
        kind: NodeKind::Trigger,
        type_version: 1,
        name: "proposal".into(),
        config: Value::Null,
        ports: Vec::new(),
        position: None,
    }];
    let mut edges = Vec::new();

    for n in 0..VERIFIERS {
        let id = format!("verifier_{n}");
        nodes.push(agent_node(
            &id,
            Tier::Flash,
            system,
            prompt,
            schema.clone(),
            &format!("tinysweeper_{}_verify", lane_slug(lane)),
        ));
        edges.push(Edge {
            from_node: "trigger".into(),
            from_port: "main".into(),
            to_node: id.clone(),
            to_port: "main".into(),
        });
        edges.push(Edge {
            from_node: id,
            from_port: "main".into(),
            to_node: "panel".into(),
            to_port: "main".into(),
        });
    }

    nodes.push(Node {
        id: "panel".into(),
        kind: NodeKind::Merge,
        type_version: 1,
        name: "panel".into(),
        config: json!({ "mode": "append" }),
        ports: Vec::new(),
        position: None,
    });

    WorkflowGraph {
        name: format!("{}-verify", lane_slug(lane)),
        nodes,
        edges,
        ..WorkflowGraph::default()
    }
}

/// The lane's wire name, used in node and schema names.
fn lane_slug(lane: LaneId) -> &'static str {
    match lane {
        LaneId::Critique => "critique",
        LaneId::Security => "security",
        LaneId::Tests => "tests",
        LaneId::Description => "description",
        LaneId::Commits => "commits",
    }
}

#[cfg(test)]
#[path = "panel_test.rs"]
mod tests;
