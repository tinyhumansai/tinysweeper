//! The real model, behind the `harness` feature.
//!
//! A thin adapter over tinyagents' OpenAI-compatible provider. OpenRouter,
//! Moonshot and MiniMax all speak the same wire format, so pointing `base_url`
//! elsewhere is the whole of "switching provider" — there is no second code
//! path to maintain and no provider-specific SDK in the tree.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::json;
use tinyagents::harness::context::RunConfig;
use tinyagents::harness::message::Message as TaMessage;
use tinyagents::harness::model::ResponseFormat;
use tinyagents::harness::providers::openai::OpenAiModel;
use tinyagents::harness::runtime::{AgentHarness, RunPolicy};

use crate::config::types::{Models, ProviderRouting};
use crate::error::{Error, Result};
use crate::harness::pricing;
use crate::ports::model::{Model, ModelRequest, ModelResponse, Role, Usage};

/// A model reached through an OpenAI-compatible gateway.
///
/// `Debug` is written by hand rather than derived: deriving it would print the
/// API key in any log line, panic message or test failure that formats this.
pub struct GatewayModel {
    api_key: String,
    base_url: String,
    fallbacks: Vec<String>,
    reasoning_effort: String,
    provider: ProviderRouting,
}

impl std::fmt::Debug for GatewayModel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GatewayModel")
            .field("base_url", &self.base_url)
            .field("fallbacks", &self.fallbacks)
            .field("provider", &self.provider)
            .field("api_key", &"<redacted>")
            .finish()
    }
}

/// The `reasoning` block sent with every request.
///
/// `"off"` disables it outright rather than asking for the lowest effort: a
/// model that must think is better served by a deployment that says so, and
/// "off" is the setting that rescues one whose reasoning eats the answer.
fn reasoning_options(effort: &str) -> serde_json::Value {
    match effort.trim() {
        "off" | "" => json!({ "reasoning": { "enabled": false } }),
        effort => json!({ "reasoning": { "effort": effort } }),
    }
}

/// The `provider` block sent with every request, or `Null` when unpinned.
///
/// OpenRouter reads this at the top level of the body to choose which upstream
/// serves the call. Empty names are dropped rather than sent: an accidental
/// `""` in the list would otherwise become a provider the gateway cannot match,
/// and with `allow_fallbacks = false` that is a hard failure on every request.
fn provider_options(routing: &ProviderRouting) -> serde_json::Value {
    if routing.is_empty() {
        return serde_json::Value::Null;
    }

    let order: Vec<&str> = routing
        .order
        .iter()
        .map(|p| p.trim())
        .filter(|p| !p.is_empty())
        .collect();

    json!({
        "provider": {
            "order": order,
            "allow_fallbacks": routing.allow_fallbacks,
        }
    })
}

/// Merge the `reasoning` and `provider` blocks into one options object.
///
/// Both are top-level body keys, and `with_default_provider_options` takes a
/// single value, so they are combined here rather than set twice — the second
/// call would replace the first outright.
fn request_options(effort: &str, routing: &ProviderRouting) -> serde_json::Value {
    let mut options = reasoning_options(effort);

    if let (Some(target), Some(provider)) = (
        options.as_object_mut(),
        provider_options(routing).as_object(),
    ) {
        for (key, value) in provider {
            target.insert(key.clone(), value.clone());
        }
    }

    options
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
            reasoning_effort: models.reasoning_effort.clone(),
            provider: models.provider.clone(),
        })
    }

    fn harness(&self, model: &str) -> Result<AgentHarness<()>> {
        let provider = OpenAiModel::new(&self.api_key)
            .with_base_url(&self.base_url)
            .with_model(model)
            // Reasoning, at the configured effort.
            //
            // This was hard-disabled, and the reason is worth keeping because
            // it is a real hazard rather than a preference: reasoning is billed
            // against the same `max_tokens` as the answer, so a model that
            // thinks too much spends the whole budget and returns **empty**
            // content. Measured on `kimi-k3` with a 49k-token prompt:
            // finish_reason `length`, all 8000 completion tokens consumed, 17k
            // characters of reasoning, nothing left to answer with — and every
            // review then fell back to a weaker model, silently. Capping it
            // with `reasoning: {max_tokens: N}` did not hold; that model
            // ignored the cap and ran to 37k characters.
            //
            // It is on again because the model changed, and the new one was
            // measured rather than assumed. `deepseek-v4-pro` at effort `high`,
            // same shape of prompt and the same 8000-token ceiling: 416
            // reasoning tokens, 522 completion tokens, `finish_reason` `stop`,
            // valid structured output. Two orders of magnitude away from the
            // budget rather than over it.
            //
            // `models.reasoning_effort = "off"` restores the old behaviour for
            // a deployment that puts a thinking-heavy model back.
            //
            // The `provider` pin rides in the same object; see
            // [`request_options`] for why the two are merged rather than set
            // separately.
            .with_default_provider_options(request_options(
                &self.reasoning_effort,
                &self.provider,
            ))
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

        // `invoke` rather than `invoke_default`, because the run configuration
        // is where the output ceiling lives — see [`run_config`].
        let run = harness
            .invoke(&(), (), run_config(request), messages)
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

        let totals = run.usage.usage;
        let usage = Usage {
            input_tokens: totals.input_tokens,
            output_tokens: totals.output_tokens,
            cached_tokens: totals.cache_read_tokens,
            embed_tokens: 0,
            cost_usd: pricing::completion_cost(
                model,
                totals.input_tokens,
                totals.cache_read_tokens,
                totals.output_tokens,
            ),
        };

        Ok(ModelResponse {
            value,
            model: model.to_string(),
            usage,
        })
    }
}

