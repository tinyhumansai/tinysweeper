//! The real model, behind the `harness` feature.
//!
//! A thin adapter over OpenHuman's stateless completion ladder, with bounded
//! truncation retries against an OpenAI-compatible gateway. OpenRouter,
//! Moonshot and MiniMax all speak the same wire format, so pointing `base_url`
//! elsewhere is the whole of "switching provider" — there is no second code
//! path to maintain and no provider-specific SDK in the tree.
//!
//! Council reviewers can opt into Embed agent turns with host-owned read-only
//! repository tools. Both structured modes require the complete terminal answer
//! to be valid JSON; trailing prose is refused. Provider credentials remain inside
//! the adapter, and write
//! credentials never enter this model capability.

use std::sync::Arc;

use async_trait::async_trait;
use openhuman_embed::Route;
use openhuman_embed::complete::{ChatMessage, Completer, CompletionRequest, ResponseFormat};
use openhuman_embed::routing::{CompletionLadder, CompletionRung, TruncationRetry};
use serde_json::json;

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
#[derive(Clone)]
pub struct GatewayModel {
    budget: Option<openhuman_embed::budget::Budget>,
    budget_admission: Option<Arc<tokio::sync::Mutex<()>>>,
    budget_prices: std::collections::BTreeMap<String, crate::config::types::BudgetPriceBound>,
    agentic_reviewers: bool,
    api_key: String,
    base_url: String,
    fallbacks: Vec<String>,
    reasoning_effort: String,
    provider: ProviderRouting,
    routes: Vec<crate::config::types::ModelRoute>,
    structured_output: StructuredOutput,
    langfuse: Option<Arc<crate::harness::langfuse::LangfuseExporter>>,
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
/// OpenAI wire shape has no cost field — this one is
/// OpenRouter's extension, returned because [`provider_options`] asked for it.
/// `None` means the gateway reported nothing and the estimate stands.
fn gateway_cost(raw: Option<&serde_json::Value>) -> Option<f64> {
    let usage = raw?.get("usage")?;
    // Two spellings, from the two marketplaces the ladder dispatches to.
    // OpenRouter reports `cost` in dollars; Surplus reports
    // `buyer_cost_micro`, an integer count of micro-dollars, and reports it
    // through the ladder verbatim — the router returns the upstream body as
    // it came. Read either, so a call the ladder sent to Surplus is billed
    // at what it cost rather than at the price table's fallback rate.
    //
    // Surplus first. Some of its sellers relay an OpenRouter-shaped usage
    // block alongside their own, and in it `cost` is `0` with
    // `is_byok: true` — the seller's key paid upstream, not ours — while
    // `buyer_cost_micro` is what Surplus bills. Measured on
    // `deepseek-v4-flash` via Alibaba: `cost: 0`, `buyer_cost_micro: 1`.
    // Reading `cost` first billed every flash call as free.
    let cost = match usage
        .get("buyer_cost_micro")
        .and_then(serde_json::Value::as_f64)
    {
        Some(micro) => micro / 1_000_000.0,
        None => usage.get("cost")?.as_f64()?,
    };
    // A gateway that reports a nonsensical cost is a gateway to disbelieve: a
    // negative figure would credit the budget rather than spend it.
    (cost.is_finite() && cost >= 0.0).then_some(cost)
}

/// Whether reasoning took more than half of the output ceiling.
///
/// No ceiling, no budget to consume half of: a routed model with
/// `max_tokens = 0` would otherwise be warned about on every call.
fn reasoning_crowds_the_answer(cap: u32, reasoning_tokens: u64) -> bool {
    // Divided rather than doubled: the count is the provider's and unbounded.
    cap != 0 && reasoning_tokens > u64::from(cap) / 2
}

/// The model the gateway says answered, when it says so.
///
/// A ladder is asked for `deep` and answers with whichever model it
/// dispatched to — the router returns the upstream body as it came, so
/// `model` is `gpt-5.6-luna` or `deepseek/deepseek-v4-flash`, not the alias.
/// That is the name the spend ledger, the check summary and a recorded
/// cassette should carry: a bill attributed to `deep` says nothing about
/// what was bought. `None` means the body named nothing usable and the
/// requested name stands.
fn answered_model(raw: Option<&serde_json::Value>) -> Option<&str> {
    let model = raw?.get("model")?.as_str()?.trim();
    (!model.is_empty()).then_some(model)
}

/// One crate message as the completion API wants it.
///
/// Images ride on user messages only: the OpenAI-compatible format has no
/// image part for the other roles. `Message::user_with_images` is the only
/// constructor that sets images, so the other arms never see any.
fn wire_message(m: &CrateMessage) -> ChatMessage {
    match m.role {
        Role::System => ChatMessage::system(&m.content),
        Role::Assistant => ChatMessage::assistant(&m.content),
        Role::User => m
            .images
            .iter()
            .fold(ChatMessage::user(&m.content), |message, url| {
                message.with_image(url.clone())
            }),
    }
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

        Ok(Self::with_key(models, api_key))
    }

