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

/// A fresh monetary ledger. Invalid limits admit no paid work.
pub(crate) fn ledger(limit_usd: f64) -> Budget {
    let micros = if limit_usd.is_finite() && limit_usd > 0.0 {
        (limit_usd * 1_000_000.0).floor() as u64
    } else {
        0
    };
    Budget::new(SpendLimits {
        tokens: None,
        cost_micros: Some(micros),
    })
}

/// Reserve against a configured alias bound or the public model rate table,
/// with additional bounded tool input. Invalid configured rates never fall
/// back to an estimate.
pub(crate) fn call(
    ledger: &Budget,
    request: &CompletionRequest,
    extra_input_bytes: u64,
    configured: &std::collections::BTreeMap<String, crate::config::types::BudgetPriceBound>,
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
    let input_tokens = serde_json::to_vec(request)
        .map_err(|error| Error::Model(error.to_string()))?
        .len() as u64;
    // Chat wire framing and schemas are additional input, not free metadata.
    let input_tokens = input_tokens
        .saturating_mul(2)
        .saturating_add(extra_input_bytes)
        .saturating_add(4096);
    let output_tokens = request
        .max_tokens
        .filter(|cap| *cap > 0)
        .ok_or_else(|| Error::Model("budgeted inference requires an output cap".into()))?;
    let rate = price.input.max(price.cached);
    let cost_micros =
        (input_tokens as f64 * rate + f64::from(output_tokens) * price.output).ceil() as u64;
    Ok(ModelBudget {
        ledger: ledger.clone(),
        call: CallBudget {
            input_tokens,
            output_tokens,
            cost_micros,
        },
    }
    .wait_for_capacity())
}

#[cfg(test)]
#[path = "budget_test.rs"]
mod tests;
