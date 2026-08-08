//! Turning a source tree into indexable chunks.
//!
//! Always compiled. The tree-sitter grammars sit behind `treesitter`, which is
//! on by default: a grammar opens no socket, so it does not fall under the
//! "network goes behind a feature" rule the rest of the crate follows.
//!
//! The whole module exists to avoid one failure mode. A content-blind chunker —
//! accumulate characters until a limit, then cut, with a couple of lines of
//! overlap — produces spans that routinely begin halfway through a function and
//! end halfway through another. Retrieval then returns a chunk that does not
//! contain the symbol it was retrieved for, and a reviewer quotes a fragment
//! that never compiled. So boundaries are chosen by a grammar wherever one
//! exists, definitions are never cut in half, and where no grammar exists the
//! chunk *says* it was cut on lines rather than presenting itself as parsed.
//!
//! The pipeline is three separable steps, one per module:
//!
//! 1. [`select`] decides which files are worth indexing at all, and **reports**
//!    every file it left out.
//! 2. [`tree`] (or [`lines`], as the fallback) chooses the spans.
//! 3. [`chunker`] hashes each span and hands back [`Chunk`](crate::index::Chunk)
//!    values the index can store.

pub mod chunker;
pub mod lang;
pub mod lines;
pub mod select;
#[cfg(feature = "treesitter")]
pub mod tree;
pub mod types;

pub use crate::chunk::chunker::Chunker;
pub use crate::chunk::lang::Language;
pub use crate::chunk::select::Selector;
pub use crate::chunk::types::{ChunkOptions, Selection, SkipReason, SkippedFile, SourceChunk};
