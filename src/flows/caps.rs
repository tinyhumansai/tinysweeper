//! The one capability a lane has: calling the model, under a budget.
//!
//! A review lane proposes and never acts, so the only thing it is granted is a
//! structured model call through [`crate::ports::model::Model`]. There is no
//! tool, HTTP, code or shell capability to refuse, because nothing in the lane
//! path can express one: a reviewer's turn is a [`Call`] — a system prompt, an
//! evidence suffix and a schema — and its answer is JSON the host reads. Repo
//! reads a reviewer asks for are answered by the host, against the read-only
//! [`crate::ports::tree::TreeReader`], in `flows::lookup`.
//!
//! This used to be the capability seam into a tinyflows graph, which needed
//! explicit refusing implementations for the engine's required `tools`, `http`
//! and `code` slots. The graphs were flat fan-out/merge shapes, so they are now
//! plain futures in `flows::runner`, and the refusals are structural.

use std::sync::{Arc, Mutex};

use serde_json::Value;

use crate::config::types::Models;
use crate::error::{Error, Result};
use crate::flows::panel::Call;
use crate::ports::model::{Message, Model, ModelRequest, ModelResponse, Spend};

/// The model capability a lane shares: every reviewer call goes through it.
///
/// Also the only place a lane's spend is counted. It is the one object every
/// model call in a lane passes through, so the tally lives here rather than
/// being reconstructed from answers afterwards — which is how a fallback's cost
/// used to go missing.
pub struct ModelCapability {
    model: Arc<dyn Model>,
    models: Models,
    spend: Mutex<Spend>,
    budget_usd: f64,
}

impl ModelCapability {
    /// Wire a lane's reviewers to `model`, under `models`' ceilings.
    ///
    /// The budget ceiling is enforced **here** rather than by the caller, and
    /// that is what lets a lane fan out at all. Usage is only known once a
    /// call returns, so concurrent work could otherwise start after the
    /// ceiling had already been spent. This object sees every call, so it can
    /// refuse one no matter how many are in flight — a stronger guarantee than
    /// serialising ever gave, and it costs no concurrency.
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

    /// The underlying model.
    ///
    /// For the stages that are not panels — positioning a finding, falsifying
    /// one — which call the port directly and account for their own spend.
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

    /// Make one reviewer's call, answering `schema`.
    ///
    /// The model id is the one `council::reviewers` resolved from the tier.
    /// Resolution stays there rather than here so there is exactly one answer
    /// to "what did this call run on", and it is the one the cost line reports.
    pub async fn call(&self, call: &Call, schema: &Value) -> Result<ModelResponse> {
        // Checked before the call, not after. Refusing a call that has already
        // been paid for would throw away work and still overspend.
        let spent = self.spend().cost_usd();
        if spent >= self.budget_usd {
            return Err(Error::Budget {
                spent,
                limit: self.budget_usd,
            });
        }
        // A call that names no model must not silently inherit one — which
        // tier it inherited would decide both the quality and the bill,
        // invisibly.
        if call.model.trim().is_empty() {
            return Err(Error::Model(format!(
                "reviewer `{}` names no model; refusing rather than defaulting",
                call.id
            )));
        }

        let response = self
            .model
            .complete(ModelRequest {
                model: call.model.clone(),
                messages: vec![
                    Message::system(call.system.clone()),
                    Message::user(call.prompt.clone()),
                ],
                schema: schema.clone(),
                schema_name: call.schema_name.clone(),
                max_tokens: self.models.max_tokens,
            })
            .await?;

        if let Ok(mut spend) = self.spend.lock() {
            spend.record(&response.model, response.usage);
        }
        Ok(response)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::mock::MockModel;
    use crate::ports::model::Usage;
    use serde_json::json;

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

    fn call(model: &str) -> Call {
        Call {
            id: "a".into(),
            model: model.into(),
            system: "s".into(),
            prompt: "p".into(),
            schema_name: "tinysweeper_test".into(),
        }
    }

    fn capability(cost: f64, budget: f64) -> ModelCapability {
        let model = MockModel::always(json!({ "summary": "ok" })).with_usage(Usage {
            cost_usd: cost,
            ..Usage::default()
        });

        ModelCapability::new(Arc::new(model), models()).with_budget(budget)
    }

    #[tokio::test]
    async fn a_call_is_refused_once_the_ceiling_is_reached() {
        // The safety property that used to be bought by reviewing files one at
        // a time. It lives here, which is what lets the fan-out be concurrent —
        // so this is the test that keeps the budget real.
        let capability = capability(2.0, 1.0);
        let schema = json!({ "type": "object" });

        // The first call is allowed: nothing had been spent when it started.
        capability
            .call(&call("vendor/flash"), &schema)
            .await
            .expect("first");

        let refused = capability
            .call(&call("vendor/flash"), &schema)
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
            .call(&call("vendor/flash"), &json!({}))
            .await
            .expect("call");

        let spend = capability.spend();
        assert_eq!(spend.models, vec!["vendor/flash".to_string()]);
        assert!((spend.cost_usd() - 0.25).abs() < 1e-9);
    }

    #[tokio::test]
    async fn every_call_carries_the_configured_ceiling_and_the_reviewers_turn() {
        let model = Arc::new(MockModel::always(json!({})));
        let capability = ModelCapability::new(model.clone(), models());
        capability
            .call(&call("vendor/deep"), &json!({ "type": "object" }))
            .await
            .expect("call");

        let sent = &model.requests()[0];
        assert_eq!(sent.max_tokens, 1_000);
        assert_eq!(sent.model, "vendor/deep");
        assert_eq!(sent.schema_name, "tinysweeper_test");
        assert_eq!(
            sent.messages,
            vec![Message::system("s"), Message::user("p")]
        );
    }

    #[tokio::test]
    async fn a_call_naming_no_model_is_refused_rather_than_defaulted() {
        let capability = capability(0.0, 10.0);
        let err = capability.call(&call("  "), &json!({})).await.unwrap_err();
        assert!(err.to_string().contains("names no model"), "{err}");
    }
}
