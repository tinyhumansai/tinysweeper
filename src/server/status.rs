//! The umbrella check: what a pull request is told while its review runs.
//!
//! Every other check tinysweeper publishes is a *verdict* — `tinysweeper/
//! critique` says what the critique lane found, and it exists only once that
//! lane has an answer. That leaves the interesting half of a review's life
//! unreported. Between a delivery arriving and the lanes finishing there can be
//! several minutes of model calls, and for all of it the pull request looked
//! exactly like one tinysweeper had never heard of.
//!
//! Contributors read that gap as a broken bot, and they are not wrong to: for
//! most of 2026-08-13 the bot *was* broken, and nothing on any pull request
//! distinguished "reviewing" from "will never review". [`failure`] closed half
//! of that — a review that fails now says so. This module closes the other
//! half: a review that has *started* says so too, immediately, before the first
//! model call.
//!
//! ## One check, three states
//!
//! `tinysweeper/review` is created in-progress when the delivery is accepted,
//! and the same run — by id, never a second POST — is concluded afterwards:
//!
//! | State | Conclusion | Meaning |
//! |---|---|---|
//! | in progress | `None` | accepted, lanes running |
//! | concluded | `Success` | the lanes ran; their own checks carry the verdicts |
//! | concluded | `ActionRequired` | the review could not run — see [`failure`] |
//!
//! `Success` here is deliberately *not* a statement about the code. It means
//! the review completed; whether the code is any good is what the lane checks
//! say. Conflating the two would make a repository with findings unable to
//! merge on a check that was only ever meant to report liveness.
//!
//! ## The obligation this creates
//!
//! `automerge::policy::check_refusal` refuses on **any** pending check, not
//! only a required one. That is the behaviour we want — nothing should merge
//! underneath a review that is still running — but it means an in-progress
//! check that never concludes stalls auto-merge on that commit forever. Every
//! path out of `routes::handle_review` therefore concludes it, including the
//! ones that fail, and `routes::ReviewStatus` exists so that obligation is
//! discharged in one place rather than at each `return`.
//!
//! [`failure`]: crate::server::failure

use crate::forge::types::{CheckConclusion, CheckRun};

/// The name of the umbrella check.
///
/// Deliberately *not* one of the lane names. A lane check reports a lane's
/// verdict, and a review that has not finished — or never reached the lanes at
/// all — has no verdict to report; reusing `tinysweeper/critique` here would
/// overwrite a real result from an earlier push with a liveness signal.
pub const CHECK_NAME: &str = "tinysweeper/review";

/// The check published the moment a review is accepted.
///
/// Published before any model call, and before the review has read anything
/// beyond the head SHA it is pinned to. That is the entire point: its value is
/// that it appears *early*, and a status that waited for work to be done would
/// report nothing a lane check does not already report later.
pub fn in_progress(head_sha: &str) -> CheckRun {
    CheckRun {
        name: CHECK_NAME.to_string(),
        head_sha: head_sha.to_string(),
        conclusion: None,
        title: "Reviewing this pull request…".to_string(),
        summary: "tinysweeper has picked up this pull request and is reviewing it. \
                  Findings appear as inline comments, and each lane publishes its own \
                  check when it finishes.\n\n\
                  This check reports only that the review is running. It is not a verdict \
                  on the code, and it will conclude either way — including if the review \
                  fails, which it will say explicitly rather than disappearing."
            .to_string(),
    }
}

/// The check published when the lanes have run.
///
/// `Success` reports that the review *completed*, not that the code passed —
/// see the module docs. `findings` is rendered so the check is worth reading on
/// its own, and because it is the one number that tells a contributor whether
/// to go looking at the inline comments.
pub fn completed(head_sha: &str, findings: usize) -> CheckRun {
    let title = match findings {
        0 => "Reviewed — nothing to report".to_string(),
        1 => "Reviewed — 1 finding".to_string(),
        many => format!("Reviewed — {many} findings"),
    };

    CheckRun {
        name: CHECK_NAME.to_string(),
        head_sha: head_sha.to_string(),
        conclusion: Some(CheckConclusion::Success),
        title,
        summary: format!(
            "The review ran to completion and reported {findings} finding(s).\n\n\
             **This check passing does not mean the code passed.** It reports that \
             tinysweeper ran, nothing more — each lane publishes its own check with its \
             own verdict, and those are what gate the merge."
        ),
    }
}

