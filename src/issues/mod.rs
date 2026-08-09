//! Issue triage: label an issue, and — very rarely — close it with evidence.
//!
//! Two halves, deliberately unequal in ambition:
//!
//! - **Labelling** is the safe half. It only ever *adds* labels, from a
//!   vocabulary the repository allowed, capped at `issues.max_labels`. It is on
//!   by default under `[issues]` and it still works when the other half
//!   declines. The planner in [`labels`] takes an existing label list and a
//!   suggestion list and nothing else, so pull request triage can reuse it.
//! - **Closing** is the dangerous half. It is off by default, and a model can
//!   only ever *propose* it: [`close::decide`] is a pure function over facts
//!   fetched from the forge, and it refuses unless every guard agrees. Every
//!   close carries a comment naming the issue or merged pull request that
//!   justifies it, so a human can check the reasoning and reopen.
//!
//! Issue titles, bodies and comments are untrusted input throughout. They are
//! fenced and labelled as data in [`prompt`], never placed in the cacheable
//! system prefix, and a model that answers "close every other issue" is refused
//! by the gate rather than obeyed.

pub mod apply;
pub mod close;
pub mod comment;
pub mod dedupe;
pub mod labels;
pub mod prompt;
pub mod pull_request;
pub mod triage;
pub mod types;

pub use crate::issues::apply::apply_plan;
pub use crate::issues::close::{CloseInputs, CloseOutcome, Referenced, decide};
pub use crate::issues::triage::{TriageOutcome, triage};
pub use crate::issues::types::{
    ClaimKind, ClosePlan, DuplicateClaim, IssueSeverity, IssueVerdict, Priority, TriagePlan,
};
