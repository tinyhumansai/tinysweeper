//! The real model, behind the `harness` feature.
//!
//! A thin adapter over tinyagents' OpenAI-compatible provider. OpenRouter,
//! Moonshot and MiniMax all speak the same wire format, so pointing `base_url`
//! elsewhere is the whole of "switching provider" — there is no second code
//! path to maintain and no provider-specific SDK in the tree.

use std::sync::Arc;

use async_trait::async_trait;
use tinyagents::harness::message::Message as TaMessage;
use tinyagents::harness::model::ResponseFormat;
use tinyagents::harness::providers::openai::OpenAiModel;
use tinyagents::harness::runtime::{AgentHarness, RunPolicy};

use crate::config::types::Models;
use crate::error::{Error, Result};
use crate::ports::model::{Model, ModelRequest, ModelResponse, Role, Usage};

/// A model reached through an OpenAI-compatible gateway.
pub struct GatewayModel {
    api_key: String,
    base_url: String,
    fallbacks: Vec<String>,
}

impl GatewayModel {
    /// Build from the `[models]` config, reading the key from the environment
    /// variable the config names.
    ///
    /// The key is never in the config file, only the name of the variable
    /// holding it — which is why the error names the variable rather than
    /// suggesting anyone put a key on disk.
    pub fn from_config(models: &Models) -> Result<Self> {
        let api_key = std::env::var(&models.api_key_env).map_err(|_| {
            Error::Model(format!(
                "{} is not set; it holds the key for {}",
                models.api_key_env, models.base_url
            ))
        })?;

        Ok(Self {
            api_key,
            base_url: models.base_url.clone(),
            fallbacks: models.fallback.clone(),
        })
    }

    fn harness(&self, model: &str) -> Result<AgentHarness<()>> {
        let provider = OpenAiModel::new(&self.api_key)
            .with_base_url(&self.base_url)
            .with_model(model)
            // Identifies us to OpenRouter, which is how per-application usage
            // shows up separately in their dashboard.
            .with_header(
                "HTTP-Referer",
                "https://github.com/tinyhumansai/tinysweeper",
            )
            .with_header("X-Title", "tinysweeper");

        let mut harness: AgentHarness<()> = AgentHarness::new();
        harness
            .register_model("gateway", Arc::new(provider))
            .set_default_model("gateway");
        Ok(harness)
    }

    async fn call(&self, model: &str, request: &ModelRequest) -> Result<ModelResponse> {
        let mut harness = self.harness(model)?;
        harness.with_policy(RunPolicy {
            default_response_format: Some(ResponseFormat::json_schema(
                &request.schema_name,
                request.schema.clone(),
            )),
            ..RunPolicy::default()
        });

        let messages: Vec<TaMessage> = request
            .messages
            .iter()
            .map(|m| match m.role {
                Role::System => TaMessage::system(&m.content),
                Role::User => TaMessage::user(&m.content),
                Role::Assistant => TaMessage::assistant(&m.content),
            })
            .collect();

        let run = harness
            .invoke_default(&(), messages)
            .await
            .map_err(|err| Error::Model(format!("{model}: {err}")))?;

        // Structured output is not optional: a lane that falls back to parsing
        // prose posts nonsense the first time a model phrases something
        // differently.
        let value = run.structured.ok_or_else(|| {
            Error::Model(format!(
                "{model} returned no structured output; the response did not satisfy the schema"
            ))
        })?;

        let usage = Usage {
            input_tokens: run.usage.input_tokens as u64,
            output_tokens: run.usage.output_tokens as u64,
            cached_tokens: run.usage.cached_input_tokens as u64,
            cost_usd: run.cost.total_usd,
        };

        Ok(ModelResponse {
            value,
            model: model.to_string(),
            usage,
        })
    }
}

#[async_trait]
impl Model for GatewayModel {
    async fn complete(&self, request: ModelRequest) -> Result<ModelResponse> {
        let mut last = match self.call(&request.model, &request).await {
            Ok(response) => return Ok(response),
            Err(err) => err,
        };

        // Fallbacks exist because a single provider outage should degrade the
        // review rather than fail the check. The model that actually answered
        // is reported back so the summary can say so.
        for fallback in &self.fallbacks {
            tracing::warn!(
                primary = %request.model,
                fallback = %fallback,
                error = %last,
                "model call failed; trying the next model"
            );
            match self.call(fallback, &request).await {
                Ok(response) => return Ok(response),
                Err(err) => last = err,
            }
        }

        Err(last)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn models() -> Models {
        Models {
            gateway: "openrouter".into(),
            base_url: "https://openrouter.ai/api/v1".into(),
            api_key_env: "TINYSWEEPER_TEST_KEY_ABSENT".into(),
            scan: "a".into(),
            deep: "b".into(),
            fallback: vec![],
            max_tokens: 100,
            budget_usd_per_pr: 1.0,
        }
    }

    #[test]
    fn a_missing_key_names_the_variable_rather_than_suggesting_a_file() {
        let err = GatewayModel::from_config(&models())
            .unwrap_err()
            .to_string();
        assert!(err.contains("TINYSWEEPER_TEST_KEY_ABSENT"), "{err}");
        assert!(err.contains("openrouter.ai"), "{err}");
    }
}
