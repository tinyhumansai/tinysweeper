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

use std::fmt;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::error::{Error, Result};
use crate::ports::model::{Model, ModelRequest, ModelResponse, Usage};
use crate::ports::model_execution::{ModelExecutionControl, ModelExecutionRefusal};

/// A model that answers from a queue of canned responses.
#[derive(Clone, Default)]
pub struct MockModel {
    responses: Arc<Mutex<Vec<Result<Value>>>>,
    requests: Arc<Mutex<Vec<ModelRequest>>>,
    usage: Usage,
    answers_as: Option<String>,
    /// When set, the panel's own rounds are answered from their `schema_name`
    /// rather than from the queue — see [`MockModel::panel`].
    panel: Option<Arc<PanelAnswers>>,
    /// Optional review-wide guard checked before this mock records a request.
    execution_control: Option<Arc<dyn ModelExecutionControl>>,
}

impl fmt::Debug for MockModel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MockModel")
            .field("responses", &self.responses)
            .field("requests", &self.requests)
            .field("usage", &self.usage)
            .field("answers_as", &self.answers_as)
            .field("panel", &self.panel)
            .field("execution_control_bound", &self.execution_control.is_some())
            .finish()
    }
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
            execution_control: None,
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

    /// Queue a failure after a provider reports paid usage.
    pub fn then_error_with_usage(self, message: &str, usage: Usage) -> Self {
        self.responses
            .lock()
            .expect("mock model lock")
            .push(Err(Error::Model(message.to_string()).with_usage(usage)));
        self
    }

    /// Queue a typed host execution refusal at the model boundary.
    #[cfg(test)]
    pub(crate) fn then_execution_refusal(self, refusal: ModelExecutionRefusal) -> Self {
        self.responses
            .lock()
            .expect("mock model lock")
            .push(Err(execution_refusal_error(refusal)));
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
        let permit = match &self.execution_control {
            Some(control) => Some(control.acquire().await.map_err(execution_refusal_error)?),
            None => None,
        };
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
                // A lane stage (relocation, falsification). A queued response
                // still wins — that is how a test says what the stage returns —
                // but an *empty* queue must not become a provider error. A
                // panel makes several times as many calls as the single one
                // these tests were written against, and a test that queues one
                // relocation should not have to know how many. The fallbacks
                // are the inert answer for each stage: relocate nothing,
                // falsify nothing.
                self.responses
                    .lock()
                    .expect("mock model lock")
                    .is_empty()
                    .then(|| {
                        if request.schema_name.contains("falsify") {
                            json!({ "incorrect": [] })
                        } else {
                            json!({})
                        }
                    })
            };

            if let Some(value) = canned {
                if let Some(permit) = &permit {
                    permit.mark_dispatched().map_err(execution_refusal_error)?;
                }
                self.requests.lock().expect("mock model lock").push(request);
                return Ok(ModelResponse {
                    value,
                    model,
                    usage: self.usage,
                });
            }
        }

        if let Some(permit) = &permit {
            permit.mark_dispatched().map_err(execution_refusal_error)?;
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

    fn with_execution_control(
        &self,
        control: Arc<dyn ModelExecutionControl>,
    ) -> Option<Arc<dyn Model>> {
        let mut scoped = self.clone();
        scoped.execution_control = Some(control);
        Some(Arc::new(scoped))
    }
}

