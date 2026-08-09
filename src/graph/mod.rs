//! The repository graph: what a change can reach.
//!
//! Always compiled. Storage goes through the
//! [`GraphStore`](crate::ports::graph::GraphStore) port, so the default build
//! links no database driver and the whole pipeline is exercised against
//! [`MockGraphStore`](crate::index::MockGraphStore).
//!
//! The pipeline is four steps, one module each:
//!
//! 1. [`extract`] parses a file with tree-sitter into definitions, imports and
//!    usages.
//! 2. [`aliases`] reads the repository's own configuration — `tsconfig.json`,
//!    `go.mod`, `Cargo.toml` — for the path aliases its imports are written in.
//! 3. [`resolve`] turns a specifier into the file it names, per language, and
//!    records what it could not.
//! 4. [`build`] assembles nodes and edges and writes them; [`traverse`] walks
//!    them back out, bounded by hops and by a hard node cap.
//! 5. [`impact`] reads that walk *inbound only*, for the narrower question a
//!    review asks: what existing code breaks if this change is wrong, and what
//!    changed code no test reaches.
//!
//! **This graph exists to be traversed during a review**, not to be drawn. The
//! obvious version of this feature is a dashboard endpoint whose only caller is
//! an HTTP route — and whose specifier matching gives up on anything that is
//! not a relative path, so it cannot follow the very aliases its own codebase
//! is written in. Retrieval seeds [`traverse::neighbours`] with the symbols a
//! diff touches; that is the whole reason the resolver is as careful as it is.

pub mod aliases;
pub mod build;
pub mod extract;
pub mod impact;
pub mod lang;
pub mod path;
pub mod resolve;
pub mod traverse;
pub mod types;

pub use crate::graph::aliases::{AliasConfig, AliasPattern, TsBaseUrl};
pub use crate::graph::build::{build, sync_all, sync_paths};
pub use crate::graph::extract::parse;
pub use crate::graph::impact::{DEFAULT_MAX_REACHED, Impact, Impacted, Relation};
pub use crate::graph::resolve::{Resolution, Resolver};
pub use crate::graph::traverse::{DEFAULT_HOPS, DEFAULT_MAX_NODES, NeighbourQuery, neighbours};
pub use crate::graph::types::{
    Coverage, Definition, ImportStmt, Language, ParsedFile, RepoGraph, SourceFile, SymbolKind,
    Unresolved, UnresolvedReason, Usage,
};
