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
use crate::flows::tier::Tier;
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

    /// Read one required string out of a node's config.
    fn required<'a>(config: &'a Value, key: &str) -> FlowResult<&'a str> {
        config.get(key).and_then(Value::as_str).ok_or_else(|| {
            EngineError::Capability(format!("agent node config is missing a string `{key}`"))
        })
    }
}

#[async_trait]
impl LlmProvider for ModelCapability {
    /// `request` is the `agent` node's resolved config, verbatim — see
    /// `nodes::integration::agent`. This crate authors both sides of that
    /// contract, so the keys read here are the keys `flows::panel` writes.
    async fn complete(&self, request: Value, _conn: Option<&str>) -> FlowResult<Value> {
        // Checked before the call, not after. Refusing a call that has already
        // been paid for would throw away work and still overspend.
        let spent = self.spend().cost_usd();
        if spent >= self.budget_usd {
            return Err(EngineError::Capability(
                crate::error::Error::Budget {
                    spent,
                    limit: self.budget_usd,
                }
                .to_string(),
            ));
        }

        let tier = Tier::parse(Self::required(&request, "tier")?)
            .map_err(|e| EngineError::Capability(e.to_string()))?;

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
        let max_tokens = request
            .get("max_tokens")
            .and_then(Value::as_u64)
            .map_or(self.models.max_tokens, |n| {
                (n as u32).min(self.models.max_tokens)
            });

        let response = self
            .model
            .complete(ModelRequest {
                model: tier.model_id(&self.models).to_string(),
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

    let capabilities = Capabilities {
        llm: llm.clone() as Arc<dyn LlmProvider>,
        tools: Arc::new(NoTools),
        http: Arc::new(NoHttp),
        code: Arc::new(NoCode),
        state: Arc::new(RunState::default()),
        resolver: Arc::new(children),
        // No agent registry: an `agent` node here *is* one completion, not a
        // host-owned tool loop. Sub-agents are `sub_workflow` nodes, which is
        // what bounds their depth structurally.
        agent: None,
        // Absent rather than refusing. A `shell` node fails with the engine's
        // own capability error, and there is no implementation in the tree that
        // could later be wired by accident.
        shell: None,
        memory: None,
        tasks: None,
    };

    (capabilities, llm)
}
