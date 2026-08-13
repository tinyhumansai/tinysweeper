//! The Sentry port.
//!
//! Always compiled; the HTTP adapter is behind the `sentry` feature and the
//! offline implementation is [`crate::sentry::mock::MockSentry`].
//!
//! ## The port's types are the allow-list
//!
//! Every read method answers with a type from [`crate::sentry::types`], which
//! declares only promotable fields. That is deliberate and is the reason this
//! trait does not simply hand back `serde_json::Value`: an adapter cannot
//! return personal data through this seam because there is no field on the
//! return type to put it in. The boundary is enforced at the port rather than
//! downstream of it, so a second adapter — a self-hosted Sentry, a replay
//! harness, a fixture loader — inherits it without re-deriving the argument.
//!
//! ## Writes are narrow on purpose
//!
//! Two mutations, both additive-or-reversible: leave a comment, and set the
//! status to resolved. There is deliberately no delete, no assign, no merge
//! and no issue-deletion method, because nothing in the promotion pipeline
//! needs one and an unused write method is a write method something later
//! reaches for.

use async_trait::async_trait;

use crate::error::Result;
use crate::sentry::types::{RawEvent, RawIssue};

/// Read and lightly mutate a Sentry installation.
#[async_trait]
pub trait SentryApi: Send + Sync {
    /// Unresolved issues for `project`, most-recently-seen first.
    ///
    /// `limit` is a request, not a guarantee — an adapter may return fewer.
    /// It exists so a project with a hundred thousand unresolved issues does
    /// not have to be paged in full to promote ten, and it is deliberately not
    /// the same number as `sentry.max_per_run`: filters run after the fetch,
    /// so fetching exactly the cap would starve a project whose top issues are
    /// all below `min_events`.
    async fn unresolved_issues(&self, project: &str, limit: usize) -> Result<Vec<RawIssue>>;

    /// The latest event for one issue, when it has one.
    ///
    /// `Ok(None)` rather than an error when the issue has no retained event:
    /// Sentry expires event bodies on its own retention schedule while keeping
    /// the issue, and a promotion without frames is still worth opening.
    async fn latest_event(&self, issue_id: &str) -> Result<Option<RawEvent>>;

    /// Comment `text` onto a Sentry issue.
    ///
    /// Used only to write the GitHub issue URL back, so the two systems are
    /// navigable in both directions.
    async fn annotate(&self, issue_id: &str, text: &str) -> Result<()>;

    /// Mark a Sentry issue resolved.
    async fn resolve(&self, issue_id: &str) -> Result<()>;
}
