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

/// Ask every reviewer at once, and return one [`Answer`] each, in order.
///
/// Never returns `Err` for a single reviewer's failure — that is an [`Answer`]
/// carrying an `error`. `Err` is reserved for the graph itself not running,
/// which means no reviewer was asked at all.
pub async fn ask_all(
    llm: Arc<ModelCapability>,
    lane: LaneId,
    calls: &[Call],
    schema: &Value,
) -> Result<Vec<Answer>> {
    if calls.is_empty() {
        return Ok(Vec::new());
    }

    let graph = panel::council_graph(lane, calls, schema);
    let capabilities = crate::flows::caps::with_llm(llm, ChildGraphs::none());

    let compiled = tinyflows::compiler::compile(&graph)
        .map_err(|e| crate::error::Error::Model(format!("council graph did not compile: {e}")))?;

    let outcome = engine::run(&compiled, json!({}), &capabilities)
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

#[cfg(test)]
#[path = "runner_test.rs"]
mod tests;