    /// Build from the `[models]` config with the key already in hand.
    ///
    /// For tests that drive the adapter against a fake gateway: reading the
    /// key from a variable they would have to set means mutating the process
    /// environment, which races every other test in the binary.
    pub(crate) fn with_key(models: &Models, api_key: String) -> Self {
        Self {
            api_key,
            budget: None,
            budget_admission: None,
            budget_prices: models.budget_prices.clone(),
            agentic_reviewers: models.agentic_reviewers,
            base_url: models.base_url.clone(),
            fallbacks: models.fallback.clone(),
            reasoning_effort: models.reasoning_effort.clone(),
            provider: models.provider.clone(),
            routes: models.routes.clone(),
            structured_output: models.structured_output,
            langfuse: langfuse_client(),
        }
    }

    /// A gateway for calls that carry images.
    ///
    /// Not the review ladder. That ladder shares one provider pin across the
    /// primary and every fallback, and the pin names the hosts that serve the
    /// *text* models — a vision model is usually not among them, so the
    /// primary 404s, and each fallback is then a text model handed
    /// `image_url` parts it cannot see, which answers with a confident caption
    /// of a picture it never looked at. So: no fallbacks, and no pin — the
    /// gateway routes the one model wherever it is served. The price is an
    /// estimate rather than a pinned rate, and the caption is decoration, so
    /// that is the right trade here and the wrong one for a review.
    pub fn for_vision(models: &Models) -> Result<Self> {
        let mut gateway = Self::from_config(models)?;
        gateway.fallbacks = vec![];
        gateway.provider = ProviderRouting::unpinned();
        // A per-model route would outrank the unpinned routing above, and a
        // route written for the model's text endpoint pins the image call to
        // a host that cannot see the picture.
        gateway.routes = vec![];
        Ok(gateway)
    }

    /// The completer for one call, carrying this deployment's key and base
    /// URL and the gateway attribution headers.
    fn completer(&self) -> Completer {
        let completer = Completer::new(Route::openai_compatible(&self.base_url, &self.api_key))
            // Identifies us to OpenRouter, which is how per-application usage
            // shows up separately in their dashboard.
            .header(
                "HTTP-Referer",
                "https://github.com/tinyhumansai/tinysweeper",
            )
            .header("X-Title", "tinysweeper");
        match &self.langfuse {
            Some(observer) => completer.observer(observer.clone()),
            None => completer,
        }
    }

    /// Build the immutable prompt shared by all completion routes.
    fn completion_request(&self, request: &ModelRequest) -> CompletionRequest {
        let format = match self.structured_output {
            StructuredOutput::Schema => ResponseFormat::JsonSchema {
                name: request.schema_name.clone(),
                schema: request.schema.clone(),
            },
            StructuredOutput::JsonObject => ResponseFormat::JsonObject,
        };
        let messages = wire_messages(request, self.structured_output)
            .iter()
            .map(wire_message)
            .collect();
        CompletionRequest::new(&request.model, messages).response_format(format)
    }

