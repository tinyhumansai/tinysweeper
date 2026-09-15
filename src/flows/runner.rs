//! Running a lane's reviewers concurrently, and reporting who could not be
//! reached.
//!
//! This is the whole of what the graph does for a lane: it makes N model calls
//! at once and hands back one answer per reviewer, in the order they were
//! asked. Placement, merging and removal all stay where they were — in the
//! lane, in `council`, and in `falsify` respectively — because those are the
//! steps whose behaviour the golden tests pin, and moving them into a graph
//! would buy nothing and cost the tests.
//!
//! A reviewer whose call fails is reported, not swallowed. A council that
//! returns nothing because one member timed out is a review that reads "all
//! clear" for an infrastructure reason, which is the failure every layer here
//! is arranged against.

use std::sync::Arc;

use serde_json::{Value, json};
use tinyflows::engine;

use crate::config::types::{LaneId, LookupPolicy};
use crate::error::Result;
use crate::flows::caps::{ChildGraphs, ModelCapability};
use crate::flows::lookup;
use crate::flows::panel::{self, Call};
use crate::flows::subagent::{self, Answered};
use crate::ports::model::Model;
use crate::ports::tree::{Lookup, TreeReader};

/// What one reviewer said, or why it said nothing.
#[derive(Debug, Clone)]
pub struct Answer {
    /// The reviewer's id, as configured.
    pub id: String,
    /// The structured answer, when there was one.
    pub value: Option<Value>,
    /// The model that actually answered. A fallback taking over is worth
    /// knowing about and is otherwise invisible by the time findings merge.
    pub model: String,
    /// Why there was no answer.
    pub error: Option<String>,
}

impl Answer {
    /// A reviewer that could not be reached.
    fn failed(id: &str, error: impl Into<String>) -> Self {
        Self {
            id: id.to_string(),
            value: None,
            model: String::new(),
            error: Some(error.into()),
        }
    }
}

/// The capability a whole lane shares.
///
/// One per lane, not one per file: the budget ceiling and the spend tally both
/// live in it, and a fresh one per file would let each file spend the whole
/// pull request's allowance. It is also what makes the per-file fan-out safe to
/// run concurrently — see [`crate::flows::caps::ModelCapability::new`].
pub fn lane_llm(
    model: Arc<dyn Model>,
    config: &crate::config::types::Config,
    budget_usd: f64,
) -> Arc<ModelCapability> {
    Arc::new(ModelCapability::new(model, config.models.clone()).with_budget(budget_usd))
}

/// Read one agent node's structured answer out of a finished run.
///
/// Two envelopes, not one, and the difference is easy to get wrong in a way
/// nothing reports. The engine wraps a node's result as
/// `nodes.<id>.items[0].{json, raw, text}`, and the `json` there is whatever
/// [`crate::flows::caps::ModelCapability`] returned — this crate's own
/// `{json, model}` pair. So the model's answer is two `json` hops down, and
/// stopping one hop early yields `{json, model}`, which deserializes into an
/// *empty* lane response rather than failing. That reads exactly like a
/// reviewer that found nothing.
fn node_answer(output: &Value, node_id: &str) -> Option<(Value, String)> {
    let envelope = output.get("nodes")?.get(node_id)?.get("items")?.get(0)?;
    let payload = envelope.get("json")?.get("json")?;

    Some((
        payload.get("json")?.clone(),
        payload
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string(),
    ))
}

/// Why a node produced no answer, as the engine recorded it.
fn node_error(output: &Value, node_id: &str) -> Option<String> {
    output
        .get("nodes")?
        .get(node_id)?
        .get("items")?
        .get(0)?
        .get("json")?
        .get("error")?
        .get("message")?
        .as_str()
        .map(str::to_string)
}

/// Run one round of the council graph and read an answer per call.
async fn one_round(
    capabilities: &tinyflows::caps::Capabilities,
    lane: LaneId,
    calls: &[Call],
    schema: &Value,
) -> Result<Vec<Answer>> {
    let graph = panel::council_graph(lane, calls, schema);

    let compiled = tinyflows::compiler::compile(&graph)
        .map_err(|e| crate::error::Error::Model(format!("council graph did not compile: {e}")))?;

    let outcome = engine::run(&compiled, json!({}), capabilities)
        .await
        .map_err(|e| crate::error::Error::Model(e.to_string()))?;

    Ok(calls
        .iter()
        .map(|call| {
            let node = panel::node_id(&call.id);

            match node_answer(&outcome.output, &node) {
                Some((value, model)) => Answer {
                    id: call.id.clone(),
                    value: Some(value),
                    model,
                    error: None,
                },
                None => Answer::failed(
                    &call.id,
                    node_error(&outcome.output, &node)
                        .unwrap_or_else(|| "the reviewer produced no answer".into()),
                ),
            }
        })
        .collect())
}

