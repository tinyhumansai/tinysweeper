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

    /// Review workers are at capacity and a manual request was refused before
    /// it spent credentials or read pull request data.
    #[error("review capacity is full; retry the manual request")]
    ReviewCapacityFull,

    /// A model call failed, timed out, or returned output that did not match
    /// the lane's structured-output schema.
    #[error("model: {0}")]
    Model(String),

    /// A refused model answer with safe accounting from paid attempts.
    #[error("model: {message}")]
    ModelUsage {
        /// Refusal description without rejected provider content.
        message: String,
        /// Known or conservatively bounded paid usage.
        usage: Box<crate::ports::model::Usage>,
    },

    /// A review-wide physical-dispatch policy stopped model work.
    #[error("model execution refused: {refusal:?}")]
    ModelExecutionRefused {
        /// Typed policy reason; callers must not infer it from display text.
        refusal: crate::ports::model_execution::ModelExecutionRefusal,
        /// Actual provider usage observed before the refusal, when any.
        usage: Option<Box<crate::ports::model::Usage>>,
        /// Whether accounting covers every paid dispatch in the whole
        /// logical operation. `None` plus `true` means no paid physical call
        /// occurred; `None` plus `false` means a call may have been paid but
        /// its usage is unknown. `Some(false)` retains known lower-bound usage
        /// when a dispatched call omitted tokens or cost. `Usage` has no
        /// unknown-cost representation, so this marker prevents a zero
        /// placeholder from being mistaken for an actual zero-dollar total.
        accounting_complete: bool,
    },

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
        /// Paid model usage from earlier turns, when this refusal follows them.
        usage: Option<Box<crate::ports::model::Usage>>,
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

    /// The selected large-review policy cannot be enforced by the model port.
    ///
    /// This is deterministic and non-transient: retrying the same model
    /// adapter cannot make its physical-dispatch capability appear.
    #[error("large review requires a model with physical execution control")]
    ReviewExecutionUnavailable,

    /// The pull request head changed after the server claimed a review lease.
    /// This is a clean skip: retry on the new head rather than reviewing it
    /// under a lease for a different revision.
    #[error("pull request head changed before the review started")]
    ReviewSuperseded,

    /// This process lost its durable scheduler ownership before a protected
    /// review phase could start or publish.
    #[error("review worker ownership expired or was replaced")]
    ReviewOwnershipLost,

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

impl Error {
    /// Paid model usage retained when an unsuccessful review has accounting.
    pub fn usage(&self) -> Option<crate::ports::model::Usage> {
        match self {
            Self::ModelUsage { usage, .. } => Some(**usage),
            Self::ModelExecutionRefused {
                usage: Some(usage), ..
            } => Some(**usage),
            Self::Budget {
                usage: Some(usage), ..
            } => Some(**usage),
            _ => None,
        }
    }

    /// Whether the model usage has complete token and cost accounting.
    ///
    /// Legacy errors carrying [`Usage`](crate::ports::model::Usage) retain
    /// their existing accounting contract. A typed execution refusal reports
    /// `Some(false)` when any dispatched call omitted usage or cost.
    pub fn usage_accounting_complete(&self) -> Option<bool> {
        match self {
            Self::ModelExecutionRefused {
                usage,
                accounting_complete,
                ..
            } if usage.is_some() => Some(*accounting_complete),
            _ => None,
        }
    }

    /// Keep safe paid accounting when a model cannot return an answer.
    pub fn with_usage(self, usage: crate::ports::model::Usage) -> Self {
        let error = match self {
            Self::Budget { spent, limit, .. } => {
                return Self::Budget {
                    spent,
                    limit,
                    usage: Some(Box::new(usage)),
                };
            }
            Self::ModelExecutionRefused {
                refusal,
                usage: _,
                accounting_complete,
            } => {
                return Self::ModelExecutionRefused {
                    refusal,
                    usage: Some(Box::new(usage)),
                    accounting_complete,
                };
            }
            other => other,
        };
        let message = match error {
            Self::Model(message) | Self::ModelUsage { message, .. } => message,
            other => other.to_string(),
        };
        Self::ModelUsage {
            message,
            usage: Box::new(usage),
        }
    }

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
            usage: None,
        };
        assert_eq!(err.to_string(), "budget exhausted: spent $1.24 of $1.00");
    }

    #[test]
    fn paid_budget_refusal_keeps_its_classification_and_usage() {
        let usage = crate::ports::model::Usage {
            input_tokens: 12,
            output_tokens: 4,
            cost_usd: 0.015,
            ..Default::default()
        };
        let err = Error::Budget {
            spent: 0.015,
            limit: 0.02,
            usage: None,
        }
        .with_usage(usage);
        assert!(matches!(err, Error::Budget { .. }));
        assert_eq!(err.usage(), Some(usage));
    }

    #[test]
    fn a_budget_refusal_without_paid_turns_has_no_usage() {
        let err = Error::Budget {
            spent: 0.0,
            limit: 0.02,
            usage: None,
        };
        assert_eq!(err.usage(), None);
    }

    #[test]
    fn execution_refusal_keeps_its_typed_reason_and_unknown_accounting() {
        use crate::ports::model_execution::ModelExecutionRefusal;

        let err = Error::ModelExecutionRefused {
            refusal: ModelExecutionRefusal::CallLimit,
            usage: Some(Box::new(crate::ports::model::Usage {
                input_tokens: 5,
                calls_attempted: 1,
                ..Default::default()
            })),
            accounting_complete: false,
        };

        assert!(matches!(
            &err,
            Error::ModelExecutionRefused {
                refusal: ModelExecutionRefusal::CallLimit,
                ..
            }
        ));
        assert_eq!(err.usage().unwrap().input_tokens, 5);
        assert_eq!(err.usage_accounting_complete(), Some(false));
    }

    #[test]
    fn replacing_usage_preserves_execution_refusal_classification() {
        use crate::ports::model_execution::ModelExecutionRefusal;

        let err = Error::ModelExecutionRefused {
            refusal: ModelExecutionRefusal::Cancelled,
            usage: Some(Box::new(crate::ports::model::Usage {
                input_tokens: 5,
                cost_usd: 0.125,
                calls_attempted: 1,
                calls_succeeded: 1,
                ..Default::default()
            })),
            accounting_complete: true,
        }
        .with_usage(crate::ports::model::Usage {
            output_tokens: 2,
            cost_usd: 0.25,
            ..Default::default()
        });

        assert!(matches!(
            &err,
            Error::ModelExecutionRefused {
                refusal: ModelExecutionRefusal::Cancelled,
                ..
            }
        ));
        let usage = err.usage().unwrap();
        assert_eq!(usage.input_tokens, 0);
        assert_eq!(usage.output_tokens, 2);
        assert_eq!(usage.cost_usd, 0.25);
        assert_eq!(err.usage_accounting_complete(), Some(true));
    }

    #[test]
    fn path_error_leads_with_the_path() {
        let err = Error::path("/etc/nope", "not found");
        assert_eq!(err.to_string(), "/etc/nope: not found");
    }
}
