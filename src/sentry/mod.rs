//! Sentry issue promotion: unresolved Sentry issues become tracked GitHub
//! issues, deduplicated, PII-scrubbed, and linked back.
//!
//! Feature-gated network access only. The pipeline, the redaction boundary and
//! every policy decision are always compiled and tested offline against
//! [`crate::ports::sentry::MockSentry`]; the HTTP adapter that actually talks
//! to Sentry lives behind the `sentry` feature.
//!
//! ## The four steps
//!
//! | Step | Module | What it decides |
//! | --- | --- | --- |
//! | Select | [`select`] | which issues clear `min_events` / `min_users` / `ignore_culprits`, and what the `max_per_run` cap truncated |
//! | Deduplicate | [`dedupe`] | whether GitHub already tracks this Sentry issue |
//! | Promote | [`promote`] | the issue body, from allow-listed fields only |
//! | Close the loop | [`link`] | annotating Sentry, and resolving it once GitHub closes |
//!
//! [`sweep`] is the orchestration that runs them in that order.
//!
//! ## The ordering that matters
//!
//! [`redact`] lands before anything that writes. Every string reaching GitHub,
//! a log line, or a dedupe key has been through it, because the alternative
//! leaves a window in which the sweep works and the redaction does not — and
//! GitHub keeps the edit history of an issue body, so a leak in that window is
//! permanent.
//!
//! ## Where the source of truth lives
//!
//! In the GitHub issue body, as a marker comment. "Is this already tracked?"
//! is a search against GitHub, which is the system that actually holds the
//! answer; any cache is an optimisation on top and must never be consulted
//! instead. A lost cache degrades to a slower sweep, never to a duplicate —
//! and a duplicate is the worst outcome available here, because it is the one
//! that scales: every subsequent sweep adds another copy.

#[cfg(feature = "sentry")]
pub mod client;
pub mod dedupe;
pub mod link;
pub mod mock;
pub mod pii;
pub mod promote;
pub mod redact;
pub mod select;
pub mod sweep;
pub mod types;

pub use crate::sentry::mock::MockSentry;
pub use crate::sentry::sweep::{SweepOutcome, SweepReport, sweep};
pub use crate::sentry::types::{RawEvent, RawIssue, SafeIssue, Skipped};
