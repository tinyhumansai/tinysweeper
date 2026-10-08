//! Langfuse export for model calls, behind the `harness` feature.
//!
//! One trace and one `generation-create` per [`Completer`] call, sent to
//! Langfuse's `/api/public/ingestion` batch API. Configured entirely from the
//! environment, with the same variables the tinyagents exporter read, so a
//! deployment's telemetry survives the harness change untouched:
//!
//! - direct: `LANGFUSE_BASE_URL`, `LANGFUSE_PUBLIC_KEY`, `LANGFUSE_SECRET_KEY`
//!   (Basic auth);
//! - proxied: `TINYHUMANS_LANGFUSE_PROXY_URL` plus `TINYHUMANS_AUTH_TOKEN`
//!   (Bearer auth), which takes precedence when set;
//! - `LANGFUSE_ENVIRONMENT` tags every trace.
//!
//! Export is fire-and-forget on the ambient tokio runtime: telemetry must never
//! slow or fail a review, so a send error is logged at `warn` and dropped.
//!
//! [`Completer`]: openhuman_embed::complete::Completer

use serde_json::{Value, json};

use openhuman_embed::complete::{CompletionObserver, CompletionTrace};

/// How the exporter authenticates.
#[derive(Clone)]
enum Auth {
    Basic {
        public_key: String,
        secret_key: String,
    },
    Bearer {
        token: String,
    },
}

/// Sends each completion to Langfuse.
#[derive(Clone)]
pub struct LangfuseExporter {
    client: reqwest::Client,
    endpoint: String,
    auth: Auth,
    environment: Option<String>,
}

impl std::fmt::Debug for LangfuseExporter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LangfuseExporter")
            .field("endpoint", &self.endpoint)
            .field("auth", &"<redacted>")
            .finish()
    }
}

fn env(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// `base` with the ingestion path appended unless it already ends in one.
fn normalize(base: &str, suffix: &str) -> String {
    let trimmed = base.trim().trim_end_matches('/');
    if trimmed.ends_with("/api/public/ingestion")
        || trimmed.ends_with("/telemetry/langfuse/ingestion")
    {
        trimmed.to_string()
    } else {
        format!("{trimmed}{suffix}")
    }
}

impl LangfuseExporter {
    /// The exporter the environment describes, or `None` when telemetry is not
    /// configured. A half-configured environment is reported at `warn` and
    /// treated as unconfigured: a typo in a telemetry variable is not a reason
    /// to refuse to review.
    pub fn from_env() -> Option<Self> {
        let environment = env("LANGFUSE_ENVIRONMENT");
        if let Some(proxy) = env("TINYHUMANS_LANGFUSE_PROXY_URL") {
            let Some(token) = env("TINYHUMANS_AUTH_TOKEN") else {
                tracing::warn!(
                    "TINYHUMANS_LANGFUSE_PROXY_URL is set without TINYHUMANS_AUTH_TOKEN; Langfuse export is off"
                );
                return None;
            };
            return Some(Self::new(
                normalize(&proxy, "/telemetry/langfuse/ingestion"),
                Auth::Bearer { token },
                environment,
            ));
        }
        let names = [
            "LANGFUSE_BASE_URL",
            "LANGFUSE_PUBLIC_KEY",
            "LANGFUSE_SECRET_KEY",
        ];
        let values: Vec<Option<String>> = names.iter().map(|name| env(name)).collect();
        match values.as_slice() {
            [Some(base), Some(public_key), Some(secret_key)] => Some(Self::new(
                normalize(base, "/api/public/ingestion"),
                Auth::Basic {
                    public_key: public_key.clone(),
                    secret_key: secret_key.clone(),
                },
                environment,
            )),
            _ if values.iter().any(Option::is_some) => {
                tracing::warn!(
                    "Langfuse is partly configured (need LANGFUSE_BASE_URL, LANGFUSE_PUBLIC_KEY and LANGFUSE_SECRET_KEY); export is off"
                );
                None
            }
            _ => None,
        }
    }

    fn new(endpoint: String, auth: Auth, environment: Option<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
            endpoint,
            auth,
            environment,
        }
    }

    async fn send(&self, payload: Value) {
        let request = self.client.post(&self.endpoint).json(&payload);
        let request = match &self.auth {
            Auth::Basic {
                public_key,
                secret_key,
            } => request.basic_auth(public_key, Some(secret_key)),
            Auth::Bearer { token } => request.bearer_auth(token),
        };
        match request.send().await {
            // 207 is Langfuse's per-item report; anything else non-2xx is a
            // rejected batch.
            Ok(response) if response.status().is_success() => {}
            Ok(response) => {
                tracing::warn!(status = %response.status(), "Langfuse rejected a model-call trace");
            }
            Err(err) => tracing::warn!(%err, "could not export model call to Langfuse"),
        }
    }
}

