//! The model port.
//!
//! Lanes never talk to a provider SDK. They describe what they want — messages,
//! a JSON schema the answer must satisfy, a token ceiling — and get back either
//! a parsed value or an error. That keeps tinyagents (and its HTTP client) out
//! of the default build, and it makes every lane testable against a canned
//! response.
//!
//! Structured output is not optional. A lane that parses prose is a lane that
//! silently misbehaves when a model phrases something differently.

use async_trait::async_trait;
use serde_json::Value;

use crate::error::Result;

/// One message in a model conversation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    /// Who is speaking.
    pub role: Role,
    /// What they said.
    pub content: String,
}

impl Message {
    /// A system message: the lane's instructions.
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: Role::System,
            content: content.into(),
        }
    }

    /// A user message: the evidence.
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: content.into(),
        }
    }
}

/// Who authored a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// The lane's instructions.
    System,
    /// Evidence supplied to the model. Always untrusted input.
    User,
    /// The model's own prior turn.
    Assistant,
}

/// A request for one structured completion.
#[derive(Debug, Clone)]
pub struct ModelRequest {
    /// The model id to call, already resolved from tier to id.
    pub model: String,
    /// The conversation.
    pub messages: Vec<Message>,
    /// The JSON schema the response must satisfy.
    pub schema: Value,
    /// A name for the schema, surfaced to providers that want one.
    pub schema_name: String,
    /// Ceiling on generated tokens.
    pub max_tokens: u32,
}

/// What a model returned.
#[derive(Debug, Clone)]
pub struct ModelResponse {
    /// The parsed structured output.
    pub value: Value,
    /// The model that actually answered. Differs from the request when a
    /// fallback took over, which is worth reporting in the check summary.
    pub model: String,
    /// Token and cost accounting for this call.
    pub usage: Usage,
}

/// Token and cost accounting for one call.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Usage {
    /// Prompt tokens.
    pub input_tokens: u64,
    /// Generated tokens.
    pub output_tokens: u64,
    /// Prompt tokens served from the provider's cache. The difference between
    /// a cheap re-review and a ruinous one.
    pub cached_tokens: u64,
    /// Tokens sent to an embedding model.
    ///
    /// Counted apart from `input_tokens` because embeddings are priced on their
    /// own scale entirely and mixing them into the prompt total would make the
    /// cache hit rate — computed against `input_tokens` — quietly wrong. Once a
    /// repository is indexed this is the dominant line item, so it has to be
    /// visible rather than folded away.
    pub embed_tokens: u64,
    /// Cost in USD, when the provider reports it.
    pub cost_usd: f64,
}

impl Usage {
    /// Fold another call's usage into this one.
    pub fn add(&mut self, other: Usage) {
        self.input_tokens += other.input_tokens;
        self.output_tokens += other.output_tokens;
        self.cached_tokens += other.cached_tokens;
        self.embed_tokens += other.embed_tokens;
        self.cost_usd += other.cost_usd;
    }
}

/// Usage, plus the models that actually produced it.
///
/// The two travel together because reporting one without the other is how the
/// cost line came to lie: it named the model the config *asked* for while the
/// usage came from whichever model answered. A fallback is exactly the case
/// worth reporting — it is the moment the review got cheaper and worse — so the
/// model id is recorded at the point the response arrives and never re-derived
/// from config afterwards.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Spend {
    /// Tokens and dollars.
    pub usage: Usage,
    /// Every distinct model that answered, in first-seen order.
    pub models: Vec<String>,
}

impl Spend {
    /// What one response cost, attributed to the model that returned it.
    pub fn of(response: &ModelResponse) -> Self {
        let mut spend = Self::default();
        spend.record(&response.model, response.usage);
        spend
    }

    /// Attribute `usage` to `model`.
    pub fn record(&mut self, model: &str, usage: Usage) {
        self.usage.add(usage);
        self.note(model);
    }

    /// Note that `model` answered, without any usage of its own.
    pub fn note(&mut self, model: &str) {
        if !self.models.iter().any(|known| known == model) {
            self.models.push(model.to_string());
        }
    }

    /// Fold another spend into this one.
    pub fn merge(&mut self, other: Spend) {
        self.usage.add(other.usage);
        for model in other.models {
            self.note(&model);
        }
    }

    /// Dollars spent.
    pub fn cost_usd(&self) -> f64 {
        self.usage.cost_usd
    }
}

/// A model that answers with structured output.
#[async_trait]
pub trait Model: Send + Sync {
    /// Run one completion.
    async fn complete(&self, request: ModelRequest) -> Result<ModelResponse>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_accumulates_across_calls() {
        let mut total = Usage::default();
        total.add(Usage {
            input_tokens: 100,
            output_tokens: 10,
            cached_tokens: 80,
            embed_tokens: 0,
            cost_usd: 0.01,
        });
        total.add(Usage {
            input_tokens: 50,
            output_tokens: 5,
            cached_tokens: 0,
            embed_tokens: 400,
            cost_usd: 0.02,
        });

        assert_eq!(total.input_tokens, 150);
        assert_eq!(total.output_tokens, 15);
        assert_eq!(total.cached_tokens, 80);
        assert_eq!(total.embed_tokens, 400);
        assert!((total.cost_usd - 0.03).abs() < f64::EPSILON);
    }

    #[test]
    fn a_spend_is_attributed_to_the_model_that_answered() {
        let response = ModelResponse {
            value: serde_json::Value::Null,
            // A fallback answered; the configured model did not.
            model: "vendor/fallback".into(),
            usage: Usage {
                input_tokens: 10,
                cost_usd: 0.5,
                ..Usage::default()
            },
        };

        let spend = Spend::of(&response);
        assert_eq!(spend.models, vec!["vendor/fallback".to_string()]);
        assert!((spend.cost_usd() - 0.5).abs() < f64::EPSILON);
    }

    #[test]
    fn merging_sums_the_usage_and_keeps_each_model_once() {
        let mut spend = Spend::default();
        spend.record(
            "vendor/deep",
            Usage {
                input_tokens: 10,
                cost_usd: 0.1,
                ..Usage::default()
            },
        );

        let mut other = Spend::default();
        other.record(
            "vendor/scan",
            Usage {
                input_tokens: 5,
                cost_usd: 0.01,
                ..Usage::default()
            },
        );
        other.record(
            "vendor/deep",
            Usage {
                input_tokens: 1,
                ..Usage::default()
            },
        );

        spend.merge(other);
        assert_eq!(spend.usage.input_tokens, 16);
        assert!((spend.cost_usd() - 0.11).abs() < 1e-9);
        assert_eq!(
            spend.models,
            vec!["vendor/deep".to_string(), "vendor/scan".to_string()]
        );
    }
}
