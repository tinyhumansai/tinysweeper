//! Sub-agents, and the one level of depth they are allowed.
//!
//! A panellist reading one file often cannot settle a question from that file
//! alone: whether a caller elsewhere already validates this argument, whether
//! the helper being called has the behaviour the code assumes. Before, that
//! uncertainty had nowhere to go and came out as a hedged finding — the kind a
//! human has to go and check, which is the work the review was supposed to do.
//!
//! So a reviewer may end its turn with **questions** instead of guessing. Each
//! one is dispatched to a child workflow that answers it against the evidence
//! already gathered, and the reviewer then gets **one** more turn with those
//! answers in hand. What it says on that turn is what counts.
//!
//! The direction matters. This makes a reviewer *find more* — it is the same
//! argument `src/council` makes for a second reviewer, and the opposite of
//! asking a second model whether the first was right, which `src/falsify`
//! explains at length deletes the findings that needed context to notice.
//! Nothing here can remove a finding; removal stays falsify's job.
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
//! The reason for the bound is cost, and it compounds rather than adds: files
//! times reviewers times questions is already the widest part of a review, and
//! a second level multiplies it again for answers that are, by then, about
//! evidence nobody has looked at directly.
//!
//! ## Nothing is spent when nothing is asked
//!
//! A reviewer with no questions costs exactly what it cost before: one call.
//! The follow-up turn happens only for reviewers that asked, and only when at
//! least one answer came back — re-asking with no new evidence is a second call
//! that cannot say anything the first did not.

use serde_json::{Value, json};
use tinyflows::model::{Edge, Node, NodeKind, WorkflowGraph};

/// How many questions one reviewer may ask.
///
/// A cap rather than a budget line because the failure it prevents is not
/// expense but drift: a reviewer that asks twenty questions has stopped
/// reviewing the diff and started exploring the repository, and the answers
/// arrive too late in the run to be worth that.
pub const MAX_QUESTIONS_PER_REVIEWER: usize = 3;

/// The instruction that tells a reviewer it may ask instead of guessing.
///
/// Appended to the *end of the cacheable prefix* rather than the evidence: it
/// is a constant, so the prefix stays byte-identical run to run and the cache
/// still hits. It also has to answer the schema's own "there is no second
/// turn" line, which is true of the last turn and false of this one.
pub const ASK_INSTRUCTION: &str = "\n\n## Asking instead of guessing\n\nIf something you cannot see would change your verdict — whether a caller already validates this argument, whether the helper being called behaves as the code assumes — put it in `questions` rather than reporting a hedged finding. Each question is answered from the repository and you are asked once more with the answers, which is the turn your verdict is taken from. Ask only what would change what you report: a question whose answer you would ignore costs a call and buys nothing. If nothing is in doubt, omit the key.";

