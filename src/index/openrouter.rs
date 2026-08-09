//! Embeddings through the OpenRouter gateway. Feature `harness`.
//!
//! Why this is a direct client rather than another arm of
//! [`build_model`](crate::index::provider::build_model): tinyagents'
//! `EmbeddingModel::embed` returns bare `Vec<Vec<f32>>`, and every adapter
//! behind it decodes the response body and **discards the `usage` object**.
//! OpenRouter sends one, and it carries both `prompt_tokens` and the actual
//! `cost` it charged. Routing this provider through that trait would throw away
//! the only authoritative number in the response and fall back to
//! `estimate_tokens`, which is four-bytes-per-token guesswork.
//!
//! That matters more here than anywhere else in the program: indexing a
//! repository is the largest token count tinysweeper produces, so it is the
//! line of the bill least well served by an estimate.
//!
//! The provider-reported `cost` is preferred over
//! [`crate::harness::pricing`] when present. The local table is hand-maintained
//! and goes stale silently; the gateway is quoting what it actually billed,
//! including any routing markup, and it cannot disagree with the invoice.
//! The table stays as the fallback for a response that omits `cost`.

use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;

use crate::error::{Error, Result};
use crate::index::types::{EmbedSignature, Embedded};
use crate::ports::embed::Embedder;

/// The gateway's OpenAI-shaped embeddings endpoint.
pub const OPENROUTER_EMBEDDINGS_URL: &str = "https://openrouter.ai/api/v1/embeddings";

/// How long one embedding call may take.
///
/// Generous because a full batch of large chunks against a cold upstream is
/// genuinely slow, and a timeout here costs the whole batch.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

/// Embeddings over OpenRouter.
pub struct OpenRouterEmbedder {
    client: reqwest::Client,
    url: String,
    api_key: String,
    signature: EmbedSignature,
}

// Hand-written so the key cannot reach a log through a derived `Debug`. The
// same reason `GatewayModel` has one.
impl std::fmt::Debug for OpenRouterEmbedder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenRouterEmbedder")
            .field("url", &self.url)
            .field("signature", &self.signature.harness_key())
            .field("api_key", &"<redacted>")
            .finish()
    }
}

impl OpenRouterEmbedder {
    /// Build an embedder for `signature`, reading the key from `api_key_env`.
    ///
    /// `base_url` empty means the gateway's own endpoint. A non-empty value is
    /// taken verbatim so a proxy or a recorded fixture can stand in.
    pub fn new(signature: EmbedSignature, api_key_env: &str, base_url: &str) -> Result<Self> {
        let api_key_env = api_key_env.trim();
        let api_key = std::env::var(api_key_env)
            .ok()
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| {
                Error::config(format!(
                    "{api_key_env} is not set; it holds the API key for the \
                     `openrouter` embedding provider"
                ))
            })?;
        Self::with_key(signature, api_key, base_url)
    }

    /// Build with the key already in hand.
    ///
    /// Split from [`OpenRouterEmbedder::new`] so tests never have to reach for
    /// `std::env::set_var`, which is `unsafe` in Rust 2024 precisely because it
    /// races every other thread reading the environment — and this crate's
    /// tests run in parallel. A test that mutated the variable could make an
    /// unrelated test read a key that was never configured for it, which is a
    /// flake that presents as a security test failing at random.
    pub fn with_key(signature: EmbedSignature, api_key: String, base_url: &str) -> Result<Self> {
        let url = match base_url.trim() {
            "" => OPENROUTER_EMBEDDINGS_URL.to_string(),
            given => given.to_string(),
        };

        let client = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(|err| Error::config(format!("could not build an HTTP client: {err}")))?;

        Ok(Self {
            client,
            url,
            api_key,
            signature,
        })
    }

    /// One request. Split from [`Embedder::embed`] so the batching and the
    /// wire format can be tested apart from each other.
    async fn post(&self, texts: &[String]) -> Result<EmbeddingsResponse> {
        let body = serde_json::json!({
            "model": self.signature.model,
            "input": texts,
        });

        let response = self
            .client
            .post(&self.url)
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await
            .map_err(|err| Error::Model(format!("openrouter embeddings: {err}")))?;

        let status = response.status();
        let text = response
            .text()
            .await
            .map_err(|err| Error::Model(format!("openrouter embeddings: {err}")))?;

        if !status.is_success() {
            // The body is the provider's error message, which names the model
            // and the reason. Truncated because an HTML error page from a proxy
            // is not worth a screenful, and never logged with the key.
            let detail: String = text.chars().take(400).collect();
            return Err(Error::Model(format!(
                "openrouter embeddings returned {status}: {detail}"
            )));
        }

        parse(&text)
    }
}

