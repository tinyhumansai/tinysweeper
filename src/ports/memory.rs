//! The memory port: what the reviewer remembers about a repository over time.
//!
//! Always compiled; the CortexDB adapter is behind the `cortex` feature and the
//! offline implementation is [`crate::memory::MockMemory`].
//!
//! ## What this is, next to the index
//!
//! The chunk index answers *what does this code look like* and the graph
//! answers *what does this change reach*. Both are rebuilt from the tree and
//! neither learns anything. Memory is the part that accumulates: what the
//! repository says about itself in its instruction files, what the reviewer
//! found on earlier pull requests, and what the maintainers did with those
//! findings. A reviewer with no memory re-argues every convention and re-raises
//! every rejected finding; one with memory can be told "you said this before
//! and they said no".
//!
//! ## Three operations, and why the third exists
//!
//! `remember` and `recall` are what any store offers. `answer` is different in
//! kind: it hands the *engine* a question and gets back prose grounded in
//! citations. It exists because the questions a reviewer needs answered —
//! "what conventions govern these paths?", "has a finding like this been
//! rejected?" — are questions, not queries, and an engine that holds extracted
//! facts and an entity graph can answer them better than a ranked list of
//! chunks pasted into the prompt can. The answer is still data: it lands in
//! the volatile suffix, fenced, and a lane is told to weigh it, not obey it.
//!
//! ## Every call is best-effort at the call site
//!
//! Nothing in the review path lets a memory failure fail the review. The port
//! returns errors so the adapter can be honest; `crate::memory::recall` turns
//! them into a status the check-run summary states, and the review runs on
//! whatever else it has.

use async_trait::async_trait;

use crate::error::Result;
use crate::memory::types::{
    Ask, MemoryAnswer, MemoryItem, MemoryScope, Recollection, RememberReport,
};

/// A long-lived memory of repositories, scoped per repository and section.
#[async_trait]
pub trait Memory: Send + Sync {
    /// The engine's name, for logs and the doctor report.
    fn name(&self) -> &str;

    /// Prove the engine is reachable. Called at boot so a misconfigured
    /// endpoint is a refusal to start rather than a silently forgetful review.
    async fn health(&self) -> Result<()>;

    /// Write `items` into `scope`.
    ///
    /// Idempotent on [`MemoryItem::content_id`]: offering the same key and
    /// body twice writes once. Items whose section does not match a section
    /// scope are an error, because a caller that files a review outcome under
    /// `code` has a bug that should not be quietly filed.
    async fn remember(&self, scope: &MemoryScope, items: &[MemoryItem]) -> Result<RememberReport>;

    /// The items in `scope` most relevant to `query`, best first, at most
    /// `limit` of them.
    async fn recall(
        &self,
        scope: &MemoryScope,
        query: &str,
        limit: usize,
    ) -> Result<Vec<Recollection>>;

    /// Ask the engine a question about `scope` and get a cited answer.
    ///
    /// The evidence the answer is grounded on is gathered by
    /// [`Ask::evidence`] when it is given, and by the question's own words
    /// when it is not. An engine that cannot answer returns an answer whose
    /// [`MemoryAnswer::is_grounded`] is false rather than an error: "nothing
    /// remembered" is an ordinary outcome.
    async fn answer(&self, scope: &MemoryScope, ask: &Ask<'_>) -> Result<MemoryAnswer>;

    /// Forget everything in `scope`, reporting how many items went.
    ///
    /// The one destructive operation, and it takes a scope rather than a
    /// predicate so the smallest thing it can remove is a whole section of one
    /// repository — there is no selector to get wrong.
    async fn forget(&self, scope: &MemoryScope) -> Result<u64>;
}