    /// Configure one route; the owner ladder handles every physical dispatch.
    fn completion_rung(
        &self,
        model: &str,
        request: &ModelRequest,
        completion: &CompletionRequest,
        unpinned: bool,
    ) -> Result<CompletionRung> {
        let base = self
            .routes
            .iter()
            .find(|route| route.model == model)
            .and_then(|route| route.max_tokens)
            .unwrap_or(request.max_tokens);
        let routing = if unpinned {
            std::borrow::Cow::Owned(ProviderRouting::unpinned())
        } else {
            self.routing_for(model)
        };
        let options = provider_options(&self.reasoning_effort, &routing);
        let mut completer = self.completer();
        if let Some(ledger) = &self.budget {
            let mut bound = completion.clone();
            bound.model = model.to_owned();
            bound.provider_options = options.clone();
            // Every dispatch reserves the largest possible output on this
            // route. Smaller attempts retain their original wire caps; the
            // shared ledger settles their reported spend after each response.
            bound.max_tokens = (base != 0).then(|| *truncation_ladder(base).last().unwrap());
            completer = completer.budget(crate::harness::budget::call(
                ledger,
                &bound,
                0,
                &self.budget_prices,
            )?);
        }
        let rung = CompletionRung::new(completer, model)
            .provider_options(options)
            .max_tokens((base != 0).then_some(base));
        Ok(if unpinned { rung.unpinned() } else { rung })
    }

    async fn complete_ladder(&self, request: &ModelRequest) -> Result<ModelResponse> {
        // Reserve only after the preceding paid call settles. Fan-out must not
        // turn temporary worst-case reservations into a permanent refusal of
        // cheap work. Dropping the future releases admission on cancellation.
        let _admission = self.admit_paid_work().await;
        let completion = self.completion_request(request);
        let mut ladder = CompletionLadder::new(self.completion_rung(
            &request.model,
            request,
            &completion,
            false,
        )?)
        .truncation_retry(TruncationRetry::new(MAX_TRUNCATION_RETRIES as u8, u32::MAX));
        for model in &self.fallbacks {
            ladder = ladder.fallback(self.completion_rung(model, request, &completion, false)?);
        }
        let primary = self.routing_for(&request.model);
        if primary.last_resort_unpinned && !primary.is_empty() {
            ladder = ladder.fallback(self.completion_rung(
                &request.model,
                request,
                &completion,
                true,
            )?);
        }
        let result = ladder.complete(completion).await.map_err(|error| {
            // Keep the operator's actionable ceiling diagnostic when every
            // bounded attempt is cut off. Other refusal types retain their
            // owner error instead of being interpreted from message text.
            let refusal = if let Some(attempt) = error.attempts.last()
                && matches!(attempt.finish_reason.as_deref(), Some("length" | "max_tokens" | "MAX_TOKENS"))
            {
                let model = &attempt.requested_model;
                let routed = self.routes.iter().find(|route| &route.model == model)
                    .and_then(|route| route.max_tokens);
                let base = routed.unwrap_or(request.max_tokens);
                let key = if routed.is_some() {
                    format!("`models.routes[{model}].max_tokens`")
                } else {
                    "`models.max_tokens`".to_owned()
                };
                let usage = attempt.usage.clone().unwrap_or_default();
                Error::Model(format!(
                    "{model} ran out of output tokens at {} ({} generated, {} of them reasoning); the answer was cut off. Raise {key} (currently {base}) or lower `models.reasoning_effort`.",
                    attempt.max_tokens.unwrap_or_default(), usage.output_tokens, usage.reasoning_tokens,
                ))
            } else {
                Error::Model(error.to_string())
            };
            // Structured refusals can arrive after several billed dispatches.
            // Preserve their safe owner metadata for lane and budget accounting,
            // pricing omitted charges against the model that actually answered.
            if let Some(totals) = error.total_usage {
                let cost_usd = error.attempts.iter().filter_map(|attempt| {
                    let usage = attempt.usage.as_ref()?;
                    let model = attempt.answered_model.as_deref()
                        .unwrap_or(&attempt.requested_model);
                    Some(usage.cost_usd.unwrap_or_else(|| pricing::completion_cost(
                        model, usage.input_tokens, usage.cached_tokens, usage.output_tokens,
                    )))
                }).sum();
                refusal.with_usage(Usage {
                    input_tokens: totals.input_tokens,
                    output_tokens: totals.output_tokens,
                    cached_tokens: totals.cached_tokens,
                    embed_tokens: 0,
                    cost_usd,
                })
            } else {
                refusal
            }
        })?;
        let response = result.response;
        let answered = response
            .answered_model
            .as_deref()
            .or_else(|| answered_model(response.raw.as_ref()))
            .unwrap_or(&request.model);
        let value = response.structured.ok_or_else(|| {
            Error::Model(format!(
                "{answered} returned no structured output; the response did not satisfy the schema"
            ))
        })?;
        let totals = result.total_usage.unwrap_or_default();
        let last = result.attempts.len().saturating_sub(1);
        let mut cost = 0.0;
        for (index, attempt) in result.attempts.iter().enumerate() {
            let usage = attempt.usage.clone().unwrap_or_default();
            let model = attempt
                .answered_model
                .as_deref()
                .unwrap_or(&attempt.requested_model);
            let reported = if index == last {
                gateway_cost(response.raw.as_ref()).or(usage.cost_usd)
            } else {
                usage.cost_usd
            };
            cost += reported.unwrap_or_else(|| {
                pricing::completion_cost(
                    model,
                    usage.input_tokens,
                    usage.cached_tokens,
                    usage.output_tokens,
                )
            });
            tracing::info!(model, cap = attempt.max_tokens, input_tokens = usage.input_tokens,
                cached_tokens = usage.cached_tokens, output_tokens = usage.output_tokens,
                reasoning_tokens = usage.reasoning_tokens, finish_reason = ?attempt.finish_reason,
                reported_cost_usd = reported, "model call");
            if reasoning_crowds_the_answer(
                attempt.max_tokens.unwrap_or_default(),
                usage.reasoning_tokens,
            ) {
                tracing::warn!(
                    model,
                    "reasoning consumed over half the output budget; consider raising `models.max_tokens` or lowering `models.reasoning_effort`"
                );
            }
        }
        Ok(ModelResponse {
            value,
            model: answered.to_owned(),
            usage: Usage {
                input_tokens: totals.input_tokens,
                output_tokens: totals.output_tokens,
                cached_tokens: totals.cached_tokens,
                embed_tokens: 0,
                cost_usd: cost,
            },
        })
    }

