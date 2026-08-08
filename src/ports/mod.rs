//! The kernel's dependency-inverted seams. One port is one trait in one file.
//!
//! Every port has an always-compiled offline implementation, so the default
//! build links no HTTP client and the test suite never reaches the network. The
//! real, network-backed adapters live in sibling modules behind Cargo features.

pub mod embed;
pub mod forge;
pub mod graph;
pub mod index;
pub mod knowledge;
pub mod model;
pub mod review_state;

pub use crate::ports::embed::Embedder;
pub use crate::ports::forge::{ForgeRead, ForgeWrite};
pub use crate::ports::graph::GraphStore;
pub use crate::ports::index::ChunkIndex;
pub use crate::ports::knowledge::KnowledgeStore;
pub use crate::ports::model::{Message, Model, ModelRequest, ModelResponse, Role, Usage};
pub use crate::ports::review_state::ReviewStateStore;
