//! The triage comment: why this pull request was labelled, and — if it was
//! closed — the evidence, spelled out well enough to argue with.
//!
//! Nothing is closed silently. A contributor whose pull request is closed by a
//! bot is owed three things: what it concluded, what it checked to conclude it,
//! and how to say it was wrong. All three are in every comment this renders,
//! and the tone is deliberate — the person on the other end did the work in
//! good faith and usually just did not see the other pull request.

use std::fmt::Write as _;

use crate::pr_triage::types::{Flag, TriagePlan, Verdict};

/// The marker that identifies tinysweeper's own pull request triage comment.
///
/// Distinct from the issue triage marker so a run can find and edit *its own*
/// previous comment rather than a different job's.
pub const MARKER: &str = "<!-- tinysweeper:pr-triage -->";

/// Render the comment for a plan, or `None` when there is nothing worth saying.
///
/// A pull request that is merely worth reading gets no comment. The label
/// already says it, and a bot commenting "this looks fine" on a hundred pull
/// requests is the exact noise this repository's review policy exists to
/// prevent.
pub fn render(plan: &TriagePlan) -> Option<String> {
    // A pull request that is merely worth reading, with nothing flagged, gets
    // no comment. A flagged one does, whatever the verdict: the label says
    // *that* something was noticed and the comment has to say *what*, or the
    // label is an accusation with no evidence attached.
    if matches!(plan.verdict, Verdict::Review { .. }) && plan.flags.is_empty() {
        return None;
    }

    let mut body = format!("{MARKER}\n\n");

    for (flag, why) in &plan.flags {
        let headline = match flag {
            Flag::Promotional => "This reads like self-promotion",
        };
        let _ = write!(
            body,
            "**{headline}.** It matched {why}.\n\n\
             Those are textual signals, not a verdict — plenty of legitimate \
             contributions add an endpoint and a key. **Nothing is closed on \
             this basis**; a maintainer decides. If the signals are wrong, say \
             so and clear the `{}` label.\n\n",
            flag.label(),
        );
    }

    match &plan.verdict {
        Verdict::Duplicate {
            of,
            path_overlap,
            line_overlap,
        } => {
            let _ = write!(
                body,
                "This looks like the same change as #{of}, which was opened first.\n\n\
                 The two pull requests touch {} of the same files and share {} of \
                 the lines they add. That comparison is the whole of the reasoning \
                 — no model was asked, and the title and description were not read.\n",
                percent(*path_overlap),
                percent(*line_overlap),
            );
        }
        Verdict::Superseded {
            base_ref,
            lines_checked,
        } => {
            let _ = write!(
                body,
                "Every line this changes is already on `{base_ref}`.\n\n\
                 tinysweeper compared {lines_checked} changed lines against the \
                 current base branch: each block this adds is already there, and \
                 each block it removes is already gone. Applying this pull request \
                 would change nothing.\n",
            );
        }
        // Reachable now: a flagged pull request that is otherwise worth
        // reading gets the flag paragraph above and nothing more.
        Verdict::Review { .. } => {}
    }

    if !plan.add_labels.is_empty() {
        let labels: Vec<String> = plan
            .add_labels
            .iter()
            .map(|label| format!("`{label}`"))
            .collect();
        let _ = write!(body, "\nLabelled {}.\n", labels.join(" "));
    }

    match (&plan.close, plan.close_refusal) {
        (Some(close), _) if close.dry_run => {
            // Says what is true. `dry_run` skips the close and nothing else —
            // the label above was still applied and this comment was still
            // posted — so "nothing was changed" would be false every time the
            // verdict label was new.
            let _ = write!(
                body,
                "\ntinysweeper would close this, but `pr_triage.close.dry_run` \
                 is on, so it has been left open.\n"
            );
        }
        (Some(_), _) => {
            let _ = write!(
                body,
                "\nClosing it on that basis. **If this is wrong, say so and \
                 reopen it** — the comparison above is the entire argument, so \
                 it is easy to check and easy to disagree with. Thank you for \
                 the patch either way.\n"
            );
        }
        (None, Some(refusal)) => {
            // The refusals are the useful half. A maintainer who wants to know
            // why the sweep flagged something and then left it alone should not
            // have to read the server log to find out.
            let _ = write!(body, "\nLeft open: {refusal}. A human decides this one.\n");
        }
        (None, None) => {}
    }

    Some(body)
}

/// A ratio as a percentage, for a sentence rather than for a machine.
fn percent(ratio: f64) -> String {
    format!("{:.0}%", (ratio.clamp(0.0, 1.0) * 100.0).round())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pr_triage::types::ClosePlan;

    fn duplicate_plan() -> TriagePlan {
        let mut plan = TriagePlan::new(
            5798,
            Verdict::Duplicate {
                of: 5789,
                path_overlap: 1.0,
                line_overlap: 0.94,
            },
        );
        plan.add_labels = vec!["triage: duplicate".into()];
        plan
    }

    #[test]
    fn a_pull_request_worth_reading_gets_no_comment() {
        assert_eq!(
            render(&TriagePlan::new(1, Verdict::Review { because: "-" })),
            None
        );
    }

    #[test]
    fn a_flag_is_explained_even_when_the_verdict_is_worth_reading() {
        let mut plan = TriagePlan::new(1, Verdict::Review { because: "-" });
        plan.flags.push((
            Flag::Promotional,
            "a link carrying a referral or campaign parameter".into(),
        ));
        let body = render(&plan).expect("a flagged pull request is explained");
        assert!(body.contains("referral"));
        // A label that accuses somebody must carry the way to disagree with it.
        assert!(body.contains("`flag: promotional`"));
        assert!(body.contains("Nothing is closed on this basis"));
    }

    #[test]
    fn a_duplicate_comment_names_the_original_and_the_evidence() {
        let body = render(&duplicate_plan()).expect("a comment");
        assert!(body.starts_with(MARKER));
        assert!(body.contains("#5789"));
        assert!(body.contains("100%"));
        assert!(body.contains("94%"));
        assert!(body.contains("`triage: duplicate`"));
    }

    #[test]
    fn a_close_says_how_to_undo_it() {
        let mut plan = duplicate_plan();
        plan.close = Some(ClosePlan {
            number: 5798,
            dry_run: false,
        });
        let body = render(&plan).expect("a comment");
        assert!(body.contains("reopen it"));
    }

    #[test]
    fn a_dry_run_says_nothing_was_changed() {
        let mut plan = duplicate_plan();
        plan.close = Some(ClosePlan {
            number: 5798,
            dry_run: true,
        });
        let body = render(&plan).expect("a comment");
        assert!(body.contains("dry_run"));
        assert!(body.contains("left open"));
        // A dry run still labels and still comments, so it must not claim
        // otherwise.
        assert!(!body.contains("nothing was changed"));
        assert!(!body.contains("reopen it"));
    }

    #[test]
    fn a_refusal_is_reported_rather_than_swallowed() {
        let mut plan = duplicate_plan();
        plan.close_refusal = Some("it is a draft");
        let body = render(&plan).expect("a comment");
        assert!(body.contains("Left open: it is a draft"));
    }

    #[test]
    fn a_superseded_comment_names_the_branch_and_the_line_count() {
        let mut plan = TriagePlan::new(
            42,
            Verdict::Superseded {
                base_ref: "main".into(),
                lines_checked: 12,
            },
        );
        plan.add_labels = vec!["triage: superseded".into()];
        let body = render(&plan).expect("a comment");
        assert!(body.contains("`main`"));
        assert!(body.contains("12 changed lines"));
    }
}
