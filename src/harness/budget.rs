//! Physical-call reservations for the optional OpenHuman gateway adapter.
//!
//! A ledger belongs to one lane/review, while call bounds include serialized
//! prompts, schemas and tool context. Unknown route prices are refused before
//! inference. Operators must verify the rate table bounds their gateway
//! prices; local admission cannot constrain a provider's eventual bill.
use crate::error::{Error, Result};
use crate::harness::pricing;
use openhuman_embed::budget::{Budget, CallBudget, ModelBudget, SpendLimits};
use openhuman_embed::complete::CompletionRequest;
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

/// Bounded successful and billed output observations, partitioned by lane
/// schema and resolved route. Clones share observations for one model adapter.
type OutputKey = (String, String);
type OutputSamples = HashMap<OutputKey, VecDeque<u32>>;

#[derive(Debug, Clone, Default)]
pub(crate) struct OutputHistory(Arc<Mutex<OutputSamples>>);

impl OutputHistory {
    const WINDOW: usize = 32;

    /// Record a physical attempt's observed output, bounded by its provider cap.
    pub(crate) fn record(&self, lane: &str, model: &str, tokens: u64, cap: u32) {
        if cap == 0 {
            return;
        }
        let tokens = tokens.min(u64::from(cap)) as u32;
        let mut history = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let observations = history
            .entry((lane.to_owned(), model.to_owned()))
            .or_default();
        if observations.len() == Self::WINDOW {
            observations.pop_front();
        }
        observations.push_back(tokens);
    }

    pub(crate) fn p90(&self, lane: &str, model: &str, cap: u32) -> u32 {
        let history = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(observations) = history.get(&(lane.to_owned(), model.to_owned())) else {
            // A cold-start allowance of one eighth of the physical cap leaves
            // headroom for normal structured answers without pricing the whole
            // retry ladder as though every call consumes it.
            return (cap / 8).max(1).min(cap);
        };
        let mut ordered: Vec<u32> = observations.iter().copied().collect();
        ordered.sort_unstable();
        let index = ordered
            .len()
            .saturating_mul(9)
            .div_ceil(10)
            .saturating_sub(1);
        ordered[index].min(cap)
    }
}

/// A fresh monetary ledger. Invalid limits admit no paid work.
pub(crate) fn ledger(limit_usd: f64) -> Budget {
    Budget::new(SpendLimits {
        tokens: None,
        cost_micros: Some(limit_micros(limit_usd)),
    })
}

/// A lane ledger charged atomically to the review root and capped locally.
pub(crate) fn child_ledger(parent: &Budget, limit_usd: f64) -> Budget {
    parent.child(SpendLimits {
        tokens: None,
        cost_micros: Some(limit_micros(limit_usd)),
    })
}

/// Convert both root and lane limits with the same conservative rounding.
fn limit_micros(limit_usd: f64) -> u64 {
    if limit_usd.is_finite() && limit_usd > 0.0 {
        (limit_usd * 1_000_000.0).floor() as u64
    } else {
        0
    }
}

/// Reserve against a configured alias bound or the public model rate table,
/// with additional bounded tool input. Invalid configured rates never fall
/// back to an estimate.
#[cfg(test)]
pub(crate) fn call(
    ledger: &Budget,
    request: &CompletionRequest,
    extra_input_bytes: u64,
    configured: &std::collections::BTreeMap<String, crate::config::types::BudgetPriceBound>,
) -> Result<ModelBudget> {
    call_with_history(
        ledger,
        request,
        extra_input_bytes,
        configured,
        request
            .response_format
            .as_ref()
            .map_or("text", |format| match format {
                openhuman_embed::complete::ResponseFormat::JsonSchema { name, .. } => name,
                openhuman_embed::complete::ResponseFormat::JsonObject => "json_object",
                openhuman_embed::complete::ResponseFormat::Text => "text",
            }),
        &OutputHistory::default(),
    )
}

/// Reserve using output observations retained by this model adapter.
pub(crate) fn call_with_history(
    ledger: &Budget,
    request: &CompletionRequest,
    extra_input_bytes: u64,
    configured: &std::collections::BTreeMap<String, crate::config::types::BudgetPriceBound>,
    lane: &str,
    output_history: &OutputHistory,
) -> Result<ModelBudget> {
    let price = configured
        .get(&request.model)
        .map(|bound| pricing::Price {
            input: bound.input,
            cached: bound.cached,
            output: bound.output,
        })
        .or_else(|| pricing::price_of(&request.model))
        .ok_or_else(|| {
            Error::Model(format!(
                "{} has no verified price bound for budgeted inference",
                request.model
            ))
        })?;
    if !price.input.is_finite()
        || price.input < 0.0
        || !price.cached.is_finite()
        || price.cached < 0.0
        || !price.output.is_finite()
        || price.output <= 0.0
    {
        return Err(Error::Model(format!(
            "{} has an invalid budget price bound; rates must be finite and nonnegative, with positive output",
            request.model
        )));
    }
    let serialized_bytes = serde_json::to_vec(request)
        .map_err(|error| Error::Model(error.to_string()))?
        .len() as u64;
    // Keep Embed's independent serialized-size refusal bound conservative.
    let input_tokens = serialized_bytes
        .saturating_mul(2)
        .saturating_add(extra_input_bytes)
        .saturating_add(4096);
    let output_tokens = request
        .max_tokens
        .filter(|cap| *cap > 0)
        .ok_or_else(|| Error::Model("budgeted inference requires an output cap".into()))?;
    // The monetary estimate uses a token/byte ratio plus bounded room for
    // message framing not represented in the structured request. Schemas are
    // already included in `serialized_bytes`.
    let estimated_input_tokens = serialized_bytes
        .saturating_add(extra_input_bytes)
        .div_ceil(4)
        .saturating_add(1024);
    let expected_output_tokens = output_history.p90(lane, &request.model, output_tokens);
    let rate = price.input.max(price.cached);
    let cost_micros = (estimated_input_tokens as f64 * rate
        + f64::from(expected_output_tokens) * price.output)
        .ceil() as u64;
    tracing::info!(model = %request.model, lane, input_bytes = serialized_bytes,
        estimated_input_tokens, expected_output_tokens, output_cap_tokens = output_tokens,
        reserved_cost_usd = cost_micros as f64 / 1_000_000.0,
        "model budget reservation");
    Ok(ModelBudget {
        ledger: ledger.clone(),
        call: CallBudget {
            input_tokens,
            output_tokens,
            cost_micros,
        },
    })
}

#[cfg(test)]
#[path = "budget_test.rs"]
mod tests;
