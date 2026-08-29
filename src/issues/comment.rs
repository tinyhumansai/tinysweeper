//! The triage comment: the evidence a human checks in one click.
//!
//! Nothing is ever closed silently. If [`crate::issues::close::decide`] allowed
//! a close, the comment names the issue or merged pull request that justified
//! it and says how to undo it. If it refused but a duplicate was still
//! suggested, the comment cross-links it and leaves the issue open — which is
//! the common and correct outcome.

use std::fmt::Write as _;

use crate::issues::types::{ClaimKind, TriagePlan};

/// The marker that identifies tinysweeper's own triage comment.
///
/// Kept in the body so a later run can find and edit the same comment instead
/// of adding a second one, and so a human can see at a glance that a bot wrote
/// it.
pub const MARKER: &str = "<!-- tinysweeper:issue-triage -->";

/// Render the comment for a plan, or `None` when there is nothing worth saying.
///
/// `cross_link` is a probable duplicate that did **not** clear the close gate.
/// Mentioning it is the whole value of the dedupe half on the overwhelmingly
/// common path where nothing gets closed.
pub fn render(
    plan: &TriagePlan,
    summary: &str,
    cross_link: Option<u64>,
    promotion: Option<&str>,
) -> Option<String> {
    if plan.add_labels.is_empty()
        && plan.close.is_none()
        && cross_link.is_none()
        && promotion.is_none()
    {
        return None;
    }

    let mut body = format!("{MARKER}\n");
    if !summary.trim().is_empty() {
        let _ = write!(body, "\n{}\n", summary.trim());
    }

    // Before the labels, because it is the part a human is being asked to look
    // at. A label that accuses somebody has to carry its evidence and the way
    // to disagree with it, or it is just a slur with a colour.
    if let Some(why) = promotion {
        let _ = write!(
            body,
            "\nThis reads like self-promotion: it matched {why}. That is a set \
             of textual signals rather than a verdict, and nothing was closed on \
             account of it."
        );
        // Only where the label actually went on. Telling somebody to clear a
        // label that is not there sends them looking for something that does
        // not exist.
        let flagged = plan
            .add_labels
            .iter()
            .any(|label| label.eq_ignore_ascii_case(crate::pr_triage::Flag::Promotional.label()));
        if flagged {
            let _ = write!(
                body,
                " If the signals are wrong, say so and clear the label."
            );
        }
        let _ = writeln!(body);
    }

    if !plan.add_labels.is_empty() {
        let labels: Vec<String> = plan
            .add_labels
            .iter()
            .map(|label| format!("`{label}`"))
            .collect();
        let _ = write!(body, "\nLabelled {}.\n", labels.join(" "));
    }

    if let Some(close) = &plan.close {
        let evidence = match close.kind {
            ClaimKind::Duplicate => format!("a duplicate of #{}", close.reference),
            ClaimKind::Resolved => format!("fixed by #{}", close.reference),
        };
        if close.dry_run {
            let _ = write!(
                body,
                "\ntinysweeper would close this as {evidence}, but \
                 `issues.close.dry_run` is on, so nothing was changed.\n"
            );
        } else {
            let _ = write!(
                body,
                "\nClosing this as {evidence}. If that is wrong, reopen the \
                 issue — this was decided automatically and the reference above \
                 is the whole of the reasoning.\n"
            );
        }
    } else if let Some(number) = cross_link {
        let _ = write!(
            body,
            "\nThis may be related to #{number}. Left open either way — \
             tinysweeper only closes an issue when the match is unambiguous.\n"
        );
    }

    Some(body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::issues::types::ClosePlan;

    fn plan() -> TriagePlan {
        TriagePlan {
            number: 42,
            add_labels: vec!["priority: p2".into()],
            ..TriagePlan::default()
        }
    }

    #[test]
    fn a_promotional_flag_carries_its_evidence_and_the_way_to_disagree() {
        let body = render(
            &plan(),
            "Asks for support for a hosted service.",
            None,
            Some("a link carrying a referral or campaign parameter"),
        )
        .expect("a comment");
        assert!(body.contains("referral"));
        assert!(body.contains("a set of textual signals"));
        assert!(body.contains("nothing was closed"));
    }

    #[test]
    fn a_flag_alone_is_enough_to_be_worth_a_comment() {
        let empty = TriagePlan {
            number: 42,
            ..TriagePlan::default()
        };
        assert!(render(&empty, "", None, Some("marketing language")).is_some());
    }

    #[test]
    fn a_labelling_only_run_reports_the_labels_it_added() {
        let body = render(&plan(), "The editor crashes on save.", None, None).expect("a comment");
        assert!(body.starts_with(MARKER));
        assert!(body.contains("The editor crashes on save."));
        assert!(body.contains("`priority: p2`"));
    }

    #[test]
    fn nothing_to_say_produces_no_comment() {
        // A run that added no labels, found no duplicate and closed nothing has
        // no business posting. This is the difference between a triage bot and
        // a notification generator.
        let empty = TriagePlan {
            number: 42,
            ..TriagePlan::default()
        };
        assert_eq!(render(&empty, "", None, None), None);
    }

    #[test]
    fn a_close_names_the_duplicate_and_how_to_undo_it() {
        let plan = TriagePlan {
            close: Some(ClosePlan {
                kind: ClaimKind::Duplicate,
                reference: 7,
                dry_run: false,
            }),
            ..plan()
        };
        let body =
            render(&plan, "Same crash as the earlier report.", None, None).expect("a comment");
        assert!(body.contains("duplicate of #7"));
        assert!(
            body.contains("reopen"),
            "a close a human cannot undo in one click is not conservative"
        );
    }

    #[test]
    fn a_close_by_a_merged_pull_request_names_the_pull_request() {
        let plan = TriagePlan {
            close: Some(ClosePlan {
                kind: ClaimKind::Resolved,
                reference: 31,
                dry_run: false,
            }),
            ..plan()
        };
        let body = render(&plan, "Fixed upstream.", None, None).expect("a comment");
        assert!(body.contains("fixed by #31"));
    }

    #[test]
    fn a_dry_run_says_it_would_have_closed_rather_than_that_it_did() {
        let plan = TriagePlan {
            close: Some(ClosePlan {
                kind: ClaimKind::Duplicate,
                reference: 7,
                dry_run: true,
            }),
            ..plan()
        };
        let body = render(&plan, "Same crash.", None, None).expect("a comment");
        assert!(body.contains("would close"));
        assert!(!body.contains("Closing this"));
    }

    #[test]
    fn a_refused_close_still_cross_links_the_probable_duplicate() {
        // The common outcome, and the one the design is optimised for: say what
        // it looks like, change nothing.
        let body = render(&plan(), "Looks familiar.", Some(7), None).expect("a comment");
        assert!(body.contains("#7"));
        assert!(body.contains("related"));
        assert!(!body.contains("Closing"));
    }
}