/// The check published when a run opened a status and then declined to review.
///
/// [`CheckConclusion::Neutral`] is the honest answer and the only one that is:
/// `Success` would claim a review that did not happen, and `Failure` would
/// accuse the code over a decision tinysweeper made about itself. Neutral is
/// what a lane reports when it did not apply, which is exactly this.
///
/// It does not block — `automerge::policy` reads `Neutral` as inapplicable —
/// so a pull request that legitimately went unreviewed is not held hostage by
/// the check that was only ever reporting liveness.
pub fn not_reviewed(head_sha: &str) -> CheckRun {
    CheckRun {
        name: CHECK_NAME.to_string(),
        head_sha: head_sha.to_string(),
        conclusion: Some(CheckConclusion::Neutral),
        title: "Not reviewed".to_string(),
        summary: "tinysweeper started on this commit and then stopped before reviewing it \
                  — the pull request became a draft, the author is not reviewed, or another \
                  worker took the commit.\n\n\
                  Nothing is wrong with the code, and nothing was checked. Use the \
                  repository's **Manual review** workflow to review it anyway."
            .to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_starting_check_is_pending_so_nothing_merges_underneath_it() {
        let check = in_progress("abc123");
        assert!(check.is_in_progress());

        // The gate reads it through `CheckStatus`, and refuses on *any*
        // pending check. That is what stops auto-merge racing a review that
        // has been accepted but has not yet found anything.
        let observed = crate::forge::types::CheckStatus {
            name: check.name.clone(),
            conclusion: check.conclusion,
        };
        assert!(observed.is_pending());
        assert!(!observed.is_green(), "a review in flight is not a pass");
    }

    #[test]
    fn every_state_reports_under_one_name() {
        // Three states of one check, not three checks. If these ever diverge
        // the pull request grows a row per state, and none of them supersedes
        // the others.
        let err = crate::error::Error::Model("gateway returned 403".into());
        assert_eq!(in_progress("abc123").name, CHECK_NAME);
        assert_eq!(completed("abc123", 0).name, CHECK_NAME);
        assert_eq!(
            crate::server::failure::check_run("abc123", &err).name,
            CHECK_NAME
        );
    }

    #[test]
    fn completing_is_terminal_and_does_not_block() {
        let check = completed("abc123", 3);
        assert_eq!(check.conclusion, Some(CheckConclusion::Success));
        assert!(!check.is_in_progress());
        assert!(
            !check.conclusion.is_some_and(CheckConclusion::blocks),
            "liveness must not block the merge; the lane checks carry the verdicts"
        );
    }

    #[test]
    fn a_completed_review_never_claims_the_code_is_fine() {
        // The trap this check would otherwise set: a green `tinysweeper/review`
        // sitting next to a red `tinysweeper/security`, read as a pass.
        for findings in [0, 1, 7] {
            let check = completed("abc123", findings);
            assert!(
                check.summary.contains("does not mean the code passed"),
                "a liveness pass must disclaim being a verdict"
            );
        }
    }

    #[test]
    fn declining_to_review_neither_claims_a_pass_nor_blames_the_code() {
        let check = not_reviewed("abc123");
        assert_eq!(check.conclusion, Some(CheckConclusion::Neutral));

        let observed = crate::forge::types::CheckStatus {
            name: check.name.clone(),
            conclusion: check.conclusion,
        };
        // Not a pass — nothing was reviewed.
        assert!(!observed.is_green());
        // Not an accusation, and not a merge blocker: the pull request did
        // nothing wrong, so it must not be stuck behind this.
        assert!(!observed.is_failing());
        assert!(observed.is_inapplicable());
    }

    #[test]
    fn every_terminal_state_actually_concludes() {
        // The obligation the in-progress check creates. If any of these were
        // left pending, auto-merge would refuse on that commit until somebody
        // pushed again.
        let err = crate::error::Error::Model("gateway returned 403".into());
        for check in [
            completed("abc123", 0),
            not_reviewed("abc123"),
            crate::server::failure::check_run("abc123", &err),
        ] {
            assert!(
                !check.is_in_progress(),
                "`{}` must not be left pending",
                check.title
            );
        }
    }

    #[test]
    fn the_title_counts_findings_and_gets_the_plural_right() {
        assert!(completed("abc", 0).title.contains("nothing to report"));
        assert!(completed("abc", 1).title.contains("1 finding"));
        assert!(completed("abc", 4).title.contains("4 findings"));
    }
}
