//! Safe accounting retained when an agentic reviewer refuses its answer.

use crate::error::Error;
use crate::ports::model::Usage;
use openhuman_embed::CoreError;
use openhuman_embed::budget::Budget;

/// Provider counters are untrusted; aggregation must not wrap paid usage.
pub(crate) fn accumulate_usage(total: &mut Usage, usage: Usage) {
    total.input_tokens = total.input_tokens.saturating_add(usage.input_tokens);
    total.output_tokens = total.output_tokens.saturating_add(usage.output_tokens);
    total.cached_tokens = total.cached_tokens.saturating_add(usage.cached_tokens);
    total.embed_tokens = total.embed_tokens.saturating_add(usage.embed_tokens);
    total.cost_usd = (total.cost_usd + usage.cost_usd).min(f64::MAX);
}

/// An internal failure carries paid usage without exposing rejected content.
#[derive(Debug)]
pub(crate) struct ReviewFailure {
    pub(crate) error: Error,
    pub(crate) usage: Option<Box<Usage>>,
}

impl From<Error> for ReviewFailure {
    fn from(error: Error) -> Self {
        Self { error, usage: None }
    }
}

impl ReviewFailure {
    pub(crate) fn into_error(self) -> Error {
        match self.usage {
            Some(usage) => self.error.with_usage(*usage),
            None => self.error,
        }
    }

    pub(crate) fn core(
        error: CoreError,
        requested_model: &str,
        ledger: Option<&Budget>,
        estimate: Usage,
    ) -> Self {
        let mut failure = Self::unknown(
            Error::Model("agentic reviewer failed".into()),
            ledger,
            estimate,
        );
        if matches!(
            error,
            CoreError::Unavailable { .. } | CoreError::InvalidRoute { .. }
        ) {
            failure.usage = None;
            return failure;
        }
        if let CoreError::StructuredOutput {
            failure: structured,
            ..
        } = error
        {
            if let Some(usage) = structured.usage {
                let model = structured
                    .answered_model
                    .as_deref()
                    .unwrap_or(requested_model);
                let cost = usage
                    .cost_usd
                    .filter(|cost| cost.is_finite() && *cost >= 0.0)
                    .or_else(|| {
                        ledger.and_then(|ledger| {
                            let cost = ledger.snapshot().spent.cost_micros;
                            (cost > 0).then_some(cost as f64 / 1_000_000.0)
                        })
                    })
                    .unwrap_or_else(|| {
                        if usage.input_tokens == 0 && usage.output_tokens == 0 {
                            estimate.cost_usd
                        } else {
                            estimate
                                .cost_usd
                                .max(crate::harness::pricing::completion_cost(
                                    model,
                                    usage.input_tokens,
                                    usage.cached_tokens,
                                    usage.output_tokens,
                                ))
                        }
                    });
                failure.usage = Some(Box::new(Usage {
                    input_tokens: usage.input_tokens,
                    output_tokens: usage.output_tokens,
                    cached_tokens: usage.cached_tokens,
                    cost_usd: cost,
                    embed_tokens: 0,
                }));
            } else if structured.reason
                == openhuman_embed::structured::StructuredFailureReason::InvalidSchema
            {
                failure.usage = None;
            }
        }
        failure
    }

    pub(crate) fn unknown(error: Error, ledger: Option<&Budget>, estimate: Usage) -> Self {
        // This is an isolated attempt ledger, not the shared review ledger:
        // concurrent reviewers must never be counted as this failed attempt.
        let usage = match ledger {
            Some(ledger) => {
                let spent = ledger.snapshot().spent;
                // The ledger records combined tokens; without a provider
                // breakdown, retain them as an input estimate, not lost spend.
                (spent.tokens != 0 || spent.cost_micros != 0).then(|| {
                    Box::new(Usage {
                        input_tokens: spent.tokens,
                        output_tokens: 0,
                        cached_tokens: 0,
                        cost_usd: spent.cost_micros as f64 / 1_000_000.0,
                        embed_tokens: 0,
                    })
                })
            }
            None => Some(Box::new(estimate)),
        };
        Self { error, usage }
    }
}