/// The run this call is made from, carrying the configured output ceiling.
///
/// `models.max_tokens` reaches the provider as the run's per-turn output cap
/// rather than as a field on the request: the agent loop builds the provider
/// request itself, and `RunConfig::max_turn_output_tokens` is the documented
/// hook it applies before dispatching. Setting it on a request we do not own
/// would be discarded — which is exactly what used to happen to this setting.
///
/// The loop lowers, never raises: it takes the minimum of this cap and any cap
/// the request already carries, and its truncated-empty retry may still grow
/// the budget from here. Both are wanted — the ceiling is protection against a
/// runaway answer, not a demand for one.
fn run_config(request: &ModelRequest) -> RunConfig {
    let config = RunConfig::new("tinysweeper-lane");
    // `config::validate` rejects `max_tokens = 0`, but a `Config` built in code
    // can carry it, and forwarding a zero cap asks the provider for an empty
    // answer on every lane. Leave the ceiling off rather than guarantee failure.
    if request.max_tokens == 0 {
        return config;
    }
    config.with_max_turn_output_tokens(request.max_tokens)
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
            reasoning_effort: "high".into(),
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

    fn request(max_tokens: u32) -> ModelRequest {
        ModelRequest {
            model: "moonshotai/kimi-k3".into(),
            messages: vec![],
            schema: json!({"type": "object"}),
            schema_name: "tinysweeper_critique".into(),
            max_tokens,
        }
    }

    #[test]
    fn the_configured_ceiling_reaches_the_run_the_provider_is_called_from() {
        // `models.max_tokens` was accepted, validated, documented as the
        // ceiling on a response — and then dropped on the floor, so the
        // provider's own default decided how long an answer could get.
        assert_eq!(
            run_config(&request(4_096)).max_turn_output_tokens,
            Some(4_096)
        );
    }

    #[test]
    fn a_zero_ceiling_is_not_forwarded() {
        // `config::validate` rejects `max_tokens = 0`, but a `Config` built in
        // code can still carry it, and asking a provider for zero output tokens
        // turns a configuration mistake into an empty answer on every lane.
        assert_eq!(run_config(&request(0)).max_turn_output_tokens, None);
    }

    #[test]
    fn debug_never_prints_the_api_key() {
        let model = GatewayModel {
            reasoning_effort: "high".into(),
            api_key: "sk-secret-value".into(),
            base_url: "https://openrouter.ai/api/v1".into(),
            fallbacks: vec![],
        };
        let rendered = format!("{model:?}");
        assert!(!rendered.contains("sk-secret-value"), "{rendered}");
        assert!(rendered.contains("<redacted>"));
    }

    #[test]
    fn a_missing_key_names_the_variable_rather_than_suggesting_a_file() {
        let err = GatewayModel::from_config(&models())
            .unwrap_err()
            .to_string();
        assert!(err.contains("TINYSWEEPER_TEST_KEY_ABSENT"), "{err}");
        assert!(err.contains("openrouter.ai"), "{err}");
    }

    #[test]
    fn off_disables_reasoning_rather_than_asking_for_a_little() {
        // The escape hatch. A model whose reasoning eats the answer is not
        // fixed by asking it to think less — `kimi-k3` ignored a 4096-token cap
        // and ran to 37k characters — so "off" has to mean off.
        assert_eq!(
            reasoning_options("off"),
            json!({ "reasoning": { "enabled": false } })
        );
        assert_eq!(
            reasoning_options(""),
            json!({ "reasoning": { "enabled": false } })
        );
    }

    #[test]
    fn an_effort_is_passed_through_as_an_effort() {
        assert_eq!(
            reasoning_options("high"),
            json!({ "reasoning": { "effort": "high" } })
        );
    }

    #[test]
    fn the_configured_effort_reaches_the_gateway() {
        // `reasoning_options` is tested in isolation above; this asserts the
        // wire between config and gateway, which is the half that would leave
        // the setting inert — the same failure `max_tokens` had, where the
        // value was read, validated, documented, and then never forwarded.
        let mut models = models();
        models.reasoning_effort = "off".into();

        // SAFETY-adjacent: no env mutation. The key is read from a variable the
        // config names, so the test names one it sets nowhere and asserts the
        // error, then builds the gateway directly for the positive case.
        let gateway = GatewayModel {
            api_key: "unused".into(),
            base_url: models.base_url.clone(),
            fallbacks: models.fallback.clone(),
            reasoning_effort: models.reasoning_effort.clone(),
        };

        assert_eq!(
            reasoning_options(&gateway.reasoning_effort),
            json!({ "reasoning": { "enabled": false } })
        );
    }
}
