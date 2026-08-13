//! The real model, behind the `harness` feature.
//!
//! A thin adapter over tinyagents' OpenAI-compatible provider. OpenRouter,
//! Moonshot and MiniMax all speak the same wire format, so pointing `base_url`
//! elsewhere is the whole of "switching provider" — there is no second code
//! path to maintain and no provider-specific SDK in the tree.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::json;
use tinyagents::harness::context::{RunConfig, RunContext};
use tinyagents::harness::events::EventSink;
use tinyagents::harness::message::Message as TaMessage;
use tinyagents::harness::model::ResponseFormat;
use tinyagents::harness::providers::openai::OpenAiModel;
use tinyagents::harness::runtime::{AgentHarness, PayloadCapture, RunPolicy};
use tinyagents::{
    HarnessEventJournal, InMemoryEventJournal, JournalSink, LangfuseClient, LangfuseTraceConfig,
};

use crate::config::types::{Models, ProviderRouting, StructuredOutput};
use crate::error::{Error, Result};
use crate::harness::{pricing, schema};
use crate::ports::model::{
    Message as CrateMessage, Model, ModelRequest, ModelResponse, Role, Usage,
};

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
    structured_output: StructuredOutput,
    langfuse: Option<LangfuseClient>,
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

/// The `provider` routing block, or `Null` when unpinned.
///
/// OpenRouter reads this at the top level of the body to choose which upstream
/// serves the call. Empty names are dropped rather than sent: an unmatchable
/// name plus `allow_fallbacks = false` is a hard `404 No endpoints found` on
/// every request the deployment makes — which is exactly how the first version
/// of this shipped, and it took a live run against a real pull request to see
/// it, because every unit test uses a mock that never routes.
fn provider_pin(routing: &ProviderRouting) -> serde_json::Value {
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

/// Everything sent alongside every request that the OpenAI wire format has no
/// field for: the reasoning block, and the ask for real accounting.
///
/// `usage.include` is OpenRouter's opt-in for returning what the call actually
/// cost. Without it the only figure available is [`pricing`]'s own estimate from
/// a hand-maintained rate table, and `models.budget_usd_per_pr` — a hard stop on
/// a real bill — is then enforced against a guess that drifts every time a
/// provider reprices. A gateway that does not know the field ignores it.
fn provider_options(effort: &str, routing: &ProviderRouting) -> serde_json::Value {
    let mut options = reasoning_options(effort);
    options["usage"] = json!({ "include": true });

    // The pin is merged in rather than set separately: `reasoning`, `usage` and
    // `provider` are all top-level body keys reaching the wire through one
    // `with_default_provider_options` call, so writing them apart would drop
    // whichever went second.
    if let (Some(target), Some(pin)) = (options.as_object_mut(), provider_pin(routing).as_object())
    {
        for (key, value) in pin {
            target.insert(key.clone(), value.clone());
        }
    }

    options
}

/// The cost the gateway says it charged, when it says so.
///
/// Read out of the raw response body rather than the parsed usage, because the
/// OpenAI wire shape tinyagents parses has no cost field — this one is
/// OpenRouter's extension, returned because [`provider_options`] asked for it.
/// `None` means the gateway reported nothing and the estimate stands.
fn gateway_cost(raw: Option<&serde_json::Value>) -> Option<f64> {
    let cost = raw?.get("usage")?.get("cost")?.as_f64()?;
    // A gateway that reports a nonsensical cost is a gateway to disbelieve: a
    // negative figure would credit the budget rather than spend it.
    (cost.is_finite() && cost >= 0.0).then_some(cost)
}

/// The conversation as it goes on the wire, including anything the structured
/// output mode has to say.
///
/// Split out so the coupling can be tested: under
/// [`StructuredOutput::JsonObject`] the provider is told only "return json", so
/// if this function stops appending the schema the model is left describing a
/// contract nobody gave it — and that failure looks like a quality regression
/// rather than a bug, which is exactly the kind that survives a review.
fn wire_messages(request: &ModelRequest, mode: StructuredOutput) -> Vec<CrateMessage> {
    let mut messages = request.messages.clone();
    // Appended as its own system message rather than folded into the lane
    // prompt: the lane prompts are shared with the mock and the cassettes, and
    // this text is a property of how *this* gateway asks for structured output,
    // not of what the lane wants said.
    if mode == StructuredOutput::JsonObject {
        messages.push(CrateMessage::system(schema::json_mode_instruction(
            &request.schema,
        )));
    }
    messages
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
            structured_output: models.structured_output,
            langfuse: langfuse_client(),
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
            // It was turned on again on the strength of a measurement — 416
            // reasoning tokens on `deepseek-v4-pro` at `high` — and that
            // measurement was taken on a toy prompt. Re-measured against a
            // 23k-token diff, the same model at the same setting and the same
            // 8000-token ceiling spends **8000** reasoning tokens and returns
            // empty content. So this is not a hazard that belonged to `kimi-k3`
            // and went away; it is a property of reasoning sharing the budget,
            // and every thinking model has it.
            //
            // Two findings from that re-measurement are load-bearing here:
            //
            // - **`low` is not a smaller `high`.** Both configured models burn
            //   the entire allowance at either setting. This key selects a
            //   *style* of thinking, never an amount, so it cannot be used to
            //   bound spend. Only `"off"` bounds it.
            // - **The failure is bimodal.** There is no setting at which the
            //   model thinks a little and answers a little: either reasoning
            //   fits and the answer is whole, or reasoning takes everything and
            //   `finish_reason` is `length` with nothing to parse.
            //
            // What keeps it working today is `models.max_tokens`, raised to
            // 16000, not this key. Anyone lowering that number should read the
            // table in `config/defaults.toml` first.
            //
            // `models.reasoning_effort = "off"` restores the old behaviour for
            // a deployment that puts a thinking-heavy model back.
            //
            // The `provider` pin rides in the same object; see
            // [`provider_options`] for why they are merged rather than set
            // separately.
            .with_default_provider_options(provider_options(&self.reasoning_effort, &self.provider))
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

    async fn call(&self, model: &str, request: &ModelRequest, cap: u32) -> Result<CallOutcome> {
        let mut harness = self.harness(model)?;
        // Who enforces the schema. These two arms are one decision, not two
        // independent settings: `JsonObject` asks the provider for *some* JSON
        // and therefore has to carry the schema in the prompt itself, and the
        // prompt half is added below. Changing one arm without the other either
        // sends a schema nobody reads or asks for a shape nobody described.
        let response_format = match self.structured_output {
            StructuredOutput::Schema => {
                ResponseFormat::json_schema(&request.schema_name, request.schema.clone())
            }
            StructuredOutput::JsonObject => ResponseFormat::JsonObject,
        };
        harness.with_policy(RunPolicy {
            default_response_format: Some(response_format),
            capture: if self.langfuse.is_some() {
                PayloadCapture {
                    model_io: true,
                    tool_io: false,
                }
            } else {
                PayloadCapture::default()
            },
            ..RunPolicy::default()
        });

        let messages: Vec<TaMessage> = wire_messages(request, self.structured_output)
            .iter()
            .map(|m| match m.role {
                Role::System => TaMessage::system(&m.content),
                Role::User => TaMessage::user(&m.content),
                Role::Assistant => TaMessage::assistant(&m.content),
            })
            .collect();

        // `invoke` rather than `invoke_default`, because the run configuration
        // is where the output ceiling lives — see [`run_config`].
        let run_config = run_config(cap);
        let run_id = run_config.run_id.clone();
        let (journal, journal_sink) = self
            .langfuse
            .as_ref()
            .map(|_| {
                let journal = Arc::new(InMemoryEventJournal::new());
                let sink = Arc::new(JournalSink::new(journal.clone(), run_id.clone()));
                (journal, sink)
            })
            .unzip();
        let events = EventSink::new();
        if let Some(sink) = &journal_sink {
            events.subscribe(sink.clone());
        }
        let result = harness
            .invoke_in_context(
                &(),
                RunContext::new(run_config, ()).with_events(events),
                messages,
            )
            .await;

        if let (Some(journal), Some(sink), Some(client)) =
            (journal, journal_sink, self.langfuse.as_ref())
        {
            sink.flush();
            match journal.read_from(run_id.as_str(), 0).await {
                Ok(observations) if !observations.is_empty() => {
                    if let Err(err) = client
                        .send_observations(
                            LangfuseTraceConfig {
                                name: Some("tinysweeper model call".to_string()),
                                environment: std::env::var("LANGFUSE_ENVIRONMENT").ok(),
                                ..Default::default()
                            },
                            &observations,
                        )
                        .await
                    {
                        tracing::warn!(%err, "could not export model call to Langfuse");
                    }
                }
                Ok(_) => {}
                Err(err) => tracing::warn!(%err, "could not read Langfuse observations"),
            }
        }

        let run = result.map_err(|err| Error::Model(format!("{model}: {err}")))?;

        let totals = run.usage.usage;
        let finish_reason = run
            .final_response
            .as_ref()
            .and_then(|response| response.finish_reason.clone())
            .unwrap_or_default();
        let reported_cost = gateway_cost(run.final_response.as_ref().and_then(|r| r.raw.as_ref()));

        // Every model call, at info, because the two failures this module has
        // actually had — reasoning eating the whole budget, and an answer cut
        // off part way through the findings array — are both invisible without
        // these four numbers side by side.
        tracing::info!(
            model,
            cap,
            input_tokens = totals.input_tokens,
            cached_tokens = totals.cache_read_tokens,
            output_tokens = totals.output_tokens,
            reasoning_tokens = totals.reasoning_tokens,
            finish_reason = %finish_reason,
            reported_cost_usd = reported_cost,
            "model call"
        );

        // Truncation, reported rather than repaired.
        //
        // `finish_reason == "length"` means the answer was cut off at the
        // ceiling. The harness recovers the *empty* case on its own, but the
        // expensive case is the partial one: its repair ladder closes the
        // unterminated JSON, so a findings array cut off after two entries
        // parses cleanly and reads exactly like a review that found two things.
        // Returning `Truncated` instead sends the call back up to `complete`,
        // which retries with a larger ceiling before anything is published.
        if finish_reason == "length" {
            return Ok(CallOutcome::Truncated {
                output_tokens: totals.output_tokens,
                reasoning_tokens: totals.reasoning_tokens,
            });
        }

        // Reasoning is billed against the same ceiling as the answer, so a
        // model spending most of the budget thinking is one prompt away from
        // the truncation above. Say so while the review still succeeds.
        if totals.reasoning_tokens * 2 > u64::from(cap) {
            tracing::warn!(
                model,
                cap,
                reasoning_tokens = totals.reasoning_tokens,
                "reasoning consumed over half the output budget; \
                 consider raising `models.max_tokens` or lowering `models.reasoning_effort`"
            );
        }

        // Structured output is not optional: a lane that falls back to parsing
        // prose posts nonsense the first time a model phrases something
        // differently.
        //
        // `run.structured` is populated only when the harness was given a schema
        // to extract against, which is the `schema` mode. Under `json_object`
        // there is no schema on the wire, so the harness has nothing to extract
        // with and leaves it empty — the answer arrives as the run's text. That
        // is *not* a licence to parse prose: `serde_json::from_str` either
        // yields a JSON value or fails, and `schema::parse` downstream rejects
        // any value of the wrong shape. Both modes end at the same guarantee;
        // only the enforcer differs.
        let value = match (run.structured.clone(), self.structured_output) {
            (Some(value), _) => value,
            (None, StructuredOutput::JsonObject) => {
                let text = run.text().unwrap_or_default();
                serde_json::from_str(text.trim()).map_err(|err| {
                    Error::Model(format!(
                        "{model} answered in `json_object` mode with something that is not \
                         JSON: {err}"
                    ))
                })?
            }
            (None, StructuredOutput::Schema) => {
                return Err(Error::Model(format!(
                    "{model} returned no structured output; the response did not satisfy the \
                     schema"
                )));
            }
        };

        let usage = Usage {
            input_tokens: totals.input_tokens,
            output_tokens: totals.output_tokens,
            cached_tokens: totals.cache_read_tokens,
            embed_tokens: 0,
            // What the gateway says it charged, when it says — the rate table is
            // a fallback for a gateway that reports nothing, not the preferred
            // figure. `models.budget_usd_per_pr` stops a review on this number,
            // so an estimate that drifts with a provider's repricing is the
            // wrong thing to enforce a real bill against.
            cost_usd: reported_cost.unwrap_or_else(|| {
                pricing::completion_cost(
                    model,
                    totals.input_tokens,
                    totals.cache_read_tokens,
                    totals.output_tokens,
                )
            }),
        };

        Ok(CallOutcome::Answer(ModelResponse {
            value,
            model: model.to_string(),
            usage,
        }))
    }

    /// One model, tried at the configured ceiling and then at growing ones
    /// until it answers without being cut off.
    ///
    /// The ladder doubles, capped at [`MAX_TRUNCATION_RETRIES`] steps, and the
    /// last rung's truncation is returned as an error naming the config key —
    /// a review that cannot fit its findings in four times the configured
    /// budget is a review whose operator needs to know, not one to publish half
    /// of. Tokens are billed as produced, so a rung that is never reached costs
    /// nothing.
    async fn call_until_complete(
        &self,
        model: &str,
        request: &ModelRequest,
    ) -> Result<ModelResponse> {
        let base = request.max_tokens;
        let ladder = truncation_ladder(base);
        let last = ladder.len() - 1;

        for (attempt, cap) in ladder.into_iter().enumerate() {
            match self.call(model, request, cap).await? {
                CallOutcome::Answer(response) => return Ok(response),
                CallOutcome::Truncated {
                    output_tokens,
                    reasoning_tokens,
                } => {
                    if attempt == last {
                        return Err(Error::Model(format!(
                            "{model} ran out of output tokens at {cap} \
                             ({output_tokens} generated, {reasoning_tokens} of them reasoning); \
                             the answer was cut off. Raise `models.max_tokens` (currently {base}) \
                             or lower `models.reasoning_effort`."
                        )));
                    }
                    tracing::warn!(
                        model,
                        cap,
                        output_tokens,
                        reasoning_tokens,
                        "answer was cut off at the output ceiling; retrying with a larger one"
                    );
                }
            }
        }

        unreachable!("the loop returns on its last iteration")
    }
}

/// How many times a truncated answer is retried with a doubled ceiling before
/// the call fails. Two retries means the last attempt runs at 4x
/// `models.max_tokens`.
const MAX_TRUNCATION_RETRIES: u32 = 2;

/// The output ceilings one model is tried at, in order.
///
/// A zero base means "no ceiling" (see [`run_config`]): there is nothing to
/// double, and a truncation at that point is the provider's own limit rather
/// than ours, so the ladder is a single rung and the failure is reported
/// straight away.
fn truncation_ladder(base: u32) -> Vec<u32> {
    if base == 0 {
        return vec![0];
    }
    (0..=MAX_TRUNCATION_RETRIES)
        .map(|step| base.saturating_mul(1 << step))
        .collect()
}

/// What one model call produced.
///
/// Truncation is a third outcome rather than an error because it is the one
/// failure worth *retrying differently*: same model, same prompt, more room.
enum CallOutcome {
    /// A complete, schema-satisfying answer.
    Answer(ModelResponse),
    /// The answer was cut off at the output ceiling.
    Truncated {
        /// Tokens generated before the cut-off.
        output_tokens: u64,
        /// How many of them went to the hidden reasoning channel.
        reasoning_tokens: u64,
    },
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
/// The run id is unique per call, and stays that way across the truncation
/// ladder: each rung is its own Langfuse trace, so a retry at a larger ceiling
/// is visible as a retry rather than overwriting the attempt that was cut off.
fn run_config(cap: u32) -> RunConfig {
    static NEXT_RUN_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let run_id = NEXT_RUN_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let config = RunConfig::new(format!("tinysweeper-lane-{run_id}"));
    // `config::validate` rejects `max_tokens = 0`, but a `Config` built in code
    // can carry it, and forwarding a zero cap asks the provider for an empty
    // answer on every lane. Leave the ceiling off rather than guarantee failure.
    if cap == 0 {
        return config;
    }
    config.with_max_turn_output_tokens(cap)
}

/// Build the direct Langfuse exporter only when its complete environment
/// configuration is present. A deployment without telemetry keeps the
/// existing offline and non-networking behaviour; malformed telemetry config
/// is reported and never prevents a review from running.
fn langfuse_client() -> Option<LangfuseClient> {
    let configured = [
        "LANGFUSE_BASE_URL",
        "LANGFUSE_PUBLIC_KEY",
        "LANGFUSE_SECRET_KEY",
    ]
    .iter()
    .any(|name| std::env::var(name).is_ok_and(|value| !value.trim().is_empty()));
    if !configured {
        return None;
    }

    match LangfuseClient::from_env() {
        Ok(client) => Some(client),
        Err(err) => {
            tracing::warn!(%err, "Langfuse telemetry is configured but unusable; continuing without it");
            None
        }
    }
}

#[async_trait]
impl Model for GatewayModel {
    async fn complete(&self, request: ModelRequest) -> Result<ModelResponse> {
        let mut last = match self.call_until_complete(&request.model, &request).await {
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
            match self.call_until_complete(fallback, &request).await {
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
            structured_output: StructuredOutput::Schema,
            gateway: "openrouter".into(),
            base_url: "https://openrouter.ai/api/v1".into(),
            api_key_env: "TINYSWEEPER_TEST_KEY_ABSENT".into(),
            scan: "a".into(),
            deep: "b".into(),
            flash: "c".into(),
            fallback: vec![],
            provider: ProviderRouting::default(),
            max_tokens: 100,
            budget_usd_per_pr: 1.0,
        }
    }

    fn pinned(order: &[&str], allow_fallbacks: bool) -> ProviderRouting {
        ProviderRouting {
            order: order.iter().map(|p| (*p).to_string()).collect(),
            allow_fallbacks,
        }
    }

    #[test]
    fn an_unpinned_config_sends_no_provider_block() {
        // Absence matters: sending `provider: {order: []}` is not the same as
        // sending nothing, and the empty list is the shape that would pin the
        // gateway to no provider at all.
        let options = provider_options("high", &ProviderRouting::default());
        assert!(options.get("provider").is_none());
        assert_eq!(options["reasoning"]["effort"], json!("high"));
    }

    #[test]
    fn the_pin_and_the_reasoning_block_both_survive_the_merge() {
        // The regression this guards: both are top-level body keys set through
        // one `with_default_provider_options` call, so building them
        // separately silently drops whichever is written second.
        let options = provider_options("high", &pinned(&["deepseek"], false));

        assert_eq!(options["reasoning"]["effort"], json!("high"));
        assert_eq!(options["provider"]["order"], json!(["deepseek"]));
        assert_eq!(options["provider"]["allow_fallbacks"], json!(false));
    }

    #[test]
    fn a_pin_survives_reasoning_being_turned_off() {
        // `off` takes a different branch of `reasoning_options`, which returns a
        // differently-shaped object. The pin has to ride along on both.
        let options = provider_options("off", &pinned(&["deepseek"], false));

        assert_eq!(options["reasoning"]["enabled"], json!(false));
        assert_eq!(options["provider"]["order"], json!(["deepseek"]));
    }

    #[test]
    fn blank_provider_names_are_dropped_rather_than_sent() {
        // With `allow_fallbacks = false` an unmatchable name is not a cosmetic
        // problem: it fails every request the deployment makes.
        let options = provider_options("high", &pinned(&["", "  ", "deepseek"], false));
        assert_eq!(options["provider"]["order"], json!(["deepseek"]));
    }

    #[test]
    fn a_routing_block_of_only_blanks_counts_as_unpinned() {
        assert!(pinned(&["", "   "], false).is_empty());
        assert!(
            provider_options("high", &pinned(&[""], false))
                .get("provider")
                .is_none()
        );
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
    fn schema_mode_leaves_the_conversation_alone() {
        // Under `schema` the provider holds the schema, so adding it to the
        // prompt as well would spend input tokens on every call of every lane
        // to say something the wire format already said.
        let mut req = request(100);
        req.messages = vec![CrateMessage::system("review this")];

        let wire = wire_messages(&req, StructuredOutput::Schema);

        assert_eq!(wire.len(), 1, "schema mode must not touch the prompt");
    }

    #[test]
    fn json_object_mode_carries_the_schema_in_the_prompt() {
        // The other half of the same decision. `ResponseFormat::JsonObject`
        // tells the provider "any json object", so the shape has to arrive in
        // the prompt or the model is guessing at the contract.
        let mut req = request(100);
        req.messages = vec![CrateMessage::system("review this")];
        req.schema = crate::harness::schema::json_schema();

        let wire = wire_messages(&req, StructuredOutput::JsonObject);

        assert_eq!(wire.len(), 2, "the schema instruction must be appended");
        assert!(matches!(wire[1].role, Role::System));
        assert!(
            wire[1].content.contains("existing_code"),
            "the appended message must actually carry the schema"
        );
    }

    #[test]
    fn the_json_mode_instruction_says_the_word_json() {
        // Not a style assertion. DeepSeek's JSON mode **rejects** a request
        // whose prompt never says "json", so a well-meaning reword that drops
        // the word turns every call into a 400 — and the fallback chain then
        // hides it behind a working review from a different model.
        let text = crate::harness::schema::json_mode_instruction(&json!({"type": "object"}));

        assert!(
            text.to_lowercase().contains("json"),
            "DeepSeek's JSON mode requires the literal word in the prompt"
        );
    }

    #[test]
    fn the_configured_ceiling_reaches_the_run_the_provider_is_called_from() {
        // `models.max_tokens` was accepted, validated, documented as the
        // ceiling on a response — and then dropped on the floor, so the
        // provider's own default decided how long an answer could get.
        assert_eq!(
            run_config(request(4_096).max_tokens).max_turn_output_tokens,
            Some(4_096)
        );
    }

    #[test]
    fn a_zero_ceiling_is_not_forwarded() {
        // `config::validate` rejects `max_tokens = 0`, but a `Config` built in
        // code can still carry it, and asking a provider for zero output tokens
        // turns a configuration mistake into an empty answer on every lane.
        assert_eq!(
            run_config(request(0).max_tokens).max_turn_output_tokens,
            None
        );
    }

    #[test]
    fn every_request_asks_the_gateway_for_the_cost_it_charged() {
        // Without this the only cost figure in the whole system is the rate
        // table's estimate, and `budget_usd_per_pr` stops a real bill on it.
        let options = provider_options("high", &ProviderRouting::default());
        assert_eq!(options["usage"], json!({ "include": true }));
        // The reasoning block is still there: the two travel in one object and
        // an overwrite would silently un-configure `reasoning_effort`.
        assert_eq!(options["reasoning"], json!({ "effort": "high" }));
    }

    #[test]
    fn the_reported_cost_is_read_out_of_the_raw_body() {
        let raw = json!({ "usage": { "cost": 0.0123, "prompt_tokens": 10 } });
        assert_eq!(gateway_cost(Some(&raw)), Some(0.0123));
    }

    #[test]
    fn a_gateway_that_reports_no_cost_leaves_the_estimate_standing() {
        // Every gateway other than OpenRouter, and OpenRouter itself on an
        // endpoint that does not honour `usage.include`.
        assert_eq!(gateway_cost(None), None);
        assert_eq!(gateway_cost(Some(&json!({ "usage": {} }))), None);
        assert_eq!(gateway_cost(Some(&json!({}))), None);
    }

    #[test]
    fn a_nonsensical_reported_cost_is_disbelieved() {
        // A negative cost would credit the per-pull-request budget instead of
        // spending it, which turns a hard stop into no stop at all.
        assert_eq!(
            gateway_cost(Some(&json!({ "usage": { "cost": -1.0 } }))),
            None
        );
        assert_eq!(
            gateway_cost(Some(&json!({ "usage": { "cost": "0.01" } }))),
            None
        );
    }

    #[test]
    fn a_truncated_answer_is_retried_at_a_larger_ceiling() {
        // The failure this ladder exists for: an answer cut off part way
        // through the findings array is closed by the harness' repair ladder
        // and parses cleanly, so it reads exactly like a review that found
        // fewer things. Growing the ceiling is the only fix that keeps the
        // findings; four rungs of it would just be slow.
        assert_eq!(truncation_ladder(16_000), vec![16_000, 32_000, 64_000]);
    }

    #[test]
    fn a_ceiling_that_would_overflow_stops_growing_rather_than_wrapping() {
        // Saturating, not wrapping: a wrapped ceiling asks the provider for
        // almost no output and turns one truncated review into a guaranteed
        // empty one.
        let ladder = truncation_ladder(u32::MAX);
        assert_eq!(ladder, vec![u32::MAX; 3]);
    }

    #[test]
    fn an_absent_ceiling_is_a_single_rung() {
        // Zero means the ceiling was never forwarded, so a truncation came from
        // the provider's own limit and doubling nothing would just spend three
        // calls to reach the same answer.
        assert_eq!(truncation_ladder(0), vec![0]);
    }

    #[test]
    fn debug_never_prints_the_api_key() {
        let model = GatewayModel {
            reasoning_effort: "high".into(),
            structured_output: StructuredOutput::Schema,
            api_key: "sk-secret-value".into(),
            base_url: "https://openrouter.ai/api/v1".into(),
            fallbacks: vec![],
            provider: ProviderRouting::default(),
            langfuse: None,
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
            provider: models.provider.clone(),
            structured_output: models.structured_output,
            langfuse: None,
        };

        assert_eq!(
            reasoning_options(&gateway.reasoning_effort),
            json!({ "reasoning": { "enabled": false } })
        );
    }
}
