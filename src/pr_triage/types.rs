//! Core types for pull request triage.
//!
//! The split mirrors `crate::issues::types`, with one difference that matters:
//! there is no advisory half. Issue triage has an [`IssueVerdict`] because a
//! model speaks first and deterministic code decides afterwards. Nothing speaks
//! first here — a [`Verdict`] *is* the deterministic conclusion, carrying the
//! evidence that produced it — so there is no untrusted structure to keep apart
//! from a trusted one.
//!
//! [`IssueVerdict`]: crate::issues::types::IssueVerdict

use serde::{Deserialize, Serialize};

/// What the sweep concluded about one pull request.
///
/// One axis, three values, and they are exclusive by construction: the sweep
/// checks for a duplicate first, then for a superseded change, and calls
/// everything else worth reading.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "verdict")]
pub enum Verdict {
    /// An older open pull request makes substantially the same change.
    Duplicate {
        /// The older pull request. Always a lower number than the subject.
        of: u64,
        /// Overlap of the two changed-path sets, 0..=1.
        path_overlap: f64,
        /// Overlap of the two added-line sets, 0..=1.
        line_overlap: f64,
    },
    /// Every line this pull request changes is already on the base branch.
    ///
    /// "Already implemented", stated as something checkable: applying this pull
    /// request would change nothing.
    Superseded {
        /// The branch its lines were found on.
        base_ref: String,
        /// The exact commit of that branch they were compared against.
        ///
        /// Named so the finding is reproducible: "already on `main`" is only
        /// checkable if it says *which* `main`, and the branch will have moved
        /// by the time anybody reads the comment.
        base_sha: String,
        /// How many substantive lines were checked. Below
        /// `pr_triage.min_landed_lines` this verdict is not reached at all.
        lines_checked: usize,
    },
    /// Nothing disqualifies it. A human should read it.
    Review {
        /// Why the cheaper verdicts did not apply, for the log and the comment.
        because: &'static str,
    },
}

impl Verdict {
    /// The `triage:` label this verdict wants present.
    ///
    /// One facet, three values, exclusive — so applying one retires the others
    /// through [`crate::issues::labels::plan`], and a pull request can never
    /// carry both `triage: duplicate` and `triage: review`.
    pub fn label(&self) -> &'static str {
        match self {
            Verdict::Duplicate { .. } => "triage: duplicate",
            Verdict::Superseded { .. } => "triage: superseded",
            Verdict::Review { .. } => "triage: review",
        }
    }

    /// Every label in the `triage:` facet, so the vocabulary has one
    /// definition and the facet has one owner.
    pub fn labels() -> [&'static str; 3] {
        ["triage: duplicate", "triage: superseded", "triage: review"]
    }

    /// The verdict's evidence, in one line, for a log or a terminal.
    ///
    /// The numbers rather than a restatement of the label: an operator reading
    /// a hundred rows wants to know *which* pull request a duplicate duplicates
    /// without opening it.
    pub fn detail(&self) -> String {
        match self {
            Verdict::Duplicate {
                of,
                path_overlap,
                line_overlap,
            } => format!(
                "of #{of} ({:.0}% of paths, {:.0}% of added lines)",
                path_overlap * 100.0,
                line_overlap * 100.0
            ),
            Verdict::Superseded {
                base_ref,
                base_sha,
                lines_checked,
            } => format!(
                "{lines_checked} lines already on `{base_ref}` at {}",
                short(base_sha)
            ),
            Verdict::Review { because } => because.to_string(),
        }
    }

    /// Whether this verdict is even a candidate for closing.
    ///
    /// `Review` never is, which is why the gate in [`crate::pr_triage::gate`]
    /// takes a verdict rather than a boolean: "worth reading" and "closeable"
    /// are not two independent facts that could contradict each other.
    pub fn is_closeable(&self) -> bool {
        !matches!(self, Verdict::Review { .. })
    }
}

/// Something a human should see *before* judging an item on its merits.
///
/// A second, independent axis. A pull request can be a duplicate and an
/// advertisement at once, and `Verdict` cannot say both — so this is its own
/// facet rather than a fourth verdict.
///
/// One variant today, and the enum exists anyway: "unrelated" and "needs a
/// human" are the obvious next two, and adding them to a type is a smaller
/// change than inventing a second labelling path when they arrive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Flag {
    /// It reads as an advertisement: a referral link, a vendor endpoint and a
    /// key, or product copy in a change that adds no code.
    ///
    /// **Advisory, always.** [`crate::pr_triage::gate::decide`] cannot close on
    /// a flag, because the honest form of this judgement is a judgement — the
    /// same integration is a real contribution to one repository and an
    /// advertisement on another.
    Promotional,
}

