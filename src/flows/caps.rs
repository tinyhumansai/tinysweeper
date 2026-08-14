//! The capability seam: how a tinyflows graph reaches this crate's ports.
//!
//! tinyflows is host-agnostic — every call that touches the outside world goes
//! through a trait the embedding application implements. That is exactly the
//! shape the security boundary in `AGENTS.md` wants, so the implementations
//! here are as much about what they *refuse* as what they do:
//!
//! | capability | wired to | why |
//! |---|---|---|
//! | `llm` | [`crate::ports::model::Model`] | the one path to a provider |
//! | `tools` | refused | a lane has no tools; a model that could call one could act |
//! | `http` | refused | the only network call a review makes is the model call |
//! | `code` | refused | contributor code is never executed |
//! | `shell` | absent | same, and absent is stronger than refusing |
//! | `state` | in-memory, per run | a lane is pure; nothing outlives the run |
//!
//! Refusing rather than omitting matters for `tools`, `http` and `code`: the
//! engine treats an absent optional capability as a run-time error already, but
//! these three are *required* fields, so something must be supplied. What is
//! supplied denies every call with an error naming the boundary, so a graph that
//! grows a `code` node fails on its first run with the reason rather than
//! quietly executing.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::{Value, json};
use tinyflows::caps::{
    Capabilities, CodeLanguage, CodeRunner, HttpClient, LlmProvider, StateStore, ToolInvoker,
    WorkflowResolver,
};
use tinyflows::error::{EngineError, Result as FlowResult};
use tinyflows::model::WorkflowGraph;

use crate::config::types::Models;
use crate::ports::model::{Message, Model, ModelRequest, Spend};

/// Refuse a capability, naming the boundary rather than the missing wire.
///
/// The message is the point. "not wired" reads like an oversight somebody
/// should fix; naming the invariant tells the reader this is the design.
fn refused(capability: &str, why: &str) -> EngineError {
    EngineError::Capability(format!(
        "tinysweeper grants no `{capability}` capability to a review graph: {why}"
    ))
}

/// The model capability: turns a node's config into a [`ModelRequest`].
///
/// Also the only place a lane's spend is counted. The engine gives a host no
/// channel to report usage back through, and this is the one object every model
/// call in a run passes through, so the tally lives here rather than being
/// reconstructed from node outputs afterwards — which is how a fallback's cost
/// used to go missing.
pub struct ModelCapability {
    model: Arc<dyn Model>,
    models: Models,
    spend: Mutex<Spend>,
    /// Worst-case cost of the calls currently in flight.
    ///
    /// The budget was checked against *completed* spend, which is a ceiling
    /// only when calls are serialised — and the whole point of the graph is
    /// that they are not. Eight concurrent reviewers all read a tally of zero,
    /// all pass, and the budget is exceeded by up to eight calls before the
    /// first one records anything. Serialising was what used to hide this.
    ///
    /// So a call reserves its worst case before dispatch and releases it after,
    /// whether it succeeded or failed. The reservation is deliberately
    /// pessimistic — the full output ceiling at the model's output rate — so
    /// the error is always toward refusing a call that would have fit, never
    /// toward allowing one that does not.
    reserved: Mutex<f64>,
    budget_usd: f64,
}

impl ModelCapability {
    /// Wire a graph's `agent` nodes to `model`, resolving tiers through
    /// `models`.
    ///
    /// The budget ceiling is enforced **here** rather than by the caller, and
    /// that is what lets a lane fan out at all. The previous design serialised
    /// every file precisely because usage is only known once a call returns, so
    /// concurrent work could start after the ceiling had already been spent.
    /// This object sees every call in the run, so it can refuse one no matter
    /// how many are in flight — which makes the budget a stronger guarantee
    /// than serialising ever gave, and costs no concurrency to get.
    pub fn new(model: Arc<dyn Model>, models: Models) -> Self {
        let budget_usd = models.budget_usd_per_pr;
        Self {
            model,
            models,
            spend: Mutex::new(Spend::default()),
            reserved: Mutex::new(0.0),
            budget_usd,
        }
    }