    /// Serial admission for one monetary ledger; unscoped calls do not queue.
    async fn admit_paid_work(&self) -> Option<tokio::sync::MutexGuard<'_, ()>> {
        match &self.budget_admission {
            Some(admission) => Some(admission.lock().await),
            None => None,
        }
    }

    /// Run one isolated reviewer on its effective model route.
    async fn agentic_attempt(
        &self,
        model: &str,
        request: &ModelRequest,
        tree: &dyn crate::ports::tree::TreeReader,
        policy: &crate::config::types::LookupPolicy,
        routing: &ProviderRouting,
        lookup_budget: &Arc<crate::harness::agentic::LookupBudget>,
    ) -> std::result::Result<ModelResponse, crate::harness::agentic::ReviewFailure> {
        let mut request = request.clone();
        request.model = model.to_owned();
        request.max_tokens = self
            .routes
            .iter()
            .find(|route| route.model == model)
            .and_then(|route| route.max_tokens)
            .unwrap_or(request.max_tokens);
        let messages = wire_messages(&request, self.structured_output)
            .iter()
            .map(wire_message)
            .collect();
        let mut completion = CompletionRequest::new(&request.model, messages).response_format(
            ResponseFormat::JsonSchema {
                name: request.schema_name.clone(),
                schema: request.schema.clone(),
            },
        );
        completion.max_tokens = Some(request.max_tokens);
        completion.provider_options = provider_options(&self.reasoning_effort, routing);
        let budget = self
            .budget
            .as_ref()
            .map(|ledger| {
                crate::harness::budget::call(
                    ledger,
                    &completion,
                    (policy.max_chars as u64)
                        .saturating_mul(12)
                        .saturating_add(32768),
                    &self.budget_prices,
                )
            })
            .transpose()?;
        let provider = openhuman_embed::Provider::openai_compatible(&self.base_url, &self.api_key)
            .model(&request.model);
        crate::harness::agentic::review_accounted(
            request,
            tree,
            policy,
            provider,
            provider_options(&self.reasoning_effort, routing),
            crate::harness::agentic::ReviewResources {
                model_budget: budget,
                lookup_budget: lookup_budget.clone(),
            },
            self.langfuse.as_ref().map(|observer| {
                observer.clone() as Arc<dyn openhuman_embed::observe::TurnObserver>
            }),
        )
        .await
    }
}

