//! The real embedder, behind the `harness` feature.
//!
//! A thin adapter over tinyagents' `EmbeddingModel`, exactly parallel to
//! `crate::harness::openrouter::GatewayModel` over its completion provider: the
//! harness owns the transport, the rate limiter and the `Retry-After` backoff,
//! and this file owns the two things tinysweeper cares about that the harness
//! has no opinion on — the **signature** and the **bill**.
//!
//! # The signature is the whole point
//!
//! Both sides name an embedding space. tinyagents spells it
//! `provider=…;model=…;dims=…`; [`EmbedSignature`] spells it
//! `provider:model:dims` and writes it onto every indexed document. If those
//! two ever describe different things the failure is silent: the index keeps
//! answering queries, with vectors from a model that no longer exists,
//! confidently and wrongly.
//!
//! So the adapter refuses to exist unless they agree. [`ProviderEmbedder::new`]
//! compares the configured signature against what the model reports and returns
//! an error on a mismatch, which turns "wrong embedding space" from a retrieval
//! bug into a startup failure with the two values printed side by side.
//!
//! # The bill
//!
//! Tokens are estimated, at four bytes each, and that is a limitation rather
//! than a choice — see [`Embedded::metered`] for the seam a real count plugs
//! into. `EmbeddingModel::embed` returns `Vec<Vec<f32>>`, and every adapter
//! upstream decodes the response's `data` array and discards its `usage`
//! object, so there is nothing to plumb through even for the providers
//! (OpenAI, Voyage, Cohere) that do report one on the wire. The estimate rounds
//! up and unpriced models are billed at the ceiling, so the budget errs
//! expensive.

use std::sync::Arc;

use async_trait::async_trait;
use tinyagents::harness::embeddings::{
    CohereEmbeddingModel, EmbeddingModel, MockEmbeddingModel, OllamaEmbeddingModel,
    OpenAiEmbeddingModel, VoyageEmbeddingModel, set_rate_limit,
};

use crate::config::types::Embeddings;
use crate::error::{Error, Result};
use crate::index::types::{EmbedSignature, Embedded};
use crate::ports::embed::Embedder;

/// An [`Embedder`] backed by a real provider.
///
/// `Debug` is written by hand for the same reason `GatewayModel`'s is: the
/// model this holds was constructed with an API key, and a derived `Debug`
/// would be one `tracing` call away from printing it.
pub struct ProviderEmbedder {
    model: Arc<dyn EmbeddingModel>,
    signature: EmbedSignature,
}

impl std::fmt::Debug for ProviderEmbedder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderEmbedder")
            .field("signature", &self.signature.key())
            .finish_non_exhaustive()
    }
}

impl ProviderEmbedder {
    /// Wrap a harness model, refusing a signature it does not match.
    ///
    /// The check is the reason this constructor is fallible. A model that
    /// reports 1536 dimensions behind a config that says 1024 would write
    /// 1536-wide vectors into an index created for 1024, and MongoDB would
    /// reject them one at a time, halfway through a run — or worse, a model
    /// renamed under the same width would be accepted forever and quietly
    /// answer from a different space.
    pub fn new(model: Arc<dyn EmbeddingModel>, signature: EmbedSignature) -> Result<Self> {
        let reported = EmbedSignature::new(model.name(), model.model_id(), model.dimensions());
        if reported != signature {
            return Err(Error::config(format!(
                "the embedding provider reports `{}` but the configuration says `{}`. These are \
                 different embedding spaces: indexing under one and querying under the other \
                 returns confident nonsense rather than an error, so the mismatch is refused \
                 here. Fix `[embeddings]` to match the provider, or point it at the model it \
                 names.",
                reported.key(),
                signature.key()
            )));
        }
        Ok(Self { model, signature })
    }

    /// Build from `[embeddings]`, reading the key from the named variable.
    ///
    /// Returns `Ok(None)` when the section is disabled — a deployment with no
    /// embedding provider is supported, and every retrieval path degrades
    /// through it — and an error when it is enabled but unusable, because a
    /// half-configured provider is a mistake rather than a choice.
    pub fn from_config(config: &Embeddings) -> Result<Option<Self>> {
        let Some(signature) = config.signature() else {
            return Ok(None);
        };

        // Process-global, and set before the first call rather than after: the
        // limiter exists to keep a cold full index out of the provider's own
        // 429 path, and a limit applied late has already let the burst through.
        if config.requests_per_minute > 0 {
            set_rate_limit(config.requests_per_minute);
        }

        let model = build_model(config, &signature)?;
        Self::new(model, signature).map(Some)
    }

    /// The harness model behind this adapter.
    pub fn model(&self) -> &Arc<dyn EmbeddingModel> {
        &self.model
    }
}