    /// Override the ceiling this capability enforces.
    ///
    /// A lane's share, when several lanes run against one pull request budget.
    pub fn with_budget(mut self, budget_usd: f64) -> Self {
        self.budget_usd = budget_usd;
        self
    }

    /// The underlying model.
    ///
    /// For the stages that are not panels — positioning a finding, falsifying
    /// one — which call the port directly and account for their own spend. They
    /// go through the port rather than the graph because neither is a review:
    /// they are arithmetic over a finding that already exists.
    pub fn model(&self) -> &Arc<dyn Model> {
        &self.model
    }

    /// What every call through this capability has cost so far.
    ///
    /// A poisoned lock yields an empty spend rather than panicking: losing the
    /// cost line is bad, and failing a completed review because the tally
    /// panicked is worse.
    pub fn spend(&self) -> Spend {
        self.spend
            .lock()
            .map(|s| s.clone())
            .unwrap_or_else(|poisoned| poisoned.into_inner().clone())
    }

    /// Take this call's worst case out of the budget, or refuse it.
    ///
    /// The guard returned releases the reservation on drop, so an early return,
    /// a provider error and a panic all give it back. Returning a bare `f64` and
    /// subtracting it after the call would leak the reservation on every error
    /// path — and a leaked reservation is a lane that refuses every later call
    /// for a budget nobody spent.
    fn reserve(&self, model_id: &str, max_tokens: u32) -> FlowResult<Reservation<'_>> {
        // Output only, at the full ceiling. Input is not counted because it is
        // already known to the caller and is the cheap half; the point is to be
        // pessimistic, not exact, and to be so without a second price lookup.
        let estimate =
            crate::harness::pricing::completion_cost(model_id, 0, 0, u64::from(max_tokens));

        let spent = self.spend().cost_usd();
        let mut reserved = self
            .reserved
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        // `>=` on committed spend alone, so a budget already exhausted refuses
        // even a call whose estimate is zero — an unpriced model must not become
        // a free one.
        if spent >= self.budget_usd {
            return Err(EngineError::Capability(
                crate::error::Error::Budget {
                    spent,
                    limit: self.budget_usd,
                }
                .to_string(),
            ));
        }

        // With nothing in flight, budget remaining is enough on its own. The
        // estimate is a deliberately pessimistic worst case — the full output
        // ceiling, and the *most expensive rate in the table* for a model with
        // no price — so on a small budget it can exceed the whole allowance by
        // itself. Applied unconditionally that refuses every call and the lane
        // never runs, which is strictly worse than the behaviour this replaced:
        // before, calls ran and stopped once real spend caught up.
        //
        // So the reservation bounds *concurrency*, never progress. One call at a
        // time is always allowed while budget remains, which is exactly the
        // serial behaviour that used to be the only guarantee; everything the
        // reservation adds is on top of it.
        if *reserved > 0.0 && spent + *reserved + estimate > self.budget_usd {
            return Err(EngineError::Capability(
                crate::error::Error::Budget {
                    spent: spent + *reserved,
                    limit: self.budget_usd,
                }
                .to_string(),
            ));
        }

        *reserved += estimate;
        Ok(Reservation {
            capability: self,
            estimate,
        })
    }

    /// Give back a reservation.
    fn release(&self, estimate: f64) {
        let mut reserved = self
            .reserved
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        // Clamped at zero: floating-point subtraction of the same values in a
        // different order can leave a tiny negative, which would slowly hand
        // out budget that does not exist.
        *reserved = (*reserved - estimate).max(0.0);
    }

    /// Read one required string out of a node's config.
    fn required<'a>(config: &'a Value, key: &str) -> FlowResult<&'a str> {
        config.get(key).and_then(Value::as_str).ok_or_else(|| {
            EngineError::Capability(format!("agent node config is missing a string `{key}`"))
        })
    }
}

