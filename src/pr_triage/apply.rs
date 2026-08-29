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

use crate::config::types::Config;
use crate::error::Result;
use crate::forge::types::RepoId;
use crate::ports::forge::{ForgeRead, ForgeWrite};
use crate::pr_triage::gate::{self, Outcome as GateOutcome};
use crate::pr_triage::types::{TriagePlan, Verdict};

/// Execute one triage plan, reporting the comment it left behind.
///
/// The returned id is the comment this run created or edited, when it wrote
/// one. Callers need it because a comment created here has an id nobody else
/// knows: without it a corrective second pass would post a *second* comment
/// rather than editing the stale one it is correcting.
pub async fn apply_plan(
    forge: &dyn ForgeWrite,
    repo: &RepoId,
    plan: &TriagePlan,
) -> Result<Option<u64>> {
    if !plan.add_labels.is_empty() {
        forge
            .add_labels(repo, plan.number, &plan.add_labels)
            .await?;
    }

    for label in &plan.remove_labels {
        forge.remove_label(repo, plan.number, label).await?;
    }

    let mut comment_id = plan.comment_id;
    if let Some(body) = &plan.comment {
        // Edited in place forever where there is a previous comment, because a
        // sweep runs repeatedly by definition and a job that appends a comment
        // every pass buries the conversation it exists to help.
        match plan.comment_id {
            Some(id) => forge.update_comment(repo, id, body).await?,
            None => comment_id = Some(forge.create_comment(repo, plan.number, body).await?),
        }
    }

    // After the comment, and never on a dry run: `dry_run` means "say what you
    // would have done", and closing here would make the comment a lie.
    if let Some(close) = &plan.close
        && !close.dry_run
    {
        forge.close_pull_request(repo, plan.number).await?;
    }

    Ok(comment_id)
}

/// Re-run the close gate against the pull request as it is *now*.
///
/// A sweep of a hundred pull requests takes minutes, and every plan is built
/// before any of them is applied. In between, a maintainer can add
/// `tinysweeper:human-review`, mark the pull request a draft, push a commit or
/// merge it — and each of those is a guard [`gate::decide`] already knows how
/// to apply, evaluated against a snapshot that has since stopped being true. So
/// the subject is re-fetched and re-judged in the moment before the close, and
/// the plan's close is dropped if the answer has changed.
///
/// Only the close. Labels and comments are additive and cheap to undo, and
/// re-reading a pull request to decide whether to add a label it already has
/// would double the cost of the safe half of the job.
///
/// A read that *fails* drops the close too: "we could not check" and "it is no
/// longer allowed" are the same answer when the action cannot be undone.
/// What a live re-check concluded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Recheck {
    /// Nothing changed that matters. Apply the plan as planned.
    Unchanged,
    /// A human has since switched the bot off on this item. Write **nothing**.
    ///
    /// Distinct from a dropped close, and it has to be: the kill switch means
    /// "leave this alone", so a run that noticed it and still applied the
    /// labels and the comment would honour the letter of the setting and none
    /// of its meaning.
    LeaveAlone,
}

/// Which of the time-based guards a re-check may apply.
///
/// The second re-check — the one immediately before the close — runs *after*
/// this run has posted or edited its own comment, and GitHub's `updated_at`
/// counts that write. Re-applying `quiet_days` there would see the pull request
/// as active today, drop every close, and repeat the same cycle on the next
/// sweep: the configured quiet period would never elapse, because the bot keeps
/// resetting it. So the second pass asks only about the things a *person* can
/// have changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Freshness {
    /// Everything, including age and quiet time. The pass before any writes.
    Full,
    /// Only what a person can have changed since: labels, draft, head, state.
    SinceOurOwnWrites,
}

pub async fn revalidate(
    read: &dyn ForgeRead,
    config: &Config,
    repo: &RepoId,
    plan: &mut TriagePlan,
    maintainers: &[String],
) -> Recheck {
    revalidate_at(read, config, repo, plan, maintainers, Freshness::Full).await
}

