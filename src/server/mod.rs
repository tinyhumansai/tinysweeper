//! The webhook server.
//!
//! Reverses an earlier decision to run only in GitHub Actions. Three arguments
//! changed it, and only the first was in the original analysis:
//!
//! - Runner minutes across 43 repositories are real money for work that is
//!   mostly waiting on a model.
//! - A contributor is a fact about a *person over time*. A whitelist that
//!   resets on every workflow run is not a whitelist, and a stateless job has
//!   nowhere to keep one.
//! - Replying to a comment within seconds needs an event loop, not a cold
//!   runner.
//!
//! What did **not** change: resolving a review thread and reacting to a comment
//! still emit no webhook at all, so that state is polled here exactly as it
//! would have been in a workflow. See `docs/triggers.md`.
//!
//! The security boundary is unchanged and enforced the same way: the worker
//! runs the lanes with a read-only token, and a write token is minted only
//! afterwards, in the apply step.

pub mod auth;
pub mod routes;
pub mod store;
pub mod webhook;

pub use crate::server::routes::{ServerConfig, serve};
pub use crate::server::store::{Contributor, Store, Trust};