fn execution_refusal_error(refusal: ModelExecutionRefusal) -> Error {
    Error::ModelExecutionRefused {
        refusal,
        usage: None,
        accounting_complete: true,
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
            reasoning_effort: None,
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

    #[tokio::test]
    async fn mock_execution_ceiling_refuses_before_recording_or_consuming_the_next_answer() {
        use super::execution_test_support::execution_control;
        use crate::ports::model_execution::ModelExecutionRefusal;

        let mock = MockModel::new()
            .then(json!({"summary":"first"}))
            .then(json!({"summary":"still queued"}));
        let (_, control) = execution_control(1);
        let controlled = mock
            .with_execution_control(control)
            .unwrap_or_else(|| Arc::new(mock.clone()));

        assert_eq!(
            controlled.complete(request()).await.unwrap().value["summary"],
            "first"
        );
        let tree = crate::ports::tree::MockTree::from_files([("src/lib.rs", "fn example() {}")]);
        assert!(matches!(
            controlled
                .review(
                    request(),
                    &tree,
                    &crate::config::types::LookupPolicy::default(),
                )
                .await,
            Err(Error::ModelExecutionRefused {
                refusal: ModelExecutionRefusal::CallLimit,
                ..
            })
        ));
        assert_eq!(mock.calls(), 1, "refusal precedes request recording");

        let (_, replacement_control) = execution_control(1);
        let rebound = mock
            .with_execution_control(replacement_control)
            .unwrap_or_else(|| Arc::new(mock.clone()));
        assert_eq!(
            rebound.complete(request()).await.unwrap().value["summary"],
            "still queued",
            "refusal leaves the canned answer available"
        );
        assert_eq!(mock.calls(), 2);
    }

    #[tokio::test]
    async fn cancelled_mock_control_does_not_consume_the_first_canned_answer() {
        use super::execution_test_support::execution_control;
        use crate::ports::model_execution::ModelExecutionRefusal;

        let mock = MockModel::new().then(json!({"summary":"first"}));
        let (execution, control) = execution_control(1);
        execution.cancel();
        let controlled = mock
            .with_execution_control(control)
            .unwrap_or_else(|| Arc::new(mock.clone()));
        let tree = crate::ports::tree::MockTree::from_files([("src/lib.rs", "fn example() {}")]);

        assert!(matches!(
            controlled
                .review(
                    request(),
                    &tree,
                    &crate::config::types::LookupPolicy::default(),
                )
                .await,
            Err(Error::ModelExecutionRefused {
                refusal: ModelExecutionRefusal::Cancelled,
                ..
            })
        ));
        assert_eq!(mock.calls(), 0);

        let (_, replacement_control) = execution_control(1);
        let rebound = mock
            .with_execution_control(replacement_control)
            .unwrap_or_else(|| Arc::new(mock.clone()));
        assert_eq!(
            rebound.complete(request()).await.unwrap().value["summary"],
            "first"
        );
        assert_eq!(mock.calls(), 1);
    }
}

/// Shared physical-dispatch fixture for decorator regression tests.
#[cfg(test)]
pub(crate) mod execution_test_support {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use async_trait::async_trait;

    use crate::app::review_execution::{ReviewExecution, ReviewExecutionOptions};
    use crate::error::{Error, Result};
    use crate::lanes::large_pr::ReviewMode;
    use crate::ports::model::{Model, ModelRequest, ModelResponse};
    use crate::ports::model_execution::{
        ModelExecutionControl, ModelExecutionPermit, ModelExecutionRefusal,
    };

    use super::MockModel;

    #[derive(Clone)]
    pub(crate) struct DispatchProbe {
        model: MockModel,
        control: Option<Arc<dyn ModelExecutionControl>>,
        review_remaining: Option<Arc<AtomicUsize>>,
        lane_remaining: Option<Arc<AtomicUsize>>,
    }

    impl DispatchProbe {
        pub(crate) fn new(model: MockModel) -> Self {
            Self {
                model,
                control: None,
                review_remaining: None,
                lane_remaining: None,
            }
        }

        fn with_control(&self, control: Arc<dyn ModelExecutionControl>) -> Self {
            Self {
                model: self.model.clone(),
                control: Some(control),
                review_remaining: self.review_remaining.clone(),
                lane_remaining: self.lane_remaining.clone(),
            }
        }

        fn consume(limit: &Option<Arc<AtomicUsize>>) -> bool {
            limit.as_ref().is_none_or(|remaining| {
                remaining
                    .try_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                        count.checked_sub(1)
                    })
                    .is_ok()
            })
        }

        fn refusal(refusal: ModelExecutionRefusal) -> Error {
            Error::ModelExecutionRefused {
                refusal,
                usage: None,
                accounting_complete: true,
            }
        }
    }

    #[async_trait]
    impl Model for DispatchProbe {
        async fn complete(&self, request: ModelRequest) -> Result<ModelResponse> {
            let _permit: Option<Box<dyn ModelExecutionPermit>> = match &self.control {
                Some(control) => Some(control.acquire().await.map_err(Self::refusal)?),
                None => None,
            };
            if !Self::consume(&self.review_remaining) || !Self::consume(&self.lane_remaining) {
                return Err(Error::Budget {
                    spent: 1.0,
                    limit: 1.0,
                    usage: None,
                });
            }
            if let Some(permit) = &_permit {
                permit.mark_dispatched().map_err(Self::refusal)?;
            }
            let response = self.model.complete(request).await?;
            if let Some(observer) = crate::ports::model::current_model_call_observer() {
                observer.completed(response.usage, true, false, Duration::ZERO);
            }
            Ok(response)
        }

        fn complete_reports_physical_calls(&self) -> bool {
            true
        }

        fn scoped_budget(&self, limit_usd: f64) -> Option<Arc<dyn Model>> {
            Some(Arc::new(Self {
                model: self.model.clone(),
                control: self.control.clone(),
                review_remaining: self.review_remaining.clone(),
                lane_remaining: Some(Arc::new(AtomicUsize::new(
                    limit_usd.floor().max(0.0) as usize
                ))),
            }))
        }

        fn with_review_budget(&self, limit_usd: f64) -> Option<Arc<dyn Model>> {
            Some(Arc::new(Self {
                model: self.model.clone(),
                control: self.control.clone(),
                review_remaining: Some(Arc::new(AtomicUsize::new(
                    limit_usd.floor().max(0.0) as usize
                ))),
                lane_remaining: self.lane_remaining.clone(),
            }))
        }

        fn with_execution_control(
            &self,
            control: Arc<dyn ModelExecutionControl>,
        ) -> Option<Arc<dyn Model>> {
            Some(Arc::new(self.with_control(control)))
        }
    }

    pub(crate) fn execution_control(
        max_calls: usize,
    ) -> (Arc<ReviewExecution>, Arc<dyn ModelExecutionControl>) {
        let started_at = tokio::time::Instant::now();
        let execution = Arc::new(ReviewExecution::new(ReviewExecutionOptions {
            started_at,
            hard_deadline: started_at + Duration::from_secs(100),
            mode: ReviewMode::Large,
            max_calls: Some(max_calls),
            soft_deadline_percent: 80,
            drain_seconds: 30,
        }));
        let control: Arc<dyn ModelExecutionControl> = execution.clone();
        (execution, control)
    }
}