/// Construct the provider named in the configuration.
///
/// A `match` over a small closed set rather than a registry: every arm here is
/// a model whose price is on file in `crate::harness::pricing`, and adding one
/// without adding its price is how indexing escapes the budget.
fn build_model(
    config: &Embeddings,
    signature: &EmbedSignature,
) -> Result<Arc<dyn EmbeddingModel>> {
    let provider = signature.provider.as_str();
    let base_url = config.base_url.trim();

    // Ollama is local and has no key; every other provider needs one and says
    // which variable holds it rather than accepting one from the config file.
    let key = |()| -> Result<String> {
        std::env::var(config.api_key_env.trim())
            .ok()
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| {
                Error::config(format!(
                    "{} is not set; it holds the API key for the `{provider}` embedding provider",
                    config.api_key_env.trim()
                ))
            })
    };

    let model: Arc<dyn EmbeddingModel> = match provider {
        "voyage" => {
            let url = if base_url.is_empty() {
                tinyagents::harness::embeddings::VOYAGE_API_BASE
            } else {
                base_url
            };
            Arc::new(VoyageEmbeddingModel::with_options(
                key(())?,
                &signature.model,
                signature.dims,
                url,
            ))
        }
        "openai" => {
            let mut built = OpenAiEmbeddingModel::new(key(())?)
                .with_model(&signature.model)
                .with_dimensions(signature.dims);
            if !base_url.is_empty() {
                built = built.with_base_url(base_url);
            }
            Arc::new(built)
        }
        "cohere" => {
            let mut built = CohereEmbeddingModel::new(key(())?)
                .with_model(&signature.model)
                .with_dimensions(signature.dims);
            if !base_url.is_empty() {
                built = built.with_base_url(base_url);
            }
            Arc::new(built)
        }
        "ollama" => {
            let url = if base_url.is_empty() {
                tinyagents::harness::embeddings::DEFAULT_OLLAMA_URL
            } else {
                base_url
            };
            // `try_new` rather than `new`: it validates the URL, and a typo in
            // a local endpoint should be a startup error rather than a
            // connection failure on the first push.
            Arc::new(
                OllamaEmbeddingModel::try_new(url, &signature.model, signature.dims)
                    .map_err(|err| Error::config(format!("ollama embeddings: {err}")))?,
            )
        }
        // Not a test hook: a deployment that wants the index machinery exercised
        // end to end without a provider bill has no other way to do it, and the
        // signature keeps its rows in their own partition where they cannot be
        // mistaken for real ones.
        "mock" => Arc::new(MockEmbeddingModel::new(signature.dims)),
        other => {
            return Err(Error::config(format!(
                "`embeddings.provider = \"{other}\"` names no provider this build knows. \
                 Supported: voyage, openai, cohere, ollama, mock."
            )));
        }
    };

    Ok(model)
}

#[async_trait]
impl Embedder for ProviderEmbedder {
    fn signature(&self) -> EmbedSignature {
        self.signature.clone()
    }

    async fn embed(&self, texts: &[String]) -> Result<Embedded> {
        if texts.is_empty() {
            return Ok(Embedded::default());
        }

        let vectors = self
            .model
            .embed(texts)
            .await
            .map_err(|err| Error::Model(format!("{}: {err}", self.signature.key())))?;

        // The port promises one vector per input in input order, and callers
        // zip on that promise — `Indexer::index_group` pairs vectors with the
        // chunks they belong to positionally. A provider that returned a short
        // batch would not error there, it would attach the wrong vectors to the
        // wrong chunks and index them, so the count is checked here.
        if vectors.len() != texts.len() {
            return Err(Error::Model(format!(
                "{}: asked for {} embeddings and got {}",
                self.signature.key(),
                texts.len(),
                vectors.len()
            )));
        }
        if let Some(wrong) = vectors.iter().find(|vector| vector.len() != self.signature.dims) {
            return Err(Error::Model(format!(
                "{}: the provider returned a {}-dimensional vector",
                self.signature.key(),
                wrong.len()
            )));
        }

        Ok(Embedded::billed(&self.signature, texts, vectors))
    }

    async fn embed_query(&self, text: &str) -> Result<Embedded> {
        // Forwarded rather than defaulted, because several providers score
        // noticeably worse without the query hint and the harness models take
        // that hint on this method. Voyage and Cohere both use it.
        let vector = self
            .model
            .embed_query(text)
            .await
            .map_err(|err| Error::Model(format!("{}: {err}", self.signature.key())))?;
        if vector.len() != self.signature.dims {
            return Err(Error::Model(format!(
                "{}: the provider returned a {}-dimensional query vector",
                self.signature.key(),
                vector.len()
            )));
        }
        Ok(Embedded::billed(
            &self.signature,
            std::slice::from_ref(&text.to_string()),
            vec![vector],
        ))
    }
}

#[cfg(test)]
#[path = "provider_test.rs"]
mod tests;
