//! An offline model that returns canned structured responses.
//!
//! Always compiled. Every lane test runs against this, which is what keeps the
//! test suite hermetic and fast — and, more importantly, what makes the
//! noise-control rules testable: given a model that says exactly *this*, the
//! filtering must post exactly *that*.
//!
//! It also records the requests it received, so a test can assert on prompt
//! structure — including the cache-prefix discipline, which is otherwise
//! invisible until a bill arrives.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::error::{Error, Result};
use crate::ports::model::{Model, ModelRequest, ModelResponse, Usage};

/// A model that answers from a queue of canned responses.
#[derive(Debug, Clone, Default)]
pub struct MockModel {
    responses: Arc<Mutex<Vec<Result<Value>>>>,
    requests: Arc<Mutex<Vec<ModelRequest>>>,
    usage: Usage,
    answers_as: Option<String>,
    /// When set, the panel's own rounds are answered from their `schema_name`
    /// rather than from the queue — see [`MockModel::panel`].
    panel: Option<Arc<PanelAnswers>>,
}

/// Schema names that belong to a lane's own stages rather than to a panel
/// round, and so must reach the response queue.
const PANEL_STAGES: &[&str] = &["relocate", "falsify"];

/// What a panel-aware mock answers each round with.
#[derive(Debug)]
struct PanelAnswers {
    /// The lane response every proposing lens gives, when no `matching` entry
    /// applies.
    propose: Value,
    /// Per-file responses, chosen by a substring of the prompt.
    ///
    /// A per-file lane runs one panel per file, and some tests are precisely
    /// about one file behaving differently from another. With one canned
    /// response that is not expressible, and with a queue it depends on the
    /// panel's internal call order.
    matching: Vec<(String, Value)>,
    /// Whether every verifier confirms.
    verdict: bool,
}

impl MockModel {
    /// A model with no responses queued. Calling it is an error.
    pub fn new() -> Self {
        Self::default()
    }

    /// A model that answers every call with `value`.
    pub fn always(value: Value) -> Self {
        Self {
            responses: Arc::new(Mutex::new(vec![])),
            requests: Arc::new(Mutex::new(vec![])),
            usage: Usage::default(),
            answers_as: None,
            panel: None,
        }
        .repeating(value)
    }

    /// A model that reports no findings.
    pub fn silent() -> Self {
        Self::always(json!({"summary": "Nothing to report.", "findings": []}))
    }

    /// A model that answers a whole panel from one lane response.
    ///
    /// A panel is three rounds of differently-shaped calls, so a queue of
    /// canned values makes a golden test depend on call *order* — which is the
    /// panel's internal business and changes whenever a lens is added. This
    /// dispatches on the schema each round asks for instead: every proposing
    /// lens gets `response`, every verifier confirms, and every sub-agent
    /// answers unhelpfully (a golden test asserting on filtering should not
    /// also be asserting on what sub-agents said).
    ///
    /// The effect is that a golden test still reads "given a model that says
    /// exactly this, the lane must post exactly that" — which is the property
    /// these tests exist to pin.
    pub fn panel(response: Value) -> Self {
        Self {
            panel: Some(Arc::new(PanelAnswers {
                propose: response,
                matching: Vec::new(),
                verdict: true,
            })),
            ..Self::default()
        }
    }

    /// A panel that answers each file differently.
    ///
    /// The first entry whose key appears anywhere in the request's messages
    /// wins; `fallback` answers everything else. Written against the prompt
    /// rather than call order because the panel's ordering is its own business.
    pub fn panel_matching(matching: &[(&str, Value)], fallback: Value) -> Self {
        Self {
            panel: Some(Arc::new(PanelAnswers {
                propose: fallback,
                matching: matching
                    .iter()
                    .map(|(key, value)| ((*key).to_string(), value.clone()))
                    .collect(),
                verdict: true,
            })),
            ..Self::default()
        }
    }

    /// A panel whose verifiers refute everything the lenses propose.
    ///
    /// The other half of the contract: proving a lane publishes nothing when
    /// the verify round rejects it.
    pub fn panel_refuting(response: Value) -> Self {
        Self {
            panel: Some(Arc::new(PanelAnswers {
                propose: response,
                matching: Vec::new(),
                verdict: false,
            })),
            ..Self::default()
        }
    }

    /// Queue one response. Responses are consumed in order.
    pub fn then(self, value: Value) -> Self {
        self.responses
            .lock()
            .expect("mock model lock")
            .push(Ok(value));
        self
    }

    /// Queue a failure.
    pub fn then_error(self, message: &str) -> Self {
        self.responses
            .lock()
            .expect("mock model lock")
            .push(Err(Error::Model(message.to_string())));
        self
    }

    /// Answer as `model` whatever model was asked for.
    ///
    /// This is what a provider fallback looks like from the port: the request
    /// named one model and a different one came back. Tests use it to prove the
    /// cost line reports what answered rather than what was configured.
    pub fn answering_as(mut self, model: impl Into<String>) -> Self {
        self.answers_as = Some(model.into());
        self
    }

    /// Report `usage` on every call, so budget enforcement is testable.
    pub fn with_usage(mut self, usage: Usage) -> Self {
        self.usage = usage;
        self
    }

    /// Every request the model received, in order.
    pub fn requests(&self) -> Vec<ModelRequest> {
        self.requests.lock().expect("mock model lock").clone()
    }

