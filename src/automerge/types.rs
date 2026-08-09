//! What the auto-merge policy decided, and why.
//!
//! Every refusal is a named variant rather than a bare `false`, because the
//! reason is the product: an operator watching a pull request sit unmerged
//! needs to know which threshold it missed, and a test proving a refusal has
//! to assert on the specific one or it proves nothing.

use std::fmt;

/// The outcome of evaluating the deterministic policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Every criterion passed. Still subject to a live re-validation of the
    /// head SHA before anything is merged.
    Merge,
    /// Leave it alone, for this reason.
    Refuse(Refusal),
}

impl Decision {
    /// Whether this decision permits a merge.
    pub fn is_merge(&self) -> bool {
        matches!(self, Decision::Merge)
    }

    /// The refusal, if this is one.
    pub fn refusal(&self) -> Option<&Refusal> {
        match self {
            Decision::Refuse(refusal) => Some(refusal),
            Decision::Merge => None,
        }
    }
}

/// Why a pull request was left alone.
///
/// A wrongly-merged pull request cannot be un-merged; one left alone merely
/// waits for a human. Every variant here is that trade being taken.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// `automerge.enabled = false`.
    Disabled,
    /// The policy itself could not be compiled — a malformed glob, say.
    ///
    /// Fails closed on purpose: a threshold that cannot be read means "do not
    /// merge", never "assume fine".
    UnreadablePolicy(String),
    /// The pull request is a draft.
    Draft,
    /// It carries a blocking label.
    BlockedLabel(String),
    /// `allow_labels` is non-empty and it carries none of them.
    MissingAllowLabel,
    /// GitHub does not report it as mergeable. `None` — still computing —
    /// counts as "not", because unknown is not yes.
    NotMergeable,
    /// A review still stands asking for changes.
    ChangesRequested {
        /// Who asked. Bots count.
        reviewer: String,
    },
    /// Fewer human approvals than required.
    NotEnoughApprovals {
        /// How many there are.
        have: u32,
        /// How many the policy wants.
        want: u32,
    },
    /// A check on the head SHA concluded in a way that blocks.
    CheckFailing {
        /// The check run's name.
        name: String,
    },
    /// A check on the head SHA has not concluded yet.
    CheckPending {
        /// The check run's name.
        name: String,
    },
    /// A check named in `require_checks` has reported nothing on the head SHA.
    RequiredCheckMissing {
        /// The check run's name.
        name: String,
    },
    /// A check named in `require_checks` reported, but not a verdict.
    ///
    /// `Skipped` is the case: the check could not run, so the gate it was
    /// named for has no evidence behind it. Distinct from
    /// [`Refusal::RequiredCheckMissing`] because the two need different
    /// fixes — one is a check that vanished, the other is a check that ran
    /// and declined — and telling an operator "has not reported" about a
    /// check sitting in front of them on the page is worse than saying
    /// nothing.
    RequiredCheckInconclusive {
        /// The check run's name.
        name: String,
    },
    /// The pull request changed no files, so there is nothing to judge.
    NoChangedFiles,
    /// More files than `max_files`.
    TooManyFiles {
        /// The count.
        files: usize,
        /// The cap.
        max: usize,
    },
    /// A path that always wants a human.
    SensitivePath {
        /// The first matching path, in the order the forge listed them.
        path: String,
    },
    /// A complexity signal over its threshold.
    TooComplex {
        /// Which signal: `changed_lines`, `hunks`, `directories`.
        signal: &'static str,
        /// What it measured.
        value: u64,
        /// The threshold it passed.
        max: u64,
    },
    /// A complexity signal could not be measured at all.
    ///
    /// A file the forge gave no patch for — too large, or binary — has no
    /// countable hunks, and a cap that cannot be evaluated is not a cap.
    Unmeasurable {
        /// The path that could not be measured.
        path: String,
    },
    /// The head moved between the decision and the merge.
    HeadMoved {
        /// What was evaluated.
        evaluated: String,
        /// What is live now.
        live: String,
    },
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Refusal::Disabled => write!(f, "auto-merge is off"),
            Refusal::UnreadablePolicy(why) => {
                write!(f, "the auto-merge policy is unreadable: {why}")
            }
            Refusal::Draft => write!(f, "it is a draft"),
            Refusal::BlockedLabel(label) => write!(f, "it carries the blocking label `{label}`"),
            Refusal::MissingAllowLabel => write!(f, "it carries none of the required labels"),
            Refusal::NotMergeable => write!(f, "GitHub does not report it as mergeable"),
            Refusal::ChangesRequested { reviewer } => {
                write!(f, "`{reviewer}` still asks for changes")
            }
            Refusal::NotEnoughApprovals { have, want } => {
                write!(f, "{have} of {want} required approvals")
            }
            Refusal::CheckFailing { name } => write!(f, "the check `{name}` is not green"),
            Refusal::CheckPending { name } => write!(f, "the check `{name}` is still running"),
            Refusal::RequiredCheckMissing { name } => {
                write!(f, "the required check `{name}` has not reported")
            }
            Refusal::RequiredCheckInconclusive { name } => {
                write!(f, "the required check `{name}` was skipped, so it is not evidence of a pass")
            }
            Refusal::NoChangedFiles => write!(f, "it changes no files"),
            Refusal::TooManyFiles { files, max } => {
                write!(f, "it changes {files} files, over the cap of {max}")
            }
            Refusal::SensitivePath { path } => write!(f, "it touches the sensitive path `{path}`"),
            Refusal::TooComplex { signal, value, max } => {
                write!(f, "{signal} is {value}, over the cap of {max}")
            }
            Refusal::Unmeasurable { path } => {
                write!(f, "the diff for `{path}` could not be measured")
            }
            Refusal::HeadMoved { evaluated, live } => {
                write!(f, "the head moved from {evaluated} to {live}")
            }
        }
    }
}

/// What the job did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// It was merged.
    Merged {
        /// The method the forge accepted.
        method: String,
    },
    /// It was left alone.
    Refused(Refusal),
    /// Policy said yes and the forge said no — a merge method the repository
    /// has disabled, a race with a human, a protected branch rule.
    ///
    /// Not an error: the pull request is exactly where it was, which is the
    /// safe state, and the next run will try again.
    Rejected {
        /// The method that was attempted.
        method: String,
        /// What the forge said.
        reason: String,
    },
}
