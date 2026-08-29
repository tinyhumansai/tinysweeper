//! The write half of pull request triage.
//!
//! The only module here that holds a [`ForgeWrite`], and it makes no decisions
//! of its own: it executes a [`TriagePlan`] deterministic code already settled
//! on. Two orderings are load-bearing, and both exist because of what a partial
//! run leaves behind:
//!
//! - the label goes on **before** the old one comes off, so an interrupted run
//!   leaves a pull request carrying two labels — visibly wrong — rather than
//!   none, which reads as a sweep that did nothing;
//! - the comment goes up **before** the close, so a pull request is never
//!   closed with the explanation still in flight.
//!
//! Nothing here can merge. [`ForgeWrite::merge`] needs a `MergeApproved` that
//! only the auto-merge policy can mint, and this module never sees one.

use crate::error::Result;
use crate::forge::types::RepoId;
use crate::ports::forge::ForgeWrite;
use crate::pr_triage::types::TriagePlan;

/// Execute one triage plan.
pub async fn apply_plan(forge: &dyn ForgeWrite, repo: &RepoId, plan: &TriagePlan) -> Result<()> {
    if !plan.add_labels.is_empty() {
        forge
            .add_labels(repo, plan.number, &plan.add_labels)
            .await?;
    }

    for label in &plan.remove_labels {
        forge.remove_label(repo, plan.number, label).await?;
    }

    if let Some(body) = &plan.comment {
        // Edited in place forever where there is a previous comment, because a
        // sweep runs repeatedly by definition and a job that appends a comment
        // every pass buries the conversation it exists to help.
        match plan.comment_id {
            Some(id) => forge.update_comment(repo, id, body).await?,
            None => {
                forge.create_comment(repo, plan.number, body).await?;
            }
        }
    }

    // After the comment, and never on a dry run: `dry_run` means "say what you
    // would have done", and closing here would make the comment a lie.
    if let Some(close) = &plan.close
        && !close.dry_run
    {
        forge.close_pull_request(repo, plan.number).await?;
    }

    Ok(())
}

/// Execute every plan in a sweep, reporting what happened to each.
///
/// One pull request going wrong is a report line, not an outage: a sweep over a
/// hundred pull requests that abandoned the other ninety-nine because the third
/// one was deleted mid-run would be useless in exactly the repositories it is
/// for.
///
/// `read` is used only by [`revalidate`], and only for the plans that close
/// something. Holding a read handle here is not a widening of the write half's
/// authority — reading mutates nothing — and the alternative is closing a pull
/// request against a snapshot that is minutes old.
pub async fn apply_all(
    read: &dyn ForgeRead,
    forge: &dyn ForgeWrite,
    config: &Config,
    repo: &RepoId,
    plans: &[TriagePlan],
    maintainers: &[String],
) -> Vec<Report> {
    let mut reports = Vec::with_capacity(plans.len());

    for plan in plans {
        let mut plan = plan.clone();
        revalidate(read, config, repo, &mut plan, maintainers).await;
        // Re-rendered, because the comment says what was decided and the
        // decision may have just changed. "Closing it on that basis" above a
        // pull request that stayed open is worse than no comment at all.
        if plan.comment.is_some() {
            plan.comment = crate::pr_triage::comment::render(&plan);
        }
        let plan = &plan;

        let outcome = match apply_plan(forge, repo, plan).await {
            Ok(()) => Outcome::of(plan),
            Err(err) => Outcome::Failed(err.to_string()),
        };
        reports.push(Report {
            number: plan.number,
            verdict: plan.verdict.label(),
            detail: plan.verdict.detail(),
            flags: plan
                .flags
                .iter()
                .map(|(flag, why)| (flag.label(), why.clone()))
                .collect(),
            outcome,
        });
    }

    reports
}

/// What happened to one pull request.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case", tag = "outcome", content = "detail")]
pub enum Outcome {
    /// It was closed.
    Closed,
    /// It would have been closed, but `pr_triage.close.dry_run` is on.
    WouldClose,
    /// It was labelled, commented on, or both, and left open.
    Labelled,
    /// Nothing was written: it already carried everything the plan wanted.
    Unchanged,
    /// The forge refused. Carries its complaint.
    Failed(String),
}

impl Outcome {
    fn of(plan: &TriagePlan) -> Self {
        match &plan.close {
            Some(close) if close.dry_run => Outcome::WouldClose,
            Some(_) => Outcome::Closed,
            None if plan.add_labels.is_empty() && plan.comment.is_none() => Outcome::Unchanged,
            None => Outcome::Labelled,
        }
    }
}

/// One line of a sweep's report.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Report {
    /// The pull request.
    pub number: u64,
    /// The label its verdict implies, which is also the shortest way to say
    /// what the sweep concluded.
    pub verdict: &'static str,
    /// The evidence behind the verdict, in one line.
    pub detail: String,
    /// Advisory flags raised, with what matched. Never a reason for a close.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub flags: Vec<(&'static str, String)>,
    /// What was actually written.
    #[serde(flatten)]
    pub outcome: Outcome,
}

#[cfg(test)]
#[path = "apply_test.rs"]
mod tests;
