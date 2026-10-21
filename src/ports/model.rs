//! The model port.
//!
//! Lanes never talk to a provider SDK. They describe what they want — messages,
//! a JSON schema the answer must satisfy, a token ceiling — and get back either
//! a parsed value or an error. That keeps the harness (and its HTTP client) out
//! of the default build, and it makes every lane testable against a canned
//! response.
//!
//! Structured output is not optional. A lane that parses prose is a lane that
//! silently misbehaves when a model phrases something differently.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::Result;
use crate::ports::model_execution::ModelExecutionControl;

/// Observe one settled model call without exposing its request or response.
pub(crate) trait ModelCallObserver: Send + Sync {
    /// Record safe accounting and elapsed time for one provider call.
    fn completed(&self, usage: Usage, succeeded: bool, refused: bool, latency: Duration);

    /// Correlate physical-call telemetry to its review without retaining content.
    #[cfg(feature = "harness")]
    fn correlation(&self) -> ModelCallContext {
        ModelCallContext::default()
    }
}

#[cfg(feature = "harness")]
#[derive(Clone, Debug, Default)]
pub(crate) struct ModelCallContext {
    pub(crate) repo: String,
    pub(crate) pr: u64,
    pub(crate) head_sha: String,
    pub(crate) mode: String,
}

tokio::task_local! {
    static MODEL_CALL_OBSERVER: Arc<dyn ModelCallObserver>;
}

/// Run model work with an observer visible to physical model adapters.
pub(crate) async fn with_model_call_observer<F: std::future::Future>(
    observer: Arc<dyn ModelCallObserver>,
    future: F,
) -> F::Output {
    MODEL_CALL_OBSERVER.scope(observer, future).await
}

/// Clone the active observer for model work moved into a spawned agent task.
#[allow(dead_code)] // The gateway's spawned agent bridge consumes this when the harness observer lands.
pub(crate) fn current_model_call_observer() -> Option<Arc<dyn ModelCallObserver>> {
    MODEL_CALL_OBSERVER.try_with(Arc::clone).ok()
}

/// One message in a model conversation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    /// Who is speaking.
    pub role: Role,
    /// What they said.
    pub content: String,
    /// Images shown alongside the text, as URLs the provider fetches.
    ///
    /// Only ever populated on a [`Role::User`] message: the OpenAI-compatible
    /// wire format has no image part on a system or assistant message. Empty for every lane —
    /// a review reads a diff, not a picture — and non-empty only for the UI
    /// preview's captions, which look at the screenshots they describe.
    pub images: Vec<String>,
}

