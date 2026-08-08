//! The knowledge-store port.
//!
//! Always compiled; the MongoDB adapter is behind `serve`.
//!
//! Curated context — house conventions, runbooks, decisions a previous review
//! already argued out — scoped to an organisation or to one repository. A repo
//! lookup returns the repository's own documents *and* its organisation's, so a
//! convention written once applies everywhere without being copied.
//!
//! The pinned/retrievable split is the load-bearing part. Pinned documents go
//! into every prompt unconditionally; retrievable ones only surface when a
//! query reaches them. "We never `unwrap` in library code" must not depend on
//! the diff happening to embed near that sentence, and expressing that as a
//! relevance boost would make it a tuning parameter someone later turns down.

use async_trait::async_trait;

use crate::error::Result;
use crate::index::types::{KnowledgeDoc, KnowledgeScope};

/// A store of org- and repo-scoped curated documents.
#[async_trait]
pub trait KnowledgeStore: Send + Sync {
    /// Prepare the store: create the indexes scoped lookups rely on.
    async fn prepare(&self) -> Result<()>;

    /// Insert or replace a document.
    async fn put(&self, doc: &KnowledgeDoc) -> Result<()>;

    /// Fetch one document by id.
    async fn get(&self, id: &str) -> Result<Option<KnowledgeDoc>>;

    /// Delete a document, reporting whether it existed.
    async fn delete(&self, id: &str) -> Result<bool>;

    /// Every always-included document visible from `scope`.
    ///
    /// Returned in full rather than paged: if the pinned set is large enough to
    /// need paging it is already too large to put in a prompt, and the caller
    /// should find that out.
    async fn pinned(&self, scope: &KnowledgeScope) -> Result<Vec<KnowledgeDoc>>;

    /// Every retrievable (not pinned) document visible from `scope`.
    ///
    /// These are the candidates an embedding pass indexes into the chunk index;
    /// the store itself does no ranking.
    async fn retrievable(&self, scope: &KnowledgeScope) -> Result<Vec<KnowledgeDoc>>;
}