/// The ingestion batch for one completion: a trace and its generation.
pub(crate) fn ingestion_batch(
    trace: &CompletionTrace<'_>,
    environment: Option<&str>,
    trace_id: &str,
    end: std::time::SystemTime,
) -> Value {
    let end_ms = end
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64;
    let start_ms = end_ms - trace.latency.as_millis() as i64;
    let iso = |ms: i64| {
        let secs = ms.div_euclid(1000);
        let millis = ms.rem_euclid(1000);
        // RFC 3339 without a date crate: days from the epoch, then civil date.
        let days = secs.div_euclid(86_400);
        let rem = secs.rem_euclid(86_400);
        let (y, m, d) = civil_from_days(days);
        format!(
            "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{millis:03}Z",
            rem / 3600,
            (rem % 3600) / 60,
            rem % 60
        )
    };
    let request = trace.request;
    let (output, usage, model, level, status) = match trace.outcome {
        Ok(response) => (
            json!(response.text),
            response.usage.as_ref().map(|usage| {
                json!({
                    "input": usage.input_tokens,
                    "output": usage.output_tokens,
                    "total": usage.input_tokens + usage.output_tokens,
                    "unit": "TOKENS",
                    "totalCost": usage.cost_usd,
                })
            }),
            response
                .answered_model
                .clone()
                .unwrap_or_else(|| request.model.clone()),
            "DEFAULT",
            None,
        ),
        Err(err) => (
            Value::Null,
            None,
            request.model.clone(),
            "ERROR",
            Some(err.to_string()),
        ),
    };
    let metadata = json!({
        "requested_model": request.model,
        "max_tokens": request.max_tokens,
        "finish_reason": trace.outcome.ok().and_then(|r| r.finish_reason.clone()),
    });
    let mut generation = json!({
        "id": format!("{trace_id}-generation"),
        "traceId": trace_id,
        "name": "model",
        "model": model,
        "startTime": iso(start_ms),
        "endTime": iso(end_ms),
        "input": request.messages,
        "output": output,
        "level": level,
        "metadata": metadata,
    });
    if let Some(usage) = usage {
        generation["usage"] = usage;
    }
    if let Some(status) = status {
        generation["statusMessage"] = json!(status);
    }
    let mut trace_body = json!({
        "id": trace_id,
        "name": "tinysweeper model call",
        "timestamp": iso(start_ms),
    });
    if let Some(environment) = environment {
        trace_body["environment"] = json!(environment);
    }
    json!({
        "batch": [
            {"id": format!("{trace_id}-trace"), "timestamp": iso(start_ms), "type": "trace-create", "body": trace_body},
            {"id": format!("{trace_id}-gen"), "timestamp": iso(end_ms), "type": "generation-create", "body": generation},
        ]
    })
}

/// Howard Hinnant's days-to-civil algorithm.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

impl CompletionObserver for LangfuseExporter {
    fn on_complete(&self, trace: &CompletionTrace<'_>) {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let seq = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let now = std::time::SystemTime::now();
        let nanos = now
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let trace_id = format!("tinysweeper-{nanos:x}-{seq}");
        let payload = ingestion_batch(trace, self.environment.as_deref(), &trace_id, now);
        let exporter = self.clone();
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                handle.spawn(async move { exporter.send(payload).await });
            }
            Err(_) => tracing::debug!("no tokio runtime; Langfuse export skipped"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use openhuman_embed::complete::{
        ChatMessage, CompletionRequest, CompletionResponse, CompletionUsage,
    };
    use std::time::{Duration, UNIX_EPOCH};

    fn response() -> CompletionResponse {
        CompletionResponse {
            text: "{\"ok\":true}".into(),
            structured: Some(json!({"ok": true})),
            finish_reason: Some("stop".into()),
            answered_model: Some("vendor/answered".into()),
            usage: Some(CompletionUsage {
                input_tokens: 10,
                output_tokens: 5,
                cached_tokens: 0,
                reasoning_tokens: 0,
                cost_usd: Some(0.01),
            }),
            raw: None,
        }
    }

    #[test]
    fn a_successful_call_becomes_a_trace_and_a_priced_generation() {
        let request = CompletionRequest::new("vendor/requested", vec![ChatMessage::user("hi")]);
        let response = response();
        let trace = CompletionTrace {
            request: &request,
            outcome: Ok(&response),
            latency: Duration::from_millis(1500),
        };
        let end = UNIX_EPOCH + Duration::from_millis(1_700_000_001_500);
        let batch = ingestion_batch(&trace, Some("prod"), "t1", end);
        let items = batch["batch"].as_array().unwrap();
        assert_eq!(items[0]["type"], "trace-create");
        assert_eq!(items[0]["body"]["environment"], "prod");
        let generation = &items[1]["body"];
        assert_eq!(items[1]["type"], "generation-create");
        assert_eq!(generation["traceId"], "t1");
        assert_eq!(generation["model"], "vendor/answered");
        assert_eq!(generation["usage"]["totalCost"], 0.01);
        assert_eq!(generation["startTime"], "2023-11-14T22:13:20.000Z");
        assert_eq!(generation["endTime"], "2023-11-14T22:13:21.500Z");
        assert_eq!(
            generation["metadata"]["requested_model"],
            "vendor/requested"
        );
    }

    #[test]
    fn a_failed_call_is_an_error_level_generation() {
        let request = CompletionRequest::new("m", vec![ChatMessage::user("hi")]);
        let err = openhuman_embed::CoreError::Rpc {
            method: "openhuman.complete",
            message: "boom".into(),
        };
        let trace = CompletionTrace {
            request: &request,
            outcome: Err(&err),
            latency: Duration::ZERO,
        };
        let batch = ingestion_batch(&trace, None, "t2", UNIX_EPOCH);
        let generation = &batch["batch"][1]["body"];
        assert_eq!(generation["level"], "ERROR");
        assert!(
            generation["statusMessage"]
                .as_str()
                .unwrap()
                .contains("boom")
        );
        assert!(batch["batch"][0]["body"].get("environment").is_none());
    }

    #[test]
    fn endpoints_are_normalized_once() {
        assert_eq!(
            normalize("https://lf.example/", "/api/public/ingestion"),
            "https://lf.example/api/public/ingestion"
        );
        assert_eq!(
            normalize(
                "https://lf.example/api/public/ingestion",
                "/api/public/ingestion"
            ),
            "https://lf.example/api/public/ingestion"
        );
    }

    #[test]
    fn civil_dates_are_right_across_a_leap_day() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_782), (2024, 2, 29));
    }
}
