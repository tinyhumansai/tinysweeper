//! The crate error type and its `Result` alias.
//!
//! Every fallible function in tinysweeper returns [`Result<T>`]. Variants are
//! deliberately coarse: the message carries the detail, because most of them
//! end up rendered into a GitHub check-run summary that a human reads.

use std::path::PathBuf;

/// Errors produced anywhere in tinysweeper.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A configuration file was missing, unparseable, or invalid.
    ///
    /// Validation collects *every* problem before returning, so this message
    /// is frequently multi-line — see `config::validate`.
    #[error("configuration error: {0}")]
    Config(String),

    /// No `.tinysweeper.toml` was found at any of the searched locations.
    #[error("no tinysweeper config found (looked for {0})")]
    ConfigNotFound(String),

    /// A path that had to exist did not, or could not be read.
    #[error("{path}: {message}")]
    Path {
        /// The offending path.
        path: PathBuf,
        /// What went wrong with it.
        message: String,
    },

    /// A git operation over the checkout failed.
    #[error("git: {0}")]
    Git(String),

    /// The forge (GitHub) rejected a request or returned something unusable.
    #[error("forge: {0}")]
    Forge(String),

    /// The forge refused the request because the caller's request budget is
    /// spent, and said when it refills.
    ///
    /// Its own variant rather than a `Forge` message because one caller —
    /// the memory backfill, a walk of thousands of requests — must *wait*
    /// for this and retry, where every other forge error is final. The
    /// reset is a Unix timestamp when the forge reported one; `None` when
    /// it did not, and the caller picks a pessimistic wait.
    #[error("forge: rate limited{}", reset_clause(*.reset_at))]
    RateLimited {
        /// When the budget refills, as seconds since the Unix epoch.
        reset_at: Option<u64>,
    },

    /// A model call failed, timed out, or returned output that did not match
    /// the lane's structured-output schema.
    #[error("model: {0}")]
    Model(String),

    /// A lane refused to run or could not produce a verdict.
    #[error("lane {lane}: {message}")]
    Lane {
        /// The lane's stable id, e.g. `critique`.
        lane: String,
        /// Why it could not produce a verdict.
        message: String,
    },

    /// A bounded operation did not finish inside its wall-clock deadline.
    ///
    /// Deliberately not one of the transient variants: the deadline is the
    /// budget, and retrying a run that has already spent it only spends it
    /// again.
    #[error("{what} did not finish within {seconds}s")]
    Timeout {
        /// What was being waited on, e.g. `the review of owner/repo#12`.
        what: String,
        /// The deadline that elapsed, in seconds.
        seconds: u64,
    },

    /// The per-pull-request budget was exhausted before the run completed.
    #[error("budget exhausted: spent ${spent:.2} of ${limit:.2}")]
    Budget {
        /// USD spent when the limit tripped.
        spent: f64,
        /// The configured ceiling.
        limit: f64,
    },

    /// A pull request is larger than the review's deterministic input limits.
    ///
    /// Its own variant keeps this refusal non-transient and lets the server
    /// explain the remedy without treating an intentional guard as a forge or
    /// model outage.
    #[error(
        "pull request exceeds review limits: {changed_files} changed files (limit {max_files}), {changed_lines} changed lines (limit {max_lines})"
    )]
    ReviewLimit {
        /// Files reported by the forge.
        changed_files: usize,
        /// Configured file ceiling.
        max_files: usize,
        /// Added plus deleted lines reported by the forge.
        changed_lines: u64,
        /// Configured changed-line ceiling.
        max_lines: u64,
    },

    /// A feature required for this code path was not compiled in.
    #[error("{0} requires the `{1}` feature; rebuild with --features {1}")]
    FeatureDisabled(&'static str, &'static str),

    /// Filesystem I/O failed.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    /// TOML deserialization failed.
    #[error("toml: {0}")]
    Toml(#[from] toml::de::Error),

    /// JSON serialization or deserialization failed.
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
}

/// The tail of the `RateLimited` message.
fn reset_clause(reset_at: Option<u64>) -> String {
    match reset_at {
        Some(at) => format!(" until unix time {at}"),
        None => String::new(),
    }
}

impl Error {
    /// Build a [`Error::Config`] from anything displayable.
    pub fn config(message: impl std::fmt::Display) -> Self {
        Self::Config(message.to_string())
    }

    /// Build a [`Error::Path`] for `path`.
    pub fn path(path: impl Into<PathBuf>, message: impl std::fmt::Display) -> Self {
        Self::Path {
            path: path.into(),
            message: message.to_string(),
        }
    }

    /// Build a [`Error::Timeout`] for `what` after `after` elapsed.
    ///
    /// Rounded up, not truncated: `as_secs()` alone would report a positive
    /// sub-second deadline — `Duration::from_millis(500)`, say — as `0s`,
    /// which reads as "no time at all" rather than the budget that was
    /// actually configured.
    pub fn timeout(what: impl Into<String>, after: std::time::Duration) -> Self {
        Self::Timeout {
            what: what.into(),
            seconds: after
                .as_secs()
                .saturating_add(u64::from(after.subsec_nanos() != 0)),
        }
    }

    /// Build a [`Error::Lane`] for `lane`.
    pub fn lane(lane: impl Into<String>, message: impl std::fmt::Display) -> Self {
        Self::Lane {
            lane: lane.into(),
            message: message.to_string(),
        }
    }
}

/// The crate-wide result alias.
pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn feature_disabled_names_the_flag_to_rebuild_with() {
        let err = Error::FeatureDisabled("posting to GitHub", "github");
        assert_eq!(
            err.to_string(),
            "posting to GitHub requires the `github` feature; rebuild with --features github"
        );
    }

    #[test]
    fn budget_message_rounds_to_cents() {
        let err = Error::Budget {
            spent: 1.239,
            limit: 1.0,
        };
        assert_eq!(err.to_string(), "budget exhausted: spent $1.24 of $1.00");
    }

    #[test]
    fn path_error_leads_with_the_path() {
        let err = Error::path("/etc/nope", "not found");
        assert_eq!(err.to_string(), "/etc/nope: not found");
    }
}