/// How many times a truncated answer is retried with a doubled ceiling before
/// the call fails. Two retries means the last attempt runs at 4x
/// `models.max_tokens`.
const MAX_TRUNCATION_RETRIES: u32 = 2;

/// The output ceilings one model is tried at, in order.
///
/// A zero base means "no ceiling" (it is not forwarded): there is nothing to
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

/// The Langfuse exporter, when the environment configures one. A deployment
/// without telemetry keeps the existing offline and non-networking behaviour;
/// malformed telemetry config is reported and never prevents a review from
/// running.
fn langfuse_client() -> Option<Arc<crate::harness::langfuse::LangfuseExporter>> {
    crate::harness::langfuse::LangfuseExporter::from_env().map(Arc::new)
}

impl GatewayModel {
    /// The routing for one call: the model's own route when it has one,
    /// otherwise the ladder-wide pin with the first-party-vendor bypass.
    fn routing_for(&self, model: &str) -> std::borrow::Cow<'_, ProviderRouting> {
        match self.routes.iter().find(|r| r.model == model) {
            Some(route) => std::borrow::Cow::Owned(route.routing()),
            None => self.provider.for_model(model),
        }
    }
}

#[async_trait]
impl Model for GatewayModel {
    fn scoped_budget(&self, budget_usd: f64) -> Option<Arc<dyn Model>> {
        let mut model = self.clone();
        model.budget = Some(crate::harness::budget::ledger(budget_usd));
        // Clones share a ledger and its queue; a fresh scope shares neither.
        model.budget_admission = Some(Arc::new(tokio::sync::Mutex::new(())));
        Some(Arc::new(model))
    }

    async fn review(
        &self,
        request: ModelRequest,
        tree: &dyn crate::ports::tree::TreeReader,
        policy: &crate::config::types::LookupPolicy,
    ) -> Result<ModelResponse> {
        if !self.agentic_reviewers
            || !policy.enabled
            || policy.rounds == 0
            || policy.per_round == 0
            || policy.max_chars == 0
        {
            return self.complete(request).await;
        }
        // Acquire after the complete fallback above: acquiring twice would
        // deadlock non-agentic reviews. Hold admission across tools and route
        // fallbacks so every paid turn belongs to this reviewer alone.
        let _admission = self.admit_paid_work().await;
        let lookup_budget = crate::harness::agentic::LookupBudget::new(policy);
        let mut last = None;
        let mut prior = Usage::default();
        for model in std::iter::once(&request.model).chain(self.fallbacks.iter()) {
            match self
                .agentic_attempt(
                    model,
                    &request,
                    tree,
                    policy,
                    &self.routing_for(model),
                    &lookup_budget,
                )
                .await
            {
                Ok(mut response) => {
                    crate::harness::agentic::accumulate_usage(&mut response.usage, prior);
                    return Ok(response);
                }
                Err(failure) => {
                    if let Some(usage) = failure.usage {
                        crate::harness::agentic::accumulate_usage(&mut prior, *usage);
                    }
                    last = Some(failure.error);
                }
            }
        }
        let primary = self.routing_for(&request.model);
        if primary.last_resort_unpinned && !primary.is_empty() {
            match self
                .agentic_attempt(
                    &request.model,
                    &request,
                    tree,
                    policy,
                    &ProviderRouting::unpinned(),
                    &lookup_budget,
                )
                .await
            {
                Ok(mut response) => {
                    crate::harness::agentic::accumulate_usage(&mut response.usage, prior);
                    return Ok(response);
                }
                Err(failure) => {
                    if let Some(usage) = failure.usage {
                        crate::harness::agentic::accumulate_usage(&mut prior, *usage);
                    }
                    last = Some(failure.error);
                }
            }
        }
        let error = last.expect("the primary model is always attempted");
        if prior != Usage::default() {
            Err(error.with_usage(prior))
        } else {
            Err(error)
        }
    }

