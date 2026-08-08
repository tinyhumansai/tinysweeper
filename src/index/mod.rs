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

pub mod mock;
#[cfg(feature = "serve")]
pub mod mongo;
pub mod types;

pub use crate::index::mock::{MockChunkIndex, MockEmbedder, MockGraphStore, MockKnowledgeStore};
pub use crate::index::types::{
    Chunk, ChunkMethod, EdgeKind, EmbedSignature, EmbeddedChunk, GraphEdge, GraphNode, HybridQuery,
    KnowledgeDoc, KnowledgeScope, Neighbourhood, NodeKind, ScoredChunk,
};

#[cfg(feature = "serve")]
pub use crate::index::mongo::{MongoIndex, VECTOR_SEARCH_UNAVAILABLE};