/// The questions one answer carried, capped.
///
/// The cap is applied here as well as in the schema: a schema is a request, and
/// under `json_object` the provider is not enforcing it at all. This is the
/// number of sub-agents that actually get spawned.
fn read_questions(value: &Value) -> Vec<String> {
    value
        .get("questions")
        .and_then(Value::as_array)
        .map(|questions| {
            questions
                .iter()
                .filter_map(|q| q.get("question").and_then(Value::as_str))
                .map(str::trim)
                .filter(|q| !q.is_empty())
                .take(subagent::MAX_QUESTIONS_PER_REVIEWER)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// Answer one reviewer's questions, one sub-agent each, all at once.
///
/// A question that could not be answered is simply absent from the result. It
/// was a request for more certainty; failing to get it leaves the reviewer
/// exactly where it would have been without sub-agents.
async fn answer_questions(
    capabilities: &tinyflows::caps::Capabilities,
    model: &str,
    questions: &[String],
    evidence: &str,
) -> Vec<Answered> {
    let graph = subagent::answers_graph(model, questions, evidence);

    let Ok(compiled) = tinyflows::compiler::compile(&graph) else {
        return Vec::new();
    };
    let Ok(outcome) = engine::run(&compiled, json!({}), capabilities).await else {
        return Vec::new();
    };

    questions
        .iter()
        .enumerate()
        .filter_map(|(index, question)| {
            let (value, _) = node_answer(&outcome.output, &subagent::node_id(index))?;

            Some(Answered {
                question: question.clone(),
                answer: value
                    .get("answer")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                confident: value
                    .get("confident")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            })
        })
        .collect()
}

/// How a lane wants its reviewers asked, beyond the one call.
///
/// Both extras are off when their field is absent, and the prompt is then
/// byte-identical to the plain one — which is what keeps the cassettes of a
/// deployment that has neither valid.
#[derive(Clone, Copy, Default)]
pub struct Asking<'a> {
    /// The model sub-agents answer questions on. `None` disables questions.
    pub subagent_model: Option<&'a str>,
    /// The tree a reviewer may look things up in. `None` disables lookups.
    pub tree: Option<&'a dyn TreeReader>,
    /// How much it may look up.
    pub lookup: Option<&'a LookupPolicy>,
}

impl<'a> Asking<'a> {
    /// The lookup policy in force, when lookups are possible at all.
    fn lookups(&self) -> Option<(&'a dyn TreeReader, &'a LookupPolicy)> {
        let tree = self.tree?;
        let policy = self.lookup?;
        (policy.enabled && policy.rounds > 0 && policy.per_round > 0).then_some((tree, policy))
    }
}

/// Ask every reviewer at once, and return one [`Answer`] each, in order.
///
/// Two kinds of follow-up, both bounded, both host-owned:
///
/// - **Lookups** — when a tree is available, a reviewer may end a turn with
///   reads and searches instead of a verdict; the host answers them and asks
///   again, up to `lookup.rounds` times. See [`crate::flows::lookup`].
/// - **Questions** — when `subagent_model` is set, a reviewer may end its
///   turn with questions; each is answered by a sub-agent and that reviewer is
///   asked once more. Exactly one such turn — see [`crate::flows::subagent`].
///
/// Lookups run first, so a sub-agent answering a question is handed the
/// evidence the reviewer already fetched rather than the diff alone. The last
/// turn always answers the plain schema: there is genuinely no turn after it.
///
/// Never returns `Err` for a single reviewer's failure — that is an [`Answer`]
/// carrying an `error`. `Err` is reserved for the graph itself not running,
/// which means no reviewer was asked at all.
pub async fn ask_all(
    llm: Arc<ModelCapability>,
    lane: LaneId,
    calls: &[Call],
    schema: &Value,
    asking: Asking<'_>,
) -> Result<Vec<Answer>> {
    if calls.is_empty() {
        return Ok(Vec::new());
    }

    let capabilities = crate::flows::caps::with_llm(llm, ChildGraphs::none());
    let lookups = asking.lookups();
    let subagent_model = asking.subagent_model;

    // The schema and the instruction travel together: a reviewer told it may
    // ask, answering a schema with no `questions` key, produces a refusal under
    // strict mode and a dropped key under `json_object`. Same for `lookups`.
    let schema_for = |may_lookup: bool, may_ask: bool| -> Value {
        let mut s = schema.clone();
        if may_lookup && let Some((_, policy)) = lookups {
            s = lookup::with_lookups(s, policy);
        }
        if may_ask {
            s = subagent::with_questions(s);
        }
        s
    };

    // The system prompt for a turn says exactly what that turn may do: a turn
    // that may look up is told so, a turn that may ask is told so, and the
    // settling turn is told neither — an instruction to ask, on a turn nothing
    // will answer, invites a question that is never answered.
    let system_for = |base: &str, may_lookup: bool, may_ask: bool| -> String {
        let mut system = base.to_string();
        if may_lookup && let Some((tree, policy)) = lookups {
            system.push_str(&lookup::instruction(&tree.describe(), policy));
        }
        if may_ask {
            system.push_str(subagent::ASK_INSTRUCTION);
        }
        if !may_lookup && !may_ask {
            system.push_str(crate::harness::prompt::SETTLE_INSTRUCTION);
        }
        system
    };

    let max_rounds = lookups.map_or(0, |(_, p)| p.rounds);
    let mut prompts: Vec<Call> = calls
        .iter()
        .cloned()
        .map(|mut call| {
            call.system = system_for(&call.system, max_rounds > 0, subagent_model.is_some());
            call
        })
        .collect();

    let mut answers = one_round(
        &capabilities,
        lane,
        &prompts,
        &schema_for(max_rounds > 0, subagent_model.is_some()),
    )
    .await?;

    // The lookup rounds. Each reviewer that asked gets its results appended
    // and is asked again; one that did not ask is settled and left alone. The
    // schema for the final permitted round offers no `lookups` key, so a
    // reviewer cannot ask for something no turn will answer.
    if let Some((tree, policy)) = lookups {
        let mut ledgers: Vec<lookup::Ledger> = (0..calls.len())
            .map(|_| lookup::Ledger::default())
            .collect();
        for round in 1..=max_rounds {
            let pending: Vec<(usize, Vec<Lookup>)> = answers
                .iter()
                .enumerate()
                .filter_map(|(index, answer)| {
                    let asked = lookup::read_lookups(answer.value.as_ref()?, policy);
                    (!asked.is_empty()).then_some((index, asked))
                })
                .collect();
            if pending.is_empty() {
                break;
            }
            let may_lookup_again = round < max_rounds;
            let round_schema = schema_for(may_lookup_again, subagent_model.is_some());
            for (index, asked) in pending {
                let gathered = ledgers[index].gather(tree, &asked, policy).await;
                if gathered.rendered.is_empty() {
                    continue;
                }
                prompts[index].prompt.push_str(&gathered.rendered);
                prompts[index].system = system_for(
                    &calls[index].system,
                    may_lookup_again,
                    subagent_model.is_some(),
                );
                tracing::debug!(
                    reviewer = %prompts[index].id,
                    round,
                    answered = gathered.answered,
                    chars = ledgers[index].chars(),
                    "a reviewer looked something up"
                );
                if let Ok(again) = one_round(
                    &capabilities,
                    lane,
                    std::slice::from_ref(&prompts[index]),
                    &round_schema,
                )
                .await
                    && let Some(settled) = again.into_iter().next()
                    && settled.value.is_some()
                {
                    answers[index] = settled;
                }
            }
        }
    }

    let Some(model) = subagent_model else {
        return Ok(answers);
    };

    // Which reviewers asked something, and what.
    let pending: Vec<(usize, Vec<String>)> = answers
        .iter()
        .enumerate()
        .filter_map(|(index, answer)| {
            let questions = read_questions(answer.value.as_ref()?);
            (!questions.is_empty()).then_some((index, questions))
        })
        .collect();

    for (index, questions) in pending {
        // The evidence as the reviewer last saw it: the diff plus whatever it
        // looked up. A sub-agent handed only the diff was answering "from the
        // repository" in name alone.
        let evidence = &prompts[index].prompt;
        let answered = answer_questions(&capabilities, model, &questions, evidence).await;

        // Nothing came back, so a second turn would be the same turn with the
        // same evidence — one more call that cannot say anything new.
        if answered.is_empty() {
            continue;
        }

        // The final turn answers the plain schema: there is genuinely no turn
        // after this one, so offering `questions` again would invite a question
        // nothing will ever answer.
        let mut again = prompts[index].clone();
        again.system = system_for(&calls[index].system, false, false);
        again.prompt.push_str(&subagent::render(&answered));

        if let Ok(round_two) =
            one_round(&capabilities, lane, std::slice::from_ref(&again), schema).await
            && let Some(settled) = round_two.into_iter().next()
            && settled.value.is_some()
        {
            answers[index] = settled;
        }
    }

    Ok(answers)
}

#[cfg(test)]
#[path = "runner_test.rs"]
mod tests;
