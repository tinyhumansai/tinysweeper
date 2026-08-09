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
    async fn no_label_is_ever_removed() {
        // `TriagePlan` cannot express a removal, so this asserts the property
        // end to end: whatever a plan says, `remove_label` is not called.
        let forge = MockForge::new();
        apply_plan(&forge, &repo(), &plan()).await.expect("applies");
        assert!(
            !forge
                .writes()
                .iter()
                .any(|write| matches!(write, Write::LabelRemoved { .. }))
        );
    }
}
