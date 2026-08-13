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

use crate::config::types::LaneId;
use crate::error::Result;
use crate::flows::caps::{ChildGraphs, ModelCapability};
use crate::flows::panel::{self, Call};
use crate::flows::subagent::{self, Answered};
use crate::ports::model::Model;

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

/// Ask every reviewer at once, and return one [`Answer`] each, in order.
///
/// When `subagent_model` is set, a reviewer may end its turn with questions
/// rather than a guess; each is answered by a sub-agent and that reviewer is
/// asked once more with the answers in hand. Exactly one follow-up turn, and
/// only for reviewers that asked — see [`crate::flows::subagent`] for why the
/// depth bound is structural rather than a counter.
///
/// Never returns `Err` for a single reviewer's failure — that is an [`Answer`]
/// carrying an `error`. `Err` is reserved for the graph itself not running,
/// which means no reviewer was asked at all.
pub async fn ask_all(
    llm: Arc<ModelCapability>,
    lane: LaneId,
    calls: &[Call],
    schema: &Value,
    subagent_model: Option<&str>,
) -> Result<Vec<Answer>> {
    if calls.is_empty() {
        return Ok(Vec::new());
    }

    let capabilities = crate::flows::caps::with_llm(llm, ChildGraphs::none());

    // The schema and the instruction travel together: a reviewer told it may
    // ask, answering a schema with no `questions` key, produces a refusal under
    // strict mode and a dropped key under `json_object`.
    let asked = subagent_model.map(|_| subagent::with_questions(schema.clone()));
    let round_one: Vec<Call> = match subagent_model {
        Some(_) => calls
            .iter()
            .cloned()
            .map(|mut call| {
                call.system.push_str(subagent::ASK_INSTRUCTION);
                call
            })
            .collect(),
        None => calls.to_vec(),
    };

    let mut answers = one_round(
        &capabilities,
        lane,
        &round_one,
        asked.as_ref().unwrap_or(schema),
    )
    .await?;

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
        let evidence = &calls[index].prompt;
        let answered = answer_questions(&capabilities, model, &questions, evidence).await;

        // Nothing came back, so a second turn would be the same turn with the
        // same evidence — one more call that cannot say anything new.
        if answered.is_empty() {
            continue;
        }

        // The final turn answers the plain schema: there is genuinely no turn
        // after this one, so offering `questions` again would invite a question
        // nothing will ever answer.
        let mut again = calls[index].clone();
        again.prompt.push_str(&subagent::render(&answered));

        if let Ok(round_two) = one_round(&capabilities, lane, std::slice::from_ref(&again), schema).await
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
