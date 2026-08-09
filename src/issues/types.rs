//! Core types for issue triage.
//!
//! The split here is the whole design. [`IssueVerdict`] is what a model said —
//! advisory, untrusted, and never sufficient on its own. [`TriagePlan`] is what
//! deterministic code decided, and it is the only thing `apply` will act on.

use serde::{Deserialize, Serialize};

/// How urgent the issue is, in the order a triager reads them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Priority {
    /// Drop everything.
    P0,
    /// Next.
    P1,
    /// Soon.
    P2,
    /// Someday.
    P3,
}

impl Priority {
    /// The label tinysweeper applies for this priority.
    pub fn label(self) -> &'static str {
        match self {
            Priority::P0 => "priority: p0",
            Priority::P1 => "priority: p1",
            Priority::P2 => "priority: p2",
            Priority::P3 => "priority: p3",
        }
    }

    /// One step less urgent, saturating at the bottom.
    ///
    /// Saturating rather than wrapping on purpose: a demotion must never turn
    /// the least urgent item into the most urgent one.
    pub fn demoted(self) -> Priority {
        match self {
            Priority::P0 => Priority::P1,
            Priority::P1 => Priority::P2,
            Priority::P2 | Priority::P3 => Priority::P3,
        }
    }

    /// Every priority label, so the label vocabulary has one definition.
    pub fn labels() -> [&'static str; 4] {
        [
            Priority::P0.label(),
            Priority::P1.label(),
            Priority::P2.label(),
            Priority::P3.label(),
        ]
    }
}

/// How bad the reported problem is when it happens.
///
/// Distinct from [`Priority`] on purpose: a data-loss bug behind a flag nobody
/// has enabled is critical *and* low priority, and collapsing the two loses the
/// only interesting thing triage has to say about it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum IssueSeverity {
    /// Data loss, a security hole, or an outage.
    Critical,
    /// A broken core path with no workaround.
    High,
    /// Broken with a workaround.
    Medium,
    /// Cosmetic or an enhancement.
    Low,
}

impl IssueSeverity {
    /// The label tinysweeper applies for this severity.
    pub fn label(self) -> &'static str {
        match self {
            IssueSeverity::Critical => "severity: critical",
            IssueSeverity::High => "severity: high",
            IssueSeverity::Medium => "severity: medium",
            IssueSeverity::Low => "severity: low",
        }
    }

    /// Every severity label.
    pub fn labels() -> [&'static str; 4] {
        [
            IssueSeverity::Critical.label(),
            IssueSeverity::High.label(),
            IssueSeverity::Medium.label(),
            IssueSeverity::Low.label(),
        ]
    }
}

/// What kind of work an issue is: the classification behind GitHub's native
/// issue type.
///
/// Three variants and no `Other`, because the mapping in
/// [`crate::issues::kind`] must be total in one direction and empty in the
/// other: a classification outside these three sets no type at all rather than
/// the nearest one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum IssueKind {
    /// Something is broken.
    Bug,
    /// A request, an idea, or new functionality.
    Feature,
    /// A specific piece of work that is neither.
    Task,
}

impl IssueKind {
    /// The classification word, which is also the issue type name matched on.
    pub fn as_str(self) -> &'static str {
        match self {
            IssueKind::Bug => "bug",
            IssueKind::Feature => "feature",
            IssueKind::Task => "task",
        }
    }
}

/// A model's claim that this issue is already answered somewhere else.
///
/// Every field is a *suggestion*. `number` in particular is checked against the
/// candidates we put in front of the model and then re-fetched from the forge
/// before it can close anything — a model that invents issue 9999 gets refused
/// by [`crate::issues::close::decide`], not believed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DuplicateClaim {
    /// What kind of prior art the model says it found.
    pub kind: ClaimKind,
    /// The issue or pull request number it named.
    pub number: u64,
    /// How sure it says it is, 0..=1.
    pub confidence: f64,
}

/// The two shapes of prior art that can justify a close.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClaimKind {
    /// The same report already exists as an older issue.
    Duplicate,
    /// A merged pull request already fixed it.
    Resolved,
}

/// Everything a model returned about one issue. Advisory in full.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct IssueVerdict {
    /// What kind of work it says this is, if it committed to one.
    pub kind: Option<IssueKind>,
    /// Suggested priority, if it committed to one.
    pub priority: Option<Priority>,
    /// Suggested severity, if it committed to one.
    pub severity: Option<IssueSeverity>,
    /// One sentence for the triage comment.
    pub summary: String,
    /// Its claim of prior art, if any.
    pub claim: Option<DuplicateClaim>,
}

/// What deterministic code decided to do about one issue.
///
/// Note what is *not* here: there is no way to express removing a label. The
/// rule "never remove a label a human applied" is enforced by the type rather
/// than by remembering it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TriagePlan {
    /// The issue this concerns.
    pub number: u64,
    /// Labels to add, in order, already capped and filtered.
    pub add_labels: Vec<String>,
    /// Labels superseded by one being added, in the same facet.
    ///
    /// Never a label outside the `priority:`/`severity:` facets this bot owns:
    /// two severities on one item are a contradiction, not two opinions, but a
    /// human's own labels are not ours to retire.
    pub remove_labels: Vec<String>,
    /// Labels that were suggested and refused, with why. For the log.
    pub declined_labels: Vec<(String, &'static str)>,
    /// The native issue type to set, by name, when one is to be set at all.
    ///
    /// Only ever `Some` for an issue that carries no type yet: the field is
    /// singular, so writing it destroys a human's choice rather than adding to
    /// it, and [`crate::issues::kind::plan`] refuses on that ground first.
    pub set_issue_type: Option<String>,
    /// Why no type was set, when none was. For the log.
    pub declined_issue_type: Option<&'static str>,
    /// The triage comment body, when one is to be posted.
    pub comment: Option<String>,
    /// Whether to actually close, and on what evidence.
    pub close: Option<ClosePlan>,
    /// Why the close was refused, when it was. For the log and the comment.
    pub close_refusal: Option<&'static str>,
}

/// A close that passed every deterministic guard.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClosePlan {
    /// Duplicate or resolved.
    pub kind: ClaimKind,
    /// The verified issue or merged pull request that justifies it.
    pub reference: u64,
    /// Whether to stop short of the actual close.
    pub dry_run: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn demotion_saturates_rather_than_wrapping() {
        assert_eq!(Priority::P0.demoted(), Priority::P1);
        assert_eq!(Priority::P3.demoted(), Priority::P3);
    }
}
