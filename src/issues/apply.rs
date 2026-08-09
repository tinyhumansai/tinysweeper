//! The write half of issue triage.
//!
//! The only module here that holds a [`ForgeWrite`], and it does no policy of
//! its own: it executes a [`TriagePlan`] that deterministic code already
//! decided on. The ordering is load-bearing — the comment goes up *before* the
//! close, so an issue is never closed with the evidence still in flight.

use crate::error::Result;
use crate::forge::types::RepoId;
use crate::issues::types::TriagePlan;
use crate::ports::forge::ForgeWrite;

/// Execute a triage plan.
pub async fn apply_plan(forge: &dyn ForgeWrite, repo: &RepoId, plan: &TriagePlan) -> Result<()> {
    if !plan.add_labels.is_empty() {
        forge
            .add_labels(repo, plan.number, &plan.add_labels)
            .await?;
    }

    // After the add, never before. If the process dies between the two, an item
    // carrying both labels is confusing; one carrying neither is a triage that
    // silently did nothing, and the first is easier to notice and to fix.
    for label in &plan.remove_labels {
        forge.remove_label(repo, plan.number, label).await?;
    }

    // Only ever set, never cleared: a plan carrying no type is an issue whose
    // type is either a human's or absent, and both are left exactly as found.
    if let Some(name) = &plan.set_issue_type {
        forge.set_issue_type(repo, plan.number, name).await?;
    }

    if let Some(body) = &plan.comment {
        forge.create_comment(repo, plan.number, body).await?;
    }

    // After the comment, and only when the plan is not a dry run: `dry_run`
    // means "say what you would have done", and a close here would make the
    // comment a lie.
    if let Some(close) = &plan.close
        && !close.dry_run
    {
        forge.close_issue(repo, plan.number).await?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::forge::mock::{MockForge, Write};
    use crate::issues::comment::MARKER;
    use crate::issues::types::{ClaimKind, ClosePlan};

    fn repo() -> RepoId {
        RepoId {
            owner: "acme".into(),
            name: "widget".into(),
        }
    }

    fn plan() -> TriagePlan {
        TriagePlan {
            number: 42,
            add_labels: vec!["priority: p2".into()],
            comment: Some(format!("{MARKER}\nLabelled `priority: p2`.")),
            ..TriagePlan::default()
        }
    }

    #[tokio::test]
    async fn labels_and_a_comment_are_written() {
        let forge = MockForge::new();
        apply_plan(&forge, &repo(), &plan()).await.expect("applies");

        assert_eq!(
            forge.writes(),
            vec![
                Write::Labels {
                    number: 42,
                    labels: vec!["priority: p2".into()],
                },
                Write::Comment {
                    number: 42,
                    body: format!("{MARKER}\nLabelled `priority: p2`."),
                },
            ]
        );
    }

    #[tokio::test]
    async fn a_planned_issue_type_is_written_before_the_comment_that_reports_it() {
        let forge = MockForge::new();
        let plan = TriagePlan {
            set_issue_type: Some("Bug".into()),
            ..plan()
        };
        apply_plan(&forge, &repo(), &plan).await.expect("applies");

        let writes = forge.writes();
        let typed = writes
            .iter()
            .position(|write| matches!(write, Write::IssueType { .. }))
            .expect("the type was set");
        let commented = writes
            .iter()
            .position(|write| matches!(write, Write::Comment { .. }))
            .expect("a comment was posted");
        assert!(typed < commented, "{writes:?}");
        assert_eq!(
            writes[typed],
            Write::IssueType {
                number: 42,
                name: "Bug".into(),
            }
        );
    }

    #[tokio::test]
    async fn a_plan_with_no_type_touches_the_type_field_at_all() {
        // The refusal path: an issue whose type was left alone must see no
        // PATCH, because writing one would overwrite whatever is there.
        let forge = MockForge::new();
        apply_plan(&forge, &repo(), &plan()).await.expect("applies");
        assert!(
            !forge
                .writes()
                .iter()
                .any(|write| matches!(write, Write::IssueType { .. }))
        );
    }

    #[tokio::test]
    async fn a_plan_with_nothing_in_it_writes_nothing() {
        let forge = MockForge::new();
        apply_plan(
            &forge,
            &repo(),
            &TriagePlan {
                number: 42,
                ..TriagePlan::default()
            },
        )
        .await
        .expect("applies");
        assert!(forge.wrote_nothing());
    }

    #[tokio::test]
    async fn the_evidence_comment_is_posted_before_the_close() {
        let forge = MockForge::new();
        let plan = TriagePlan {
            close: Some(ClosePlan {
                kind: ClaimKind::Duplicate,
                reference: 7,
                dry_run: false,
            }),
            ..plan()
        };
        apply_plan(&forge, &repo(), &plan).await.expect("applies");

        let writes = forge.writes();
        let comment = writes
            .iter()
            .position(|write| matches!(write, Write::Comment { .. }))
            .expect("a comment was posted");
        let closed = writes
            .iter()
            .position(|write| matches!(write, Write::IssueClosed { .. }))
            .expect("the issue was closed");
        assert!(
            comment < closed,
            "an issue closed before its evidence lands is a silent close"
        );
    }

    #[tokio::test]
    async fn a_dry_run_close_comments_but_does_not_close() {
        let forge = MockForge::new();
        let plan = TriagePlan {
            close: Some(ClosePlan {
                kind: ClaimKind::Duplicate,
                reference: 7,
                dry_run: true,
            }),
            ..plan()
        };
        apply_plan(&forge, &repo(), &plan).await.expect("applies");

        assert!(
            !forge
                .writes()
                .iter()
                .any(|write| matches!(write, Write::IssueClosed { .. })),
            "dry_run must never reach close_issue"
        );
        assert!(
            forge
                .writes()
                .iter()
                .any(|write| matches!(write, Write::Comment { .. }))
        );
    }

    #[tokio::test]
    async fn a_plan_that_supersedes_nothing_removes_nothing() {
        // The default and by far the common case. Removal happens only when a
        // label in an owned facet replaces another in that same facet.
        let forge = MockForge::new();
        apply_plan(&forge, &repo(), &plan()).await.expect("applies");
        assert!(
            !forge
                .writes()
                .iter()
                .any(|write| matches!(write, Write::LabelRemoved { .. }))
        );
    }

    #[tokio::test]
    async fn a_superseded_label_is_removed_after_its_replacement_is_added() {
        // Ordering is the point. Crashing between the two writes leaves an item
        // with both labels, which a human notices; the other order leaves it
        // with neither, which reads as a triage that did nothing.
        let superseding = TriagePlan {
            remove_labels: vec!["severity: high".into()],
            add_labels: vec!["severity: low".into()],
            comment: None,
            ..plan()
        };

        let forge = MockForge::new();
        apply_plan(&forge, &repo(), &superseding)
            .await
            .expect("applies");

        let writes = forge.writes();
        let added = writes
            .iter()
            .position(|w| matches!(w, Write::Labels { .. }))
            .expect("the new label was added");
        let removed = writes
            .iter()
            .position(|w| matches!(w, Write::LabelRemoved { .. }))
            .expect("the stale label was removed");
        assert!(added < removed, "{writes:?}");
    }
}