/// Holds one in-flight call's worst-case cost against the budget.
///
/// Releases on drop rather than at a call site, so every path — success, a
/// provider error, an early return, a panic — gives the reservation back.
struct Reservation<'a> {
    capability: &'a ModelCapability,
    estimate: f64,
}

impl Drop for Reservation<'_> {
    fn drop(&mut self) {
        self.capability.release(self.estimate);
    }
}

#[async_trait]
impl LlmProvider for ModelCapability {
    /// `request` is the `agent` node's resolved config, verbatim — see
    /// `nodes::integration::agent`. This crate authors both sides of that
    /// contract, so the keys read here are the keys `flows::panel` writes.
    async fn complete(&self, request: Value, _conn: Option<&str>) -> FlowResult<Value> {
        // The model id, already resolved from tier by `council::reviewers`.
        // Resolution stays there rather than here so there is exactly one
        // answer to "what did this call run on", and it is the one the cost
        // line reports.
        let model_id = Self::required(&request, "model")?;

        let system = Self::required(&request, "system")?;
        let user = Self::required(&request, "prompt")?;
        let schema_name = Self::required(&request, "schema_name")?;

        let schema = request.get("schema").cloned().ok_or_else(|| {
            EngineError::Capability(
                "agent node config is missing `schema`; structured output is not optional".into(),
            )
        })?;

        // The node may lower the ceiling but never raise it: `models.max_tokens`
        // is a budget decision, and a graph is data that a repository can edit.
        // Clamped in `u64` and narrowed afterwards. Narrowing first wraps: a
        // node asking for `u32::MAX + 1` becomes `0` and the call generates
        // nothing, which reads downstream as a reviewer that found nothing
        // rather than as a rejected ceiling.
        let max_tokens = request
            .get("max_tokens")
            .and_then(Value::as_u64)
            .map_or(self.models.max_tokens, |n| {
                n.min(u64::from(self.models.max_tokens)) as u32
            });

        // Reserved before dispatch, released after. Checking completed spend
        // alone is a ceiling only when calls are serialised — see `reserved`.
        let _reservation = self.reserve(model_id, max_tokens)?;

        let response = self
            .model
            .complete(ModelRequest {
                model: model_id.to_string(),
                messages: vec![Message::system(system), Message::user(user)],
                schema,
                schema_name: schema_name.to_string(),
                max_tokens,
            })
            .await
            .map_err(|e| EngineError::Capability(e.to_string()))?;

        if let Ok(mut spend) = self.spend.lock() {
            spend.record(&response.model, response.usage);
        }

        // `model` rides alongside the payload so a consensus merge can say which
        // model produced an opinion — a fallback answering is exactly the case
        // worth surfacing, and it is invisible by the time findings are merged.
        Ok(json!({
            "json": response.value,
            "model": response.model,
        }))
    }
}

/// Denies every tool call.
pub struct NoTools;

#[async_trait]
impl ToolInvoker for NoTools {
    async fn invoke(&self, slug: &str, _args: Value, _conn: Option<&str>) -> FlowResult<Value> {
        Err(refused(
            "tool_call",
            &format!(
                "`{slug}` was requested, but a lane proposes and never acts — only `src/apply` \
                 may mutate a pull request"
            ),
        ))
    }
}

/// Denies every outbound request.
pub struct NoHttp;

#[async_trait]
impl HttpClient for NoHttp {
    async fn request(&self, spec: Value, _conn: Option<&str>) -> FlowResult<Value> {
        let url = spec.get("url").and_then(Value::as_str).unwrap_or("<unset>");
        Err(refused(
            "http_request",
            &format!(
                "a review's only network call is the model call, and `{url}` is not one; \
                 evidence is gathered before the graph runs"
            ),
        ))
    }
}

/// Denies every code execution.
pub struct NoCode;