impl Message {
    /// A system message: the lane's instructions.
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: Role::System,
            content: content.into(),
            images: vec![],
        }
    }

    /// A user message: the evidence.
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: content.into(),
            images: vec![],
        }
    }

    /// A user message with images attached.
    ///
    /// The only constructor that sets [`images`](Self::images), and it exists
    /// for the user role alone — see the field's doc for why.
    pub fn user_with_images(content: impl Into<String>, images: Vec<String>) -> Self {
        Self {
            role: Role::User,
            content: content.into(),
            images,
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
    /// Optional per-request reasoning effort override.
    ///
    /// `None` preserves the gateway's configured global setting. A caller that
    /// has a role-specific policy may provide an explicit setting; adapters
    /// carry it through every physical retry and fallback for this request.
    pub reasoning_effort: Option<String>,
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
///
/// Serializable because it is carried in the proposal `apply` publishes from,
/// and `serde(default)` throughout so a proposal written before a counter
/// existed still loads.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Usage {
    /// Prompt tokens.
    pub input_tokens: u64,
    /// Generated tokens.
    pub output_tokens: u64,
    /// Tokens spent reasoning, as reported by the provider.
    pub reasoning_tokens: u64,
    /// Physical provider dispatches represented by this accounting record.
    /// Zero means the model adapter does not expose per-dispatch counts.
    #[serde(default)]
    pub calls_attempted: u64,
    /// Dispatches that returned a provider response, even if later validation
    /// rejected its contents.
    #[serde(default)]
    pub calls_succeeded: u64,
    /// Requests refused before a provider response, including admission refusals.
    #[serde(default)]
    pub calls_refused: u64,
    /// Dispatches that failed without a refusal.
    #[serde(default)]
    pub calls_errored: u64,
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
        self.reasoning_tokens += other.reasoning_tokens;
        self.calls_attempted += other.calls_attempted;
        self.calls_succeeded += other.calls_succeeded;
        self.calls_refused += other.calls_refused;
        self.calls_errored += other.calls_errored;
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

    /// Whether `complete` reports each physical provider dispatch through the
    /// task-scoped [`ModelCallObserver`]. Adapters without a physical observer
    /// keep the legacy one-result accounting fallback.
    fn complete_reports_physical_calls(&self) -> bool {
        false
    }

    /// Review against a borrowed read-only repository. Offline models retain
    /// their completion behavior; live adapters may supply bounded tools.
    /// Implementations must enforce the lookup policy and redact repository
    /// data before it enters any provider request.
    async fn review(
        &self,
        request: ModelRequest,
        _tree: &dyn crate::ports::tree::TreeReader,
        _policy: &crate::config::types::LookupPolicy,
    ) -> Result<ModelResponse> {
        self.complete(request).await
    }

    /// Whether `review` reports each physical provider dispatch. This is
    /// separate from `complete` because agentic adapters may install physical
    /// observation only when their scoped budget middleware is active.
    fn review_reports_physical_calls(&self) -> bool {
        self.complete_reports_physical_calls()
    }

    /// Whether a controlled model reports each raw physical completion to its
    /// review execution scope before applying any estimated price.
    ///
    /// This is separate from timing telemetry: adapters may report elapsed
    /// calls without retaining provider-reported usage. A model returning
    /// `true` must emit exactly one terminal callback per committed physical
    /// completion through the bound [`ModelExecutionControl`].
    fn reports_raw_completion_usage(&self) -> bool {
        false
    }

    /// Create a fresh shared budget scope for a lane, when supported.
    /// Replaces any prior scope rather than nesting ledgers. Offline models
    /// need no provider budget and keep returning `None`.
    fn scoped_budget(&self, _budget_usd: f64) -> Option<Arc<dyn Model>> {
        None
    }

    /// Bind a shared monetary budget to one review, when supported.
    ///
    /// Lane scopes created from the returned model must be children of this
    /// review budget, so their local caps also consume the shared review
    /// allowance. Clones derived through `scoped_budget` or
    /// `with_execution_control` must retain the same review root rather than
    /// creating or resetting one. This neutral hook accepts a dollar limit
    /// without exposing a provider or Embed budget type. The default keeps
    /// legacy models source-compatible; `None` means the adapter cannot bind a
    /// review-wide monetary ledger.
    fn with_review_budget(&self, _limit_usd: f64) -> Option<Arc<dyn Model>> {
        None
    }

    /// Return a model clone that preserves one review-wide dispatch control.
    ///
    /// The default keeps existing offline and custom models source-compatible.
    /// Returning `None` means this adapter cannot enforce a physical-call
    /// ceiling; normal-mode scheduling may still cancel its logical work, but
    /// a large-mode caller must fail closed before paid model work.
    fn with_execution_control(
        &self,
        _control: Arc<dyn ModelExecutionControl>,
    ) -> Option<Arc<dyn Model>> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opaque_models_keep_the_default_review_budget_binding_absent() {
        let model = crate::harness::mock::MockModel::new();

        assert!(model.with_review_budget(1.0).is_none());
    }

    #[test]
    fn usage_accumulates_across_calls() {
        let mut total = Usage::default();
        total.add(Usage {
            input_tokens: 100,
            output_tokens: 10,
            reasoning_tokens: 0,
            calls_attempted: 3,
            calls_succeeded: 1,
            calls_refused: 1,
            calls_errored: 1,
            cached_tokens: 80,
            embed_tokens: 0,
            cost_usd: 0.01,
        });
        total.add(Usage {
            input_tokens: 50,
            output_tokens: 5,
            reasoning_tokens: 0,
            calls_attempted: 1,
            calls_succeeded: 1,
            calls_refused: 0,
            calls_errored: 0,
            cached_tokens: 0,
            embed_tokens: 400,
            cost_usd: 0.02,
        });

        assert_eq!(total.input_tokens, 150);
        assert_eq!(total.output_tokens, 15);
        assert_eq!(total.cached_tokens, 80);
        assert_eq!(total.calls_attempted, 4);
        assert_eq!(total.calls_succeeded, 2);
        assert_eq!(total.calls_refused, 1);
        assert_eq!(total.calls_errored, 1);
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