impl Flag {
    /// The `flag:` label this wants present.
    pub fn label(self) -> &'static str {
        match self {
            Flag::Promotional => "flag: promotional",
        }
    }

    /// Every label in the `flag:` facet, so the vocabulary has one definition.
    pub fn labels() -> [&'static str; 1] {
        [Flag::Promotional.label()]
    }
}

/// The first seven characters of a commit, as git renders one.
fn short(sha: &str) -> &str {
    &sha[..sha.len().min(7)]
}

/// A close that passed every deterministic guard.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClosePlan {
    /// The pull request to close.
    pub number: u64,
    /// The head commit the verdict was computed against.
    ///
    /// Carried so the close can be tied to the evidence that justified it. A
    /// sweep takes minutes; if the contributor pushes in that window, the diff
    /// the verdict was reached on is no longer the diff being closed, and
    /// `apply::revalidate` refuses rather than acting on evidence that has
    /// stopped describing the pull request.
    pub head_sha: String,
    /// Whether to stop short of the close itself and only say what would
    /// happen.
    pub dry_run: bool,
}

/// What deterministic code decided to do about one pull request.
///
/// As with [`crate::issues::types::TriagePlan`], note what cannot be expressed:
/// there is no field for merging, and no field for removing a label outside the
/// facet this bot owns.
#[derive(Debug, Clone, PartialEq)]
pub struct TriagePlan {
    /// The pull request this concerns.
    pub number: u64,
    /// What the sweep concluded.
    pub verdict: Verdict,
    /// What a human should see before judging it, and why.
    ///
    /// Independent of `verdict`, and never able to close: an item is flagged
    /// *and* triaged, not flagged *instead of* triaged.
    pub flags: Vec<(Flag, String)>,
    /// Labels to add, already filtered and capped.
    pub add_labels: Vec<String>,
    /// Labels superseded by one being added, in the same facet.
    pub remove_labels: Vec<String>,
    /// Suggestions that were refused, with why. For the log.
    pub declined_labels: Vec<(String, &'static str)>,
    /// The evidence comment, when one is to be posted.
    pub comment: Option<String>,
    /// The id of tinysweeper's own previous comment on this pull request, when
    /// there is one.
    ///
    /// The sweep is meant to be run over and over — that is what a sweep is —
    /// and a job that posts a fresh comment every pass would bury the
    /// conversation it is trying to help. One comment per pull request, edited
    /// forever, exactly as the review path does it.
    pub comment_id: Option<u64>,
    /// Whether to actually close, and on what terms.
    pub close: Option<ClosePlan>,
    /// Why the close was refused, when it was. For the log and the comment.
    pub close_refusal: Option<&'static str>,
}

impl TriagePlan {
    /// An empty plan for `number` carrying `verdict` and nothing else.
    pub fn new(number: u64, verdict: Verdict) -> Self {
        Self {
            number,
            verdict,
            flags: Vec::new(),
            add_labels: Vec::new(),
            remove_labels: Vec::new(),
            declined_labels: Vec::new(),
            comment: None,
            comment_id: None,
            close: None,
            close_refusal: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_verdict_labels_within_the_one_facet() {
        let verdicts = [
            Verdict::Duplicate {
                of: 1,
                path_overlap: 1.0,
                line_overlap: 1.0,
            },
            Verdict::Superseded {
                base_ref: "main".into(),
                base_sha: "abc1234def".into(),
                lines_checked: 9,
            },
            Verdict::Review { because: "-" },
        ];

        for verdict in verdicts {
            assert!(
                Verdict::labels().contains(&verdict.label()),
                "{} escaped the facet",
                verdict.label()
            );
        }
    }

    #[test]
    fn worth_reading_is_never_closeable() {
        assert!(!Verdict::Review { because: "-" }.is_closeable());
        assert!(
            Verdict::Superseded {
                base_ref: "main".into(),
                base_sha: "abc1234def".into(),
                lines_checked: 9,
            }
            .is_closeable()
        );
    }
}