#[async_trait]
impl CodeRunner for NoCode {
    async fn run(
        &self,
        _language: CodeLanguage,
        _source: &str,
        _input: Value,
    ) -> FlowResult<Value> {
        Err(refused(
            "code",
            "contributor code is never executed — we read the diff and the tree, and build nothing",
        ))
    }
}

/// Per-run state, discarded with the run.
///
/// A lane is a pure function of its evidence; persisting anything across runs
/// would make a review depend on a previous one in a way nothing reports.
#[derive(Default)]
pub struct RunState {
    entries: Mutex<std::collections::BTreeMap<String, Value>>,
}

#[async_trait]
impl StateStore for RunState {
    async fn load(&self, key: &str) -> FlowResult<Option<Value>> {
        Ok(self
            .entries
            .lock()
            .map_err(|_| refused("state", "the run's state lock was poisoned"))?
            .get(key)
            .cloned())
    }

    async fn store(&self, key: &str, value: Value) -> FlowResult<()> {
        self.entries
            .lock()
            .map_err(|_| refused("state", "the run's state lock was poisoned"))?
            .insert(key.to_string(), value);
        Ok(())
    }
}

/// Resolves the child graphs a `sub_workflow` node names.
///
/// The registry is fixed when the run is built, so a sub-agent can only reach a
/// graph this crate authored — see `flows::subagent`, where the one-level depth
/// bound is enforced by that registry containing no graph that itself spawns
/// one.
pub struct ChildGraphs {
    graphs: std::collections::BTreeMap<String, WorkflowGraph>,
}

impl ChildGraphs {
    /// Build a registry over `graphs`, keyed by workflow id.
    pub fn new(graphs: impl IntoIterator<Item = (String, WorkflowGraph)>) -> Self {
        Self {
            graphs: graphs.into_iter().collect(),
        }
    }

    /// A registry that resolves nothing, for a lane with no sub-agents.
    pub fn none() -> Self {
        Self {
            graphs: std::collections::BTreeMap::new(),
        }
    }
}

#[async_trait]
impl WorkflowResolver for ChildGraphs {
    async fn resolve(&self, workflow_id: &str) -> FlowResult<WorkflowGraph> {
        self.graphs.get(workflow_id).cloned().ok_or_else(|| {
            EngineError::Capability(format!(
                "no child workflow `{workflow_id}` is registered for this run"
            ))
        })
    }
}

/// Assemble the capability set a lane graph runs against.
///
/// Returns the [`ModelCapability`] alongside, because it owns the spend tally
/// and the caller needs it once the run finishes.
pub fn for_lane(
    model: Arc<dyn Model>,
    models: &Models,
    children: ChildGraphs,
) -> (Capabilities, Arc<ModelCapability>) {
    let llm = Arc::new(ModelCapability::new(model, models.clone()));
    let capabilities = with_llm(llm.clone(), children);

    (capabilities, llm)
}