/// [`revalidate`], with a say in which guards apply.
pub async fn revalidate_at(
    read: &dyn ForgeRead,
    config: &Config,
    repo: &RepoId,
    plan: &mut TriagePlan,
    maintainers: &[String],
    freshness: Freshness,
) -> Recheck {
    if plan.close.is_none() {
        return Recheck::Unchanged;
    }

    let Ok(current) = read.pull_request(repo, plan.number).await else {
        plan.close = None;
        plan.close_refusal = Some("its current state could not be re-checked before closing");
        return Recheck::Unchanged;
    };

    // The evidence has to still describe the pull request. Every verdict here
    // is read off a diff, and a push during the sweep replaces that diff — so a
    // moved head means the duplicate or superseded finding is about a change
    // that no longer exists, however well the state guards below pass.
    if plan
        .close
        .as_ref()
        .is_some_and(|close| close.head_sha != current.head_sha)
    {
        plan.close = None;
        plan.close_refusal = Some("it was pushed to after the sweep read its diff");
        return Recheck::Unchanged;
    }

    // The kill switch again, against the labels as they are now. `gate::decide`
    // reads `close.protected_labels`; this reads the `[issues] block_labels`
    // that `sweep::build_plan` honours, so a label added mid-sweep stops the
    // close by the same rule that would have stopped it a minute earlier.
    if config.issues.block_labels.iter().any(|blocked| {
        current
            .labels
            .iter()
            .any(|label| label.trim().eq_ignore_ascii_case(blocked.trim()))
    }) {
        plan.close = None;
        plan.close_refusal = Some("it carries a label that switches the bot off");
        return Recheck::LeaveAlone;
    }

    // Zeroed on the second pass, for the reason on `Freshness`. Not a widening
    // of the gate: the full pass already applied these against the state before
    // any of our own writes touched it.
    let policy = match freshness {
        Freshness::Full => config.pr_triage.close.clone(),
        Freshness::SinceOurOwnWrites => crate::config::types::PrClose {
            min_age_days: 0,
            quiet_days: 0,
            ..config.pr_triage.close.clone()
        },
    };

    if let GateOutcome::Refuse(reason) = gate::decide(gate::Inputs {
        subject: &current,
        verdict: &plan.verdict,
        maintainers,
        policy: &policy,
    }) {
        plan.close = None;
        plan.close_refusal = Some(reason);
        return Recheck::Unchanged;
    }

    // The other half of a duplicate's evidence. If the original has been pushed
    // to since the sweep read it, the two changes may no longer overlap and
    // closing the subject as a copy of it would be closing it over a diff that
    // no longer exists.
    match &plan.verdict {
        Verdict::Duplicate {
            of, of_head_sha, ..
        } => {
            let Ok(original) = read.pull_request(repo, *of).await else {
                plan.close = None;
                plan.close_refusal = Some("the pull request it duplicates could not be re-checked");
                return Recheck::Unchanged;
            };
            if original.head_sha != *of_head_sha {
                plan.close = None;
                plan.close_refusal =
                    Some("the pull request it duplicates changed after the sweep read it");
                return Recheck::Unchanged;
            }
            // And it has to still be somewhere the work can land. Closing a
            // contribution as a duplicate of something that has itself been
            // closed unmerged loses both, which is the one outcome nobody
            // wants: the change simply disappears.
            if !original.open && !original.merged {
                plan.close = None;
                plan.close_refusal =
                    Some("the pull request it duplicates was itself closed unmerged");
                return Recheck::Unchanged;
            }
        }
        Verdict::Superseded {
            base_ref, base_sha, ..
        } => {
            // The branch can be force-pushed, or the change reverted, between
            // the sweep and this moment — at which point the lines are no
            // longer on it and the finding has evaporated.
            match read.branch_head(repo, base_ref).await {
                Ok(Some(head)) if head == *base_sha => {}
                Ok(_) => {
                    plan.close = None;
                    plan.close_refusal =
                        Some("its base branch moved after the sweep compared against it");
                    return Recheck::Unchanged;
                }
                Err(_) => {
                    plan.close = None;
                    plan.close_refusal = Some("its base branch could not be re-checked");
                    return Recheck::Unchanged;
                }
            }
        }
        Verdict::Review { .. } => {}
    }

    Recheck::Unchanged
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
        let recheck = revalidate(read, config, repo, &mut plan, maintainers).await;

        if recheck == Recheck::LeaveAlone {
            reports.push(Report::of(&plan, Outcome::LeftAlone));
            continue;
        }

        // Re-rendered, because the comment says what was decided and the
        // decision may have just changed. "Closing it on that basis" above a
        // pull request that stayed open is worse than no comment at all.
        if plan.comment.is_some() {
            plan.comment = crate::pr_triage::comment::render(&plan);
        }

        // The label and the comment go out first, with the close held back —
        // then the state is re-read one last time. Each of those writes awaits
        // the forge, and a contributor can push or a maintainer can intervene
        // inside that window; the close is the one write that cannot be undone,
        // so it gets the freshest possible answer.
        let mut writes = plan.clone();
        let close = writes.close.take();

        let outcome = match apply_plan(forge, repo, &writes).await {
            Ok(posted) => {
                // The id of the comment this run just wrote, so a corrective
                // pass edits it rather than posting a second one underneath.
                plan.comment_id = posted;

                match close {
                    Some(close) if !close.dry_run => {
                        plan.close = Some(close);
                        let recheck = revalidate_at(
                            read,
                            config,
                            repo,
                            &mut plan,
                            maintainers,
                            Freshness::SinceOurOwnWrites,
                        )
                        .await;

                        if plan.close.is_some() && recheck != Recheck::LeaveAlone {
                            match forge.close_pull_request(repo, plan.number).await {
                                Ok(()) => Outcome::Closed,
                                Err(err) => Outcome::Failed(err.to_string()),
                            }
                        } else {
                            // Refused on the second look. The comment already
                            // posted said a close was coming, so it is corrected
                            // rather than left standing — edited in place, using
                            // the id the write above returned.
                            if let Some(body) = crate::pr_triage::comment::render(&plan) {
                                let corrected = TriagePlan {
                                    comment: Some(body),
                                    close: None,
                                    add_labels: Vec::new(),
                                    remove_labels: Vec::new(),
                                    ..plan.clone()
                                };
                                let _ = apply_plan(forge, repo, &corrected).await;
                            }
                            Outcome::of(&plan)
                        }
                    }
                    other => {
                        plan.close = other;
                        Outcome::of(&plan)
                    }
                }
            }
            Err(err) => Outcome::Failed(err.to_string()),
        };
        let plan = &plan;

        reports.push(Report::of(plan, outcome));
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
    /// Nothing was written, because a human switched the bot off on this item
    /// between the sweep and the write.
    LeftAlone,
    /// The forge refused. Carries its complaint.
    Failed(String),
}