/// The schema a reviewer's questions are reported under.
///
/// Additive to the lane response schema: a reviewer with nothing to ask omits
/// the key entirely, which is why it is not required.
pub fn questions_schema() -> Value {
    json!({
        "type": "array",
        "maxItems": MAX_QUESTIONS_PER_REVIEWER,
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

/// How many times a sub-agent may call a tool before it must answer.
///
/// Small on purpose. A sub-agent answers *one narrow factual question*, and the
/// questions worth asking are settled by reading a file or two. A loop with
/// room to wander is one that spends a reviewer's latency budget confirming
/// something it already knew — and every round re-sends the whole accumulated
/// transcript, so the cost of round N is paid again by every round after it.
pub const MAX_TOOL_ROUNDS: usize = 3;

/// The schema a sub-agent answers under.
///
/// `with_tools` adds the optional `tool_call` key. `answer` and `confident`
/// stay required in both: a turn that asks for a file must still say what it
/// knows so far, so a sub-agent that runs out of rounds mid-loop still has an
/// answer to give rather than nothing.
pub fn answer_schema(with_tools: bool) -> Value {
    let mut schema = json!({
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
    });

    if with_tools && let Some(properties) = schema["properties"].as_object_mut() {
        properties.insert(
            "tool_call".into(),
            json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["slug", "args"],
                "description": "Set this to look something up before answering. You are asked \
                                again with the result. Omit it once you can answer.",
                "properties": {
                    "slug": {
                        "type": "string",
                        "enum": crate::flows::tools::SLUGS,
                        "description": "Which tool to call."
                    },
                    "args": {
                        "type": "object",
                        "description": "`{\"path\": \"...\"}` for read_file, \
                                        `{\"pattern\": \"...\"}` for search."
                    }
                }
            }),
        );
    }

    schema
}

/// What a tool-capable sub-agent is told about its tools.
///
/// Appended to [`ANSWER_SYSTEM`] rather than replacing it: everything that
/// prompt says about not reviewing and not guessing is still true, and more so
/// once the sub-agent can go and read the file it is speculating about.
pub const TOOL_INSTRUCTION: &str = "\n\nYou may look things up before answering. Set `tool_call` to read a file or search the repository for a literal string, and you will be asked again with the result. Prefer looking something up over answering with `confident` false — that is what the tools are for. Once you can answer, omit `tool_call`. You have a small number of lookups, so ask for what would settle the question rather than for background.";

/// Render one tool result for the next turn of the loop.
///
/// The call is echoed alongside its result. A transcript of results with no
/// calls reads, by the third round, as a pile of unexplained file contents, and
/// the sub-agent starts answering about the wrong one.
pub fn render_tool_result(slug: &str, args: &Value, result: &Value) -> String {
    format!(
        "\n\n### Lookup: `{slug}` {}\n\n```json\n{}\n```\n",
        serde_json::to_string(args).unwrap_or_default(),
        serde_json::to_string_pretty(result).unwrap_or_default(),
    )
}

/// Render a refused tool call for the next turn.
///
/// A refusal has to come back *as a turn*, not as a dropped call. A sub-agent
/// whose call vanished asks for the same thing again and burns every remaining
/// round on it; one that is told why adjusts or answers.
pub fn render_tool_refusal(slug: &str, why: &str) -> String {
    format!("\n\n### Lookup `{slug}` was refused\n\n{why}\n")
}

/// Add the `questions` key to a lane's response schema.
///
/// Not optional plumbing: the lane schema is `additionalProperties: false`, so
/// a reviewer answering with a key the schema does not declare is a refusal
/// under strict mode and a parse failure under `json_object`. It stays out of
/// `required` so a reviewer with nothing to ask answers exactly the schema it
/// always did.
pub fn with_questions(mut schema: Value) -> Value {
    // `properties` is created when absent rather than skipped. Returning the
    // schema unchanged would be the worst outcome available: the reviewer is
    // still told it may ask (the instruction and the schema are set together),
    // and it would be answering a schema with nowhere to put the question —
    // rejected outright under strict mode, silently dropped under
    // `json_object`. Either way the follow-up turn never happens and nothing
    // reports why.
    if let Some(object) = schema.as_object_mut() {
        object
            .entry("properties")
            .or_insert_with(|| json!({}))
            .as_object_mut()
            .map(|properties| properties.insert("questions".into(), questions_schema()));
    }
    schema
}

/// The node id one question's answer lands under.
pub fn node_id(index: usize) -> String {
    format!("answer_{index}")
}

/// The child graph a batch of questions is answered by.
///
/// One `agent` node per question, all concurrent. Still nothing but a trigger
/// and agents: everything this module promises about depth rests on there being
/// no node here that could run another graph.
///
/// Takes whole `prompts` rather than questions plus shared evidence because the
/// tool loop grows each one independently — by round three, question 1 may have
/// read two files and question 2 none, and their prompts have nothing left in
/// common but the diff they started from.
///
/// `with_tools` decides only the schema and the system prompt. The graph never
/// invokes a tool itself; see [`crate::flows::tools`] for why the invocation
/// stays in host code.
pub fn answers_graph(model: &str, prompts: &[String], with_tools: bool) -> WorkflowGraph {
    let mut nodes = vec![Node {
        id: "trigger".into(),
        kind: NodeKind::Trigger,
        type_version: 1,
        name: "questions".into(),
        config: Value::Null,
        ports: Vec::new(),
        position: None,
    }];
    let mut edges = Vec::new();

    let system = match with_tools {
        true => format!("{ANSWER_SYSTEM}{TOOL_INSTRUCTION}"),
        false => ANSWER_SYSTEM.to_string(),
    };

    for (index, prompt) in prompts.iter().enumerate() {
        let id = node_id(index);

        nodes.push(Node {
            id: id.clone(),
            kind: NodeKind::Agent,
            type_version: 1,
            name: "tinysweeper_subagent_answer".into(),
            config: json!({
                "model": model,
                "system": system,
                "prompt": prompt,
                "schema": answer_schema(with_tools),
                "schema_name": "tinysweeper_subagent_answer",
                // A question that cannot be answered is a question left
                // unanswered, never a failed review.
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
            to_node: "answers".into(),
            to_port: "main".into(),
        });
    }

    nodes.push(Node {
        id: "answers".into(),
        kind: NodeKind::Merge,
        type_version: 1,
        name: "answers".into(),
        config: json!({ "mode": "append" }),
        ports: Vec::new(),
        position: None,
    });

    WorkflowGraph {
        name: "subagent-answers".into(),
        nodes,
        edges,
        ..WorkflowGraph::default()
    }
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
                    "schema": answer_schema(false),
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
/// reported findings would be a reviewer nobody configured, answering a
/// question the lane never asked — its output reaches the reviewer's second
/// turn as *evidence*, and evidence that has already made up its mind is worth
/// less than none.
pub const ANSWER_SYSTEM: &str = "\
You answer one narrow, factual question about a codebase, using only the \
evidence supplied below. You are not reviewing anything: do not report \
problems, do not suggest changes, and do not say whether any finding is \
justified. If the evidence does not settle the question, set `confident` to \
false and say what is missing. A wrong confident answer is far worse than an \
honest \"the evidence does not say\".";

/// One question a reviewer asked, and what came back.
#[derive(Debug, Clone)]
pub struct Answered {
    /// The question, as the reviewer phrased it.
    pub question: String,
    /// The sub-agent's answer.
    pub answer: String,
    /// Whether the evidence settled it.
    pub confident: bool,
}

/// Render answered questions for the reviewer's second turn.
///
/// Unconfident answers are kept rather than dropped, and labelled. "The
/// evidence does not say" is a real input to a reviewer's verdict: it is the
/// difference between a doubt that was resolved and one that could not be, and
/// hiding the latter would let the reviewer read silence as confirmation.
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