/// Assemble capabilities around an existing [`ModelCapability`].
///
/// The panel builds one and reuses it across all three rounds, because the
/// budget ceiling and the spend tally both live in it — a fresh one per round
/// would let each round spend the whole ceiling.
pub fn with_llm(llm: Arc<ModelCapability>, children: ChildGraphs) -> Capabilities {
    Capabilities {
        llm: llm as Arc<dyn LlmProvider>,
        tools: Arc::new(NoTools),
        http: Arc::new(NoHttp),
        code: Arc::new(NoCode),
        state: Arc::new(RunState::default()),
        resolver: Arc::new(children),
        // No agent registry: an `agent` node here *is* one completion, not a
        // host-owned tool loop. Sub-agents are child workflows, which is what
        // bounds their depth structurally — see `flows::subagent`.
        agent: None,
        // Absent rather than refusing. A `shell` node fails with the engine's
        // own capability error, and there is no implementation in the tree that
        // could later be wired by accident.
        shell: None,
        memory: None,
        tasks: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::mock::MockModel;
    use crate::ports::model::Usage;

    fn models() -> Models {
        Models {
            flash: "vendor/flash".into(),
            scan: "vendor/scan".into(),
            deep: "vendor/deep".into(),
            max_tokens: 1_000,
            budget_usd_per_pr: 1.0,
            ..Models::default()
        }
    }

    fn request(model: &str) -> Value {
        json!({
            "model": model,
            "system": "s",
            "prompt": "p",
            "schema": { "type": "object" },
            "schema_name": "tinysweeper_test",
        })
    }

    fn capability(cost: f64, budget: f64) -> ModelCapability {
        let model = MockModel::always(json!({ "summary": "ok" })).with_usage(Usage {
            cost_usd: cost,
            ..Usage::default()
        });

        ModelCapability::new(Arc::new(model), models()).with_budget(budget)
    }

    #[tokio::test]
    async fn concurrent_calls_cannot_all_pass_a_check_none_of_them_has_paid_for() {
        // The race the reservation exists for. Every call reads *completed*
        // spend, and nothing completes until the calls return, so with a
        // check-only design all eight see zero, all eight pass, and the budget
        // is blown by eight calls before the first one records anything.
        //
        // A slow model is what makes the window wide enough to be certain: with
        // an instant mock the first call can finish before the second starts,
        // and the test would pass against the very bug it is written for.
        let model = SlowModel::default();
        let calls = model.started.clone();
        let capability =
            Arc::new(ModelCapability::new(Arc::new(model), models()).with_budget(0.10));

        let mut running = tokio::task::JoinSet::new();
        for _ in 0..8 {
            let capability = capability.clone();
            running.spawn(async move { capability.complete(request("vendor/deep"), None).await });
        }

        let mut allowed = 0usize;
        while let Some(result) = running.join_next().await {
            if result.expect("task").is_ok() {
                allowed += 1;
            }
        }

        assert!(
            allowed >= 1,
            "every call was refused; the budget is unusable"
        );
        assert!(
            allowed < 8,
            "all 8 concurrent calls passed a budget none of them had paid for"
        );
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            allowed,
            "a refused call still reached the provider"
        );
    }

    #[tokio::test]
    async fn a_pessimistic_estimate_bounds_concurrency_but_never_progress() {
        // The estimate is the full output ceiling at the *most expensive rate in
        // the table* when a model has no price, so on a small budget it exceeds
        // the whole allowance on its own. Applied unconditionally that refuses
        // every call and the lane never runs at all — strictly worse than the
        // behaviour the reservation replaced, where calls ran until real spend
        // caught up.
        let capability = ModelCapability::new(
            Arc::new(
                MockModel::always(json!({ "summary": "ok" })).with_usage(Usage {
                    cost_usd: 0.0,
                    ..Usage::default()
                }),
            ),
            models(),
        )
        .with_budget(0.000_001);

        assert!(
            capability
                .complete(request("no/such/model/anywhere"), None)
                .await
                .is_ok(),
            "a tiny budget refused every call, so the lane could never run"
        );
    }

    #[tokio::test]
    async fn a_failed_call_gives_its_reservation_back() {
        // A reservation that leaked on the error path would refuse every later
        // call for budget nobody spent — a lane that goes quiet after one
        // provider blip, reporting an exhausted budget that was never used.
        let capability = ModelCapability::new(
            Arc::new(MockModel::new().then_error("provider exploded")),
            models(),
        )
        .with_budget(1.0);

        assert!(
            capability
                .complete(request("vendor/deep"), None)
                .await
                .is_err()
        );
        assert_eq!(
            *capability.reserved.lock().expect("lock"),
            0.0,
            "the failed call kept its reservation"
        );
    }

    /// A model that takes long enough for concurrent calls to overlap.
    #[derive(Debug, Default)]
    struct SlowModel {
        started: Arc<std::sync::atomic::AtomicUsize>,
    }

    #[async_trait]
    impl Model for SlowModel {
        async fn complete(
            &self,
            request: ModelRequest,
        ) -> crate::error::Result<crate::ports::model::ModelResponse> {
            self.started
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            tokio::time::sleep(std::time::Duration::from_millis(120)).await;

            Ok(crate::ports::model::ModelResponse {
                value: json!({ "summary": "ok" }),
                model: request.model,
                usage: Usage {
                    cost_usd: 0.05,
                    ..Usage::default()
                },
            })
        }
    }

    #[tokio::test]
    async fn a_call_is_refused_once_the_ceiling_is_reached() {
        // The safety property that used to be bought by reviewing files one at
        // a time. It lives here now, which is what let the fan-out become
        // concurrent again — so this is the test that keeps the budget real.
        let capability = capability(2.0, 1.0);

        // The first call is allowed: nothing had been spent when it started.
        capability
            .complete(request("vendor/flash"), None)
            .await
            .expect("first");

        let refused = capability
            .complete(request("vendor/flash"), None)
            .await
            .expect_err("the ceiling was already exceeded");
        assert!(
            refused.to_string().contains("budget exhausted"),
            "{refused}"
        );
    }

    #[tokio::test]
    async fn spend_is_attributed_to_the_model_that_answered() {
        let capability = capability(0.25, 10.0);
        capability
            .complete(request("vendor/flash"), None)
            .await
            .expect("call");

        let spend = capability.spend();
        assert_eq!(spend.models, vec!["vendor/flash".to_string()]);
        assert!((spend.cost_usd() - 0.25).abs() < 1e-9);
    }

    #[tokio::test]
    async fn a_node_may_lower_the_token_ceiling_but_never_raise_it() {
        // A graph is data a repository can edit; `models.max_tokens` is a
        // budget decision that a graph must not be able to overrule.
        let model = Arc::new(MockModel::always(json!({})));
        let capability = ModelCapability::new(model.clone(), models());

        let mut raised = request("vendor/flash");
        raised["max_tokens"] = json!(999_999);
        capability.complete(raised, None).await.expect("call");

        let mut lowered = request("vendor/flash");
        lowered["max_tokens"] = json!(10);
        capability.complete(lowered, None).await.expect("call");

        let requests = model.requests();
        assert_eq!(requests[0].max_tokens, 1_000, "the config ceiling holds");
        assert_eq!(requests[1].max_tokens, 10, "a node may ask for less");
    }

    #[tokio::test]
    async fn every_capability_a_review_must_not_have_refuses_by_name() {
        // Each message names the invariant rather than the missing wire: "not
        // wired" reads like an oversight somebody should fix.
        let tools = NoTools.invoke("shell", json!({}), None).await.unwrap_err();
        assert!(tools.to_string().contains("only `src/apply`"), "{tools}");

        let http = NoHttp
            .request(json!({ "url": "https://example.com" }), None)
            .await
            .unwrap_err();
        assert!(http.to_string().contains("https://example.com"), "{http}");

        let code = NoCode
            .run(CodeLanguage::Python, "print(1)", json!({}))
            .await
            .unwrap_err();
        assert!(code.to_string().contains("never executed"), "{code}");
    }

    #[tokio::test]
    async fn an_unregistered_child_workflow_is_refused() {
        // The other half of the depth bound: a `sub_workflow` node can only
        // reach a graph this crate put in the registry.
        let err = ChildGraphs::none().resolve("anything").await.unwrap_err();
        assert!(err.to_string().contains("anything"), "{err}");
    }

    #[tokio::test]
    async fn a_call_naming_no_model_is_refused_rather_than_defaulted() {
        // The graph is data. A node that names no model must not silently
        // inherit one — which tier it inherited would decide both the quality
        // and the bill, invisibly.
        let capability = capability(0.0, 10.0);
        let mut anonymous = request("vendor/flash");
        anonymous.as_object_mut().unwrap().remove("model");

        let err = capability.complete(anonymous, None).await.unwrap_err();
        assert!(err.to_string().contains("`model`"), "{err}");
    }
}
