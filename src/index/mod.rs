//! Retrieval: the chunk index, the code graph, and the knowledge base.
//!
//! This is the adapter side of four ports — [`Embedder`](crate::ports::embed),
//! [`ChunkIndex`](crate::ports::index), [`GraphStore`](crate::ports::graph) and
//! [`KnowledgeStore`](crate::ports::knowledge). [`mock`] is always compiled and
//! backs the whole test suite; the MongoDB adapter sits behind `serve`,
//! alongside the rest of the stateful server.
//!
//! Why a code index at all: a reviewer that only sees the diff cannot tell a
//! rename from a semantic change, cannot find the second caller that needed the
//! same fix, and re-argues conventions the team settled a year ago. The three
//! stores answer those in turn — similarity for "what looks like this", the
//! graph for "what breaks if this changes", the knowledge base for "what did we
//! already decide".

#[cfg(feature = "serve")]
pub mod client;
pub mod mock;
#[cfg(feature = "serve")]
pub mod mongo;
#[cfg(feature = "harness")]
pub mod openrouter;
#[cfg(feature = "harness")]
pub mod provider;
pub mod types;

pub use crate::index::mock::{MockChunkIndex, MockEmbedder, MockGraphStore, MockKnowledgeStore};
pub use crate::index::types::{
    Chunk, ChunkMethod, EdgeKind, EmbedSignature, Embedded, EmbeddedChunk, GraphEdge, GraphNode,
    HybridQuery, KnowledgeDoc, KnowledgeScope, Neighbourhood, NodeKind, ScoredChunk,
};

#[cfg(feature = "harness")]
pub use crate::index::openrouter::{OPENROUTER_EMBEDDINGS_URL, OpenRouterEmbedder};
#[cfg(feature = "harness")]
pub use crate::index::provider::ProviderEmbedder;

#[cfg(feature = "serve")]
pub use crate::index::mongo::{MongoIndex, VECTOR_SEARCH_UNAVAILABLE};

/// Build the embedder `[embeddings]` describes, whichever provider it names.
///
/// A free function rather than a constructor on either type because the two
/// implementations do not share a base: `openrouter` is a direct HTTP client
/// (it keeps the `usage` block that tinyagents' `EmbeddingModel` throws away),
/// and everything else goes through that trait. Callers want an `Embedder`,
/// not to know which.
///
/// `Ok(None)` means no provider is configured, which is a supported deployment
/// — reviews run diff-only. An error means one *was* configured and could not
/// be opened, which is a mistake rather than a choice.
#[cfg(feature = "harness")]
pub fn embedder_from_config(
    config: &crate::config::types::Embeddings,
) -> crate::error::Result<Option<std::sync::Arc<dyn crate::ports::embed::Embedder>>> {
    let Some(signature) = config.signature() else {
        return Ok(None);
    };

    if signature.provider == "openrouter" {
        let embedder = OpenRouterEmbedder::new(signature, &config.api_key_env, &config.base_url)?;
        return Ok(Some(std::sync::Arc::new(embedder)));
    }

    Ok(ProviderEmbedder::from_config(config)?.map(|embedder| {
        std::sync::Arc::new(embedder) as std::sync::Arc<dyn crate::ports::embed::Embedder>
    }))
}
