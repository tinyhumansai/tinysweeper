//! The embedding port.
//!
//! Always compiled. Nothing here links an HTTP client: the offline
//! [`MockEmbedder`](crate::index::MockEmbedder) is a real implementation, and a
//! provider-backed one arrives behind a feature.
//!
//! The port's one unusual demand is [`Embedder::signature`]. An embedder that
//! could not name itself would let a model swap go unnoticed, and the symptom
//! of that is not an error — it is confidently wrong retrieval against vectors
//! from two incompatible spaces. Making the signature part of the trait forces
//! every implementation to be identifiable, and the index uses it as a
//! partition key so the invalidation is structural rather than remembered.

use async_trait::async_trait;

use crate::error::Result;
use crate::index::types::EmbedSignature;

/// Turns text into vectors.
#[async_trait]
pub trait Embedder: Send + Sync {
    /// Which provider, model and dimensionality this embedder speaks.
    ///
    /// Not `async` and not fallible: it must be answerable before any network
    /// call, because it is what the index is opened with.
    fn signature(&self) -> EmbedSignature;

    /// Embed a batch of documents.
    ///
    /// Batched rather than one-at-a-time because indexing a repository is
    /// thousands of chunks and per-call latency dominates. The returned vectors
    /// are in the same order as `texts`, and there is exactly one per input —
    /// implementations must not silently drop a text they could not handle.
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>>;

    /// Embed a search query.
    ///
    /// Separate from [`Embedder::embed`] because several providers want an
    /// explicit query/document hint and score noticeably worse without it. The
    /// default forwards to the batch call, which is correct for providers that
    /// make no distinction.
    async fn embed_query(&self, text: &str) -> Result<Vec<f32>> {
        let mut vectors = self.embed(std::slice::from_ref(&text.to_string())).await?;
        vectors.pop().ok_or_else(|| {
            crate::error::Error::Model("the embedder returned no vector for the query".into())
        })
    }
}