    async fn complete(&self, request: ModelRequest) -> Result<ModelResponse> {
        self.complete_ladder(&request).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn models() -> Models {
        Models {
            agentic_reviewers: false,
            reasoning_effort: "high".into(),
            structured_output: StructuredOutput::Schema,
            gateway: "openrouter".into(),
            base_url: "https://openrouter.ai/api/v1".into(),
            api_key_env: "TINYSWEEPER_TEST_KEY_ABSENT".into(),
            scan: "a".into(),
            deep: "b".into(),
            flash: "c".into(),
            fallback: vec![],
            vision: None,
            provider: ProviderRouting::default(),
            routes: Vec::new(),
            max_tokens: 100,
            budget_usd_per_pr: 1.0,
            budget_prices: Default::default(),
        }
    }

    async fn admission_fixture() -> (
        GatewayModel,
        crate::harness::fake_gateway::FakeGateway,
        ModelRequest,
    ) {
        use crate::harness::fake_gateway::{FakeGateway, Reply};
        let gateway = FakeGateway::start(vec![Reply::completion(
            "b",
            r#"{"summary":"checked"}"#,
            "stop",
            json!({"prompt_tokens":5,"completion_tokens":2,"cost":0.0021}),
        )])
        .await;
        let mut config = models();
        config.base_url = gateway.base_url.clone();
        config.budget_prices.insert(
            "b".into(),
            crate::config::types::BudgetPriceBound {
                input: 0.0,
                cached: 0.0,
                output: 150.0,
            },
        );
        let mut model = GatewayModel::with_key(&config, "fixture".into());
        model.budget = Some(crate::harness::budget::ledger(1.0));
        model.budget_admission = Some(Arc::new(tokio::sync::Mutex::new(())));
        let request = ModelRequest {
            model: "b".into(),
            messages: vec![
                CrateMessage::system("Review."),
                CrateMessage::user("Review source."),
            ],
            schema: json!({"type":"object","properties":{"summary":{"type":"string"}},"required":["summary"],"additionalProperties":false}),
            schema_name: "review".into(),
            max_tokens: 100,
        };
        (model, gateway, request)
    }

    #[tokio::test]
    async fn cancelled_queued_completion_does_not_hold_admission_or_dispatch() {
        let (model, gateway, request) = admission_fixture().await;
        let admission = model.budget_admission.as_ref().unwrap().clone();
        let held = admission.lock().await;
        let waiting_model = model.clone();
        let waiting_request = request.clone();
        let waiting = tokio::spawn(async move { waiting_model.complete(waiting_request).await });
        tokio::task::yield_now().await;
        assert!(!waiting.is_finished());
        waiting.abort();
        assert!(waiting.await.unwrap_err().is_cancelled());
        assert!(gateway.requests().is_empty());
        drop(held);
        let response =
            tokio::time::timeout(std::time::Duration::from_secs(2), model.complete(request))
                .await
                .expect("admission released")
                .expect("next completion succeeds");
        assert_eq!(response.value["summary"], "checked");
        assert_eq!(gateway.requests().len(), 1);
    }

    #[tokio::test]
    async fn a_fresh_budget_scope_does_not_wait_for_its_parents_admission() {
        let (model, gateway, request) = admission_fixture().await;
        let admission = model.budget_admission.as_ref().unwrap().clone();
        let _held = admission.lock().await;
        let fresh = model.scoped_budget(1.0).unwrap();
        let response =
            tokio::time::timeout(std::time::Duration::from_secs(2), fresh.complete(request))
                .await
                .expect("fresh scope has its own queue")
                .expect("completion succeeds");
        assert_eq!(response.value["summary"], "checked");
        assert_eq!(gateway.requests().len(), 1);
    }

    fn pinned(order: &[&str], allow_fallbacks: bool) -> ProviderRouting {
        ProviderRouting {
            order: order.iter().map(|p| (*p).to_string()).collect(),
            allow_fallbacks,
            ..ProviderRouting::default()
        }
    }

    #[test]
    fn a_user_message_with_images_becomes_text_then_image_parts() {
        let wired = wire_message(&CrateMessage::user_with_images(
            "what changed?",
            vec!["https://cdn.example/a.png".into()],
        ));
        assert_eq!(
            wired,
            ChatMessage::user("what changed?").with_image("https://cdn.example/a.png")
        );
    }

    #[test]
    fn a_text_only_user_message_is_wired_exactly_as_before() {
        // The single-text shape is what every lane sends and what every
        // recorded cassette was made from; images must not change it.
        assert_eq!(
            wire_message(&CrateMessage::user("plain")),
            ChatMessage::user("plain")
        );
    }

    #[test]
    fn the_last_resort_rung_is_on_by_default() {
        // A pin is only safe to default to because this rung exists. Turning it
        // off by default would restore the failure it was added for: every rung
        // of the ladder inheriting one broken pin.
        assert!(ProviderRouting::default().last_resort_unpinned);
    }

    #[test]
    fn the_last_resort_rung_sends_no_provider_block() {
        // The whole point of the rung: it must not carry the pin that just
        // failed every other rung, or it is a fourth identical attempt.
        let unpinned = ProviderRouting::unpinned();
        assert!(unpinned.is_empty());

        let options = provider_options("high", &unpinned);
        assert!(
            options.get("provider").is_none(),
            "the last rung must route freely: {options}"
        );
    }

    #[test]
    fn the_last_resort_rung_cannot_recurse() {
        // It is reached from `complete` only when `self.provider` is pinned, and
        // the routing it swaps in is unpinned — so the guard is false for it and
        // there is no second last-resort attempt.
        let unpinned = ProviderRouting::unpinned();
        assert!(!(unpinned.last_resort_unpinned && !unpinned.is_empty()));
    }

    #[test]
    fn an_unpinned_deployment_has_no_last_resort_rung_to_take() {
        // Nothing to fall back *from*: the primary call already routed freely,
        // so retrying it unpinned would be a duplicate call on a real outage.
        let routing = ProviderRouting::default();
        assert!(routing.is_empty());
        assert!(!(routing.last_resort_unpinned && !routing.is_empty()));
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
    fn a_routed_rung_gets_its_own_pin_and_ceiling_and_never_the_last_resort() {
        let mut models = models();
        models.provider = pinned(&["streamlake"], false);
        models.routes = vec![crate::config::types::ModelRoute {
            model: "openai/gpt-5.6-luna".into(),
            order: vec!["openai/flex".into()],
            allow_fallbacks: false,
            max_tokens: Some(0),
        }];
        let gateway = GatewayModel {
            budget: None,
            budget_prices: Default::default(),
            agentic_reviewers: false,
            api_key: "unused".into(),
            base_url: models.base_url.clone(),
            fallbacks: models.fallback.clone(),
            reasoning_effort: models.reasoning_effort.clone(),
            provider: models.provider.clone(),
            routes: models.routes.clone(),
            structured_output: models.structured_output,
            langfuse: None,
        };

        let routed = gateway.routing_for("openai/gpt-5.6-luna");
        assert_eq!(routed.order, vec!["openai/flex".to_string()]);
        assert!(!routed.allow_fallbacks);
        assert!(
            !routed.last_resort_unpinned,
            "a named endpoint is not rerouted"
        );
        assert_eq!(
            gateway.routing_for("deepseek/deepseek-v4-flash").order,
            vec!["streamlake".to_string()],
            "an unrouted model keeps the ladder-wide pin"
        );
        assert_eq!(models.max_tokens_for("openai/gpt-5.6-luna"), 0);
        assert_eq!(
            models.max_tokens_for("deepseek/deepseek-v4-flash"),
            models.max_tokens
        );
    }

    #[test]
    fn a_vision_gateway_drops_the_routes_with_the_pin() {
        // `for_vision` reads the key from the environment; build the same
        // gateway by hand and apply the same stripping.
        let mut models = models();
        models.provider = pinned(&["streamlake"], false);
        models.routes = vec![crate::config::types::ModelRoute {
            model: "b".into(),
            order: vec!["text-only-host".into()],
            allow_fallbacks: false,
            max_tokens: None,
        }];
        let mut gateway = GatewayModel {
            budget: None,
            budget_prices: Default::default(),
            agentic_reviewers: false,
            api_key: "unused".into(),
            base_url: models.base_url.clone(),
            fallbacks: vec!["c".into()],
            reasoning_effort: models.reasoning_effort.clone(),
            provider: models.provider.clone(),
            routes: models.routes.clone(),
            structured_output: models.structured_output,
            langfuse: None,
        };
        gateway.fallbacks = vec![];
        gateway.provider = ProviderRouting::unpinned();
        gateway.routes = vec![];
        assert!(
            gateway.routing_for("b").is_empty(),
            "an image call to a routed model must not inherit the text route"
        );
    }

    #[test]
    fn an_uncapped_route_is_never_warned_about_its_reasoning() {
        assert!(reasoning_crowds_the_answer(16_000, 9_000));
        assert!(!reasoning_crowds_the_answer(16_000, 8_000));
        assert!(
            !reasoning_crowds_the_answer(0, 50_000),
            "no ceiling, no half of it"
        );
        assert!(
            reasoning_crowds_the_answer(16_000, u64::MAX),
            "and no overflow"
        );
    }

    #[test]
    fn the_answering_model_is_read_out_of_the_raw_body() {
        // A ladder is asked for an alias and answers with the upstream's
        // body, whose `model` is what actually ran.
        let raw = json!({ "model": "gpt-5.6-luna", "usage": { "buyer_cost_micro": 4 } });
        assert_eq!(answered_model(Some(&raw)), Some("gpt-5.6-luna"));
        assert_eq!(answered_model(Some(&json!({ "model": "  " }))), None);
        assert_eq!(answered_model(Some(&json!({ "model": 7 }))), None);
        assert_eq!(answered_model(Some(&json!({}))), None);
        assert_eq!(answered_model(None), None);
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
    fn a_surplus_micro_dollar_cost_is_read_through_the_ladder() {
        let raw = json!({ "usage": { "buyer_cost_micro": 4, "prompt_tokens": 16 } });
        assert!((gateway_cost(Some(&raw)).unwrap() - 0.000004).abs() < 1e-12);
    }

    #[test]
    fn a_relayed_byok_zero_does_not_hide_what_surplus_bills() {
        // The body a Surplus seller relays from its own upstream: an
        // OpenRouter-shaped `cost: 0` (their key paid) beside the
        // `buyer_cost_micro` we are charged.
        let raw = json!({ "usage": { "cost": 0, "is_byok": true, "buyer_cost_micro": 1 } });
        assert!((gateway_cost(Some(&raw)).unwrap() - 0.000001).abs() < 1e-12);
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
            routes: Vec::new(),
            langfuse: None,
            agentic_reviewers: false,
            budget: None,
            budget_prices: Default::default(),
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
            budget: None,
            budget_prices: Default::default(),
            agentic_reviewers: false,
            api_key: "unused".into(),
            base_url: models.base_url.clone(),
            fallbacks: models.fallback.clone(),
            reasoning_effort: models.reasoning_effort.clone(),
            provider: models.provider.clone(),
            routes: models.routes.clone(),
            structured_output: models.structured_output,
            langfuse: None,
        };

        assert_eq!(
            reasoning_options(&gateway.reasoning_effort),
            json!({ "reasoning": { "enabled": false } })
        );
    }
}