    /// How many times it was called.
    pub fn calls(&self) -> usize {
        self.requests.lock().expect("mock model lock").len()
    }

    /// The concatenated text of the last request, for prompt assertions.
    pub fn last_prompt(&self) -> Option<String> {
        self.requests().last().map(|request| {
            request
                .messages
                .iter()
                .map(|m| m.content.as_str())
                .collect::<Vec<_>>()
                .join("\n")
        })
    }

    fn repeating(self, value: Value) -> Self {
        // A single response reused for every call. Enough for the common test,
        // where the point is the filtering rather than the sequence.
        for _ in 0..64 {
            self.responses
                .lock()
                .expect("mock model lock")
                .push(Ok(value.clone()));
        }
        self
    }
}

#[async_trait]
impl Model for MockModel {
    async fn complete(&self, request: ModelRequest) -> Result<ModelResponse> {
        let model = self
            .answers_as
            .clone()
            .unwrap_or_else(|| request.model.clone());
        // A panel-aware mock answers the panel's own rounds from the round it
        // recognises, and lets every other call fall through to the queue.
        //
        // The fall-through is what keeps the existing golden tests meaningful:
        // relocation (`tinysweeper_relocate`) and falsification
        // (`tinysweeper_falsify`) are not panel rounds, they are separate
        // stages a lane drives itself, and a test that queues a canned
        // relocation still needs that value to reach the positioner.
        if let Some(answers) = self.panel.clone() {
            let canned = if request.schema_name.ends_with("_verify") {
                Some(json!({ "real": answers.verdict, "why": "canned" }))
            } else if request.schema_name.ends_with("_subagent_answer") {
                Some(json!({ "answer": "The evidence does not say.", "confident": false }))
            } else if PANEL_STAGES
                .iter()
                .all(|stage| !request.schema_name.contains(stage))
            {
                let prompt = request
                    .messages
                    .iter()
                    .map(|m| m.content.as_str())
                    .collect::<Vec<_>>()
                    .join("\n");

                Some(
                    answers
                        .matching
                        .iter()
                        .find(|(key, _)| prompt.contains(key.as_str()))
                        .map_or_else(|| answers.propose.clone(), |(_, value)| value.clone()),
                )
            } else {
                None
            };

            if let Some(value) = canned {
                self.requests.lock().expect("mock model lock").push(request);
                return Ok(ModelResponse {
                    value,
                    model,
                    usage: self.usage,
                });
            }
        }

        self.requests.lock().expect("mock model lock").push(request);

        let queued = {
            let mut responses = self.responses.lock().expect("mock model lock");
            if responses.is_empty() {
                None
            } else {
                Some(responses.remove(0))
            }
        };

        match queued {
            Some(Ok(value)) => Ok(ModelResponse {
                value,
                model,
                usage: self.usage,
            }),
            Some(Err(err)) => Err(err),
            None => Err(Error::Model(
                "MockModel ran out of queued responses; queue one with `.then(…)`".into(),
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ports::model::Message;

    fn request() -> ModelRequest {
        ModelRequest {
            model: "mock".into(),
            messages: vec![Message::system("rules"), Message::user("diff")],
            schema: json!({}),
            schema_name: "lane".into(),
            max_tokens: 100,
        }
    }

    #[tokio::test]
    async fn responses_are_consumed_in_order() {
        let model = MockModel::new()
            .then(json!({"summary": "first", "findings": []}))
            .then(json!({"summary": "second", "findings": []}));

        let a = model.complete(request()).await.expect("first");
        let b = model.complete(request()).await.expect("second");

        assert_eq!(a.value["summary"], "first");
        assert_eq!(b.value["summary"], "second");
        assert_eq!(model.calls(), 2);
    }

    #[tokio::test]
    async fn running_out_of_responses_is_an_error_not_a_silent_default() {
        let model = MockModel::new();
        let err = model.complete(request()).await.unwrap_err();
        assert!(
            err.to_string().contains("ran out of queued responses"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn a_queued_failure_surfaces_as_a_model_error() {
        let model = MockModel::new().then_error("upstream exploded");
        let err = model.complete(request()).await.unwrap_err();
        assert!(err.to_string().contains("upstream exploded"), "{err}");
    }

    #[tokio::test]
    async fn requests_are_recorded_for_prompt_assertions() {
        let model = MockModel::silent();
        model.complete(request()).await.expect("answers");

        let prompt = model.last_prompt().expect("recorded");
        assert!(prompt.contains("rules"));
        assert!(prompt.contains("diff"));
    }

    #[tokio::test]
    async fn usage_is_reported_so_budget_enforcement_is_testable() {
        let model = MockModel::silent().with_usage(Usage {
            input_tokens: 1000,
            output_tokens: 100,
            cached_tokens: 800,
            cost_usd: 0.01,
            ..Usage::default()
        });
        let response = model.complete(request()).await.expect("answers");
        assert_eq!(response.usage.cached_tokens, 800);
    }

    #[tokio::test]
    async fn a_fallback_answers_under_its_own_name() {
        let model = MockModel::silent().answering_as("vendor/fallback");
        let response = model.complete(request()).await.expect("answers");
        assert_eq!(response.model, "vendor/fallback");
        assert_eq!(
            model.requests()[0].model,
            "mock",
            "the request is unchanged"
        );
    }
}