impl Outcome {
    fn of(plan: &TriagePlan) -> Self {
        match &plan.close {
            Some(close) if close.dry_run => Outcome::WouldClose,
            Some(_) => Outcome::Closed,
            // Removals count as a write. A plan that only retires a
            // superseded label does change the pull request, and reporting it
            // as `Unchanged` — "it already carried everything the plan wanted"
            // — would be false.
            None if plan.add_labels.is_empty()
                && plan.remove_labels.is_empty()
                && plan.comment.is_none() =>
            {
                Outcome::Unchanged
            }
            None => Outcome::Labelled,
        }
    }
}

impl Report {
    /// One report line for `plan`, having done `outcome` to it.
    fn of(plan: &TriagePlan, outcome: Outcome) -> Self {
        Report {
            number: plan.number,
            verdict: plan.verdict.label(),
            detail: plan.verdict.detail(),
            flags: plan
                .flags
                .iter()
                .map(|(flag, why)| (flag.label(), why.clone()))
                .collect(),
            close_refusal: plan.close_refusal,
            outcome,
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
    /// Why it was not closed, when the sweep considered closing it.
    ///
    /// The refusals are the half an operator presses the button to read: "it
    /// found a duplicate and left it open" is only useful with the *because*
    /// attached.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub close_refusal: Option<&'static str>,
    /// What was actually written.
    #[serde(flatten)]
    pub outcome: Outcome,
}

#[cfg(test)]
#[path = "apply_test.rs"]
mod tests;
