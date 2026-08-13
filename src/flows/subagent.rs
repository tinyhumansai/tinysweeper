//! Sub-agents, and the one level of depth they are allowed.
//!
//! A panellist reading one file often cannot settle a question from that file
//! alone: whether a caller elsewhere already validates this argument, whether
//! the helper being called has the behaviour the code assumes. Before, that
//! uncertainty had nowhere to go and came out as a hedged finding — the kind a
//! human has to go and check, which is the work the review was supposed to do.
//!
//! So a lens may end its turn with **questions** instead of guessing. Each one
//! is dispatched to a child workflow that answers it against the evidence
//! already gathered, and the answers are handed to the verify round, which is
//! where a claim actually lives or dies.
//!
//! ## The depth bound is structural, not a counter
//!
//! Exactly one level. Not enforced by threading a depth integer through the
//! run — that is a bound a future edit removes by accident — but by what the
//! child graph *is*: [`answer_graph`] contains `agent` nodes and nothing else,
//! and the resolver in [`crate::flows::caps::ChildGraphs`] is populated only
//! with graphs this module builds. A sub-agent therefore has no `sub_workflow`
//! node to reach for and no registry entry it could name if it had one. The
//! test at the bottom of this file is what keeps that true.
//!
//! The reason for the bound is cost, and it compounds rather than adds: N files
//! times M lenses times Q questions is already the widest part of a review, and
//! a second level multiplies it again for answers that are, by then, about
//! evidence nobody has looked at directly.

use serde_json::{Value, json};
use tinyflows::model::{Edge, Node, NodeKind, WorkflowGraph};

/// How many questions one lens may ask.
///
/// A cap rather than a budget line because the failure it prevents is not
/// expense but drift: a panellist that asks twenty questions has stopped
/// reviewing the diff and started exploring the repository, and the answers
/// arrive too late in the run to be worth that.
pub const MAX_QUESTIONS_PER_LENS: usize = 3;

/// The workflow id a lens's questions are dispatched to.
pub const ANSWER_WORKFLOW: &str = "tinysweeper.subagent.answer";

/// The schema a lens's questions are reported under.
///
/// Additive to the lane response schema: a lens that has nothing to ask omits
/// the key entirely, which is why it is not required.
pub fn questions_schema() -> Value {
    json!({
        "type": "array",
        "maxItems": MAX_QUESTIONS_PER_LENS,
        "items": {
            "type": "object",
            "additionalProperties": false,
            "required": ["question", "why"],
            "properties": {
                "question": {
                    "type": "string",
                    "description": "One narrow, factual question about the codebase that would \
                                    settle whether a finding you are considering is real."
                },
                "why": {
                    "type": "string",
                    "description": "What you would conclude from each possible answer."
                }
            }
        }
    })
}

/// The schema a sub-agent answers under.
pub fn answer_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["answer", "confident"],
        "properties": {
            "answer": {
                "type": "string",
                "description": "The answer, in one or two sentences, from the supplied evidence \
                                only."
            },
            "confident": {
                "type": "boolean",
                "description": "False when the supplied evidence does not settle it. Saying so \
                                is the correct answer; guessing is not."
            }
        }
    })
}

/// The child graph one question is answered by.
///
/// A single `agent` node and a trigger. Deliberately the smallest graph that
/// can exist: everything this module promises about depth rests on there being
/// nothing else in here.
pub fn answer_graph(model: &str, system: &str, prompt: &str) -> WorkflowGraph {
    WorkflowGraph {
        name: "subagent-answer".into(),
        nodes: vec![
            Node {
                id: "trigger".into(),
                kind: NodeKind::Trigger,
                type_version: 1,
                name: "question".into(),
                config: Value::Null,
                ports: Vec::new(),
                position: None,
            },
            Node {
                id: "answer".into(),
                kind: NodeKind::Agent,
                type_version: 1,
                name: "tinysweeper_subagent_answer".into(),
                config: json!({
                    // Whatever tier the caller picked, which should be the
                    // cheapest available: a sub-agent answers one narrow
                    // factual question against evidence already in hand, the
                    // least demanding call a review makes.
                    "model": model,
                    "system": system,
                    "prompt": prompt,
                    "schema": answer_schema(),
                    "schema_name": "tinysweeper_subagent_answer",
                }),
                ports: Vec::new(),
                position: None,
            },
        ],
        edges: vec![Edge {
            from_node: "trigger".into(),
            from_port: "main".into(),
            to_node: "answer".into(),
            to_port: "main".into(),
        }],
        ..WorkflowGraph::default()
    }
}

/// The system prompt a sub-agent answers under.
///
/// It is told it may not conclude anything about the review. A sub-agent that
/// reports findings would be a panellist nobody voted on — its output reaches
/// the verify round as *evidence*, and evidence that has already made up its
/// mind is worth less than none.
pub const ANSWER_SYSTEM: &str = "\
You answer one narrow, factual question about a codebase, using only the \
evidence supplied below. You are not reviewing anything: do not report \
problems, do not suggest changes, and do not say whether any finding is \
justified. If the evidence does not settle the question, set `confident` to \
false and say what is missing. A wrong confident answer is far worse than an \
honest \"the evidence does not say\".";

/// One question a lens asked, and what came back.
#[derive(Debug, Clone)]
pub struct Answered {
    /// The question, as the lens phrased it.
    pub question: String,
    /// The sub-agent's answer.
    pub answer: String,
    /// Whether the evidence settled it.
    pub confident: bool,
}

/// Render answered questions for the verify round's prompt.
///
/// Unconfident answers are kept rather than dropped, and labelled. "The
/// evidence does not say" is a real input to whether a finding survives — it is
/// the difference between a verifier confirming a claim and a verifier having
/// no way to check it.
pub fn render(answers: &[Answered]) -> String {
    if answers.is_empty() {
        return String::new();
    }

    let mut out = String::from("\nWhat the reviewer asked, and what was found:\n");
    for answered in answers {
        out.push_str(&format!(
            "- Q: {}\n  A: {}{}\n",
            answered.question.trim(),
            answered.answer.trim(),
            if answered.confident {
                ""
            } else {
                " (the evidence did not settle this)"
            }
        ));
    }
    out
}

#[cfg(test)]
#[path = "subagent_test.rs"]
mod tests;