#[async_trait]
impl Embedder for OpenRouterEmbedder {
    fn signature(&self) -> EmbedSignature {
        self.signature.clone()
    }

    async fn embed(&self, texts: &[String]) -> Result<Embedded> {
        if texts.is_empty() {
            return Ok(Embedded::metered(&self.signature, 0, Vec::new()));
        }

        let response = self.post(texts).await?;
        let vectors = response.vectors(texts.len(), self.signature.dims)?;

        Ok(match response.usage.as_ref() {
            // Both numbers from the gateway: the tokens it counted and the cost
            // it charged.
            Some(usage) if usage.cost.is_some() => Embedded::charged(
                usage.prompt_tokens.max(usage.total_tokens),
                usage.cost.unwrap_or_default(),
            ),
            // Tokens but no price. Real count, local table.
            Some(usage) => Embedded::metered(
                &self.signature,
                usage.prompt_tokens.max(usage.total_tokens),
                vectors.clone(),
            ),
            // Neither. Estimate, the same as any other provider.
            None => Embedded::billed(&self.signature, texts, vectors.clone()),
        }
        .with_vectors(vectors))
    }
}

/// Parse a response body, rejecting anything that is not the expected shape.
fn parse(body: &str) -> Result<EmbeddingsResponse> {
    serde_json::from_str::<EmbeddingsResponse>(body).map_err(|err| {
        let preview: String = body.chars().take(200).collect();
        Error::Model(format!(
            "openrouter embeddings returned a body this build cannot read ({err}): {preview}"
        ))
    })
}

#[derive(Debug, Deserialize)]
struct EmbeddingsResponse {
    data: Vec<EmbeddingRow>,
    #[serde(default)]
    usage: Option<UsageWire>,
}

impl EmbeddingsResponse {
    /// The vectors, in the order the inputs were given.
    ///
    /// The rows carry an `index` and the gateway is under no obligation to
    /// return them in order — several upstreams do not. Sorting by it rather
    /// than trusting arrival order is the difference between a correct index
    /// and one where every chunk is filed under its neighbour's vector, which
    /// no test of the retrieval layer would ever catch.
    fn vectors(&self, expected: usize, dims: usize) -> Result<Vec<Vec<f32>>> {
        if self.data.len() != expected {
            return Err(Error::Model(format!(
                "openrouter embeddings returned {} vectors for {expected} inputs",
                self.data.len()
            )));
        }

        let mut rows: Vec<&EmbeddingRow> = self.data.iter().collect();
        rows.sort_by_key(|row| row.index);

        // An out-of-range or duplicated index would survive the sort and put a
        // vector on the wrong chunk, so the sequence is checked rather than
        // assumed.
        for (position, row) in rows.iter().enumerate() {
            if row.index != position {
                return Err(Error::Model(format!(
                    "openrouter embeddings returned index {} where {position} was expected",
                    row.index
                )));
            }
        }

        for row in &rows {
            if row.embedding.len() != dims {
                return Err(Error::Model(format!(
                    "openrouter embeddings returned a {}-dimensional vector where the \
                     configured signature says {dims}; the index would be built against \
                     the wrong width",
                    row.embedding.len()
                )));
            }
        }

        Ok(rows.iter().map(|row| row.embedding.clone()).collect())
    }
}

#[derive(Debug, Deserialize)]
struct EmbeddingRow {
    embedding: Vec<f32>,
    #[serde(default)]
    index: usize,
}

/// The usage block. Every field optional: it is the gateway's to send, and a
/// missing one must degrade to an estimate rather than fail the call.
#[derive(Debug, Default, Deserialize)]
struct UsageWire {
    #[serde(default)]
    prompt_tokens: u64,
    #[serde(default)]
    total_tokens: u64,
    #[serde(default)]
    cost: Option<f64>,
}

#[cfg(test)]
#[path = "openrouter_test.rs"]
mod tests;
