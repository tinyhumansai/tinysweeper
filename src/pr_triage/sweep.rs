//! The read-only job: look at a repository's open pull requests and decide
//! which of them are worth a human's time.
//!
//! Holds a [`ForgeRead`] and nothing else, so — exactly as with a review lane —
//! it structurally cannot write to GitHub. What it produces is a list of
//! [`TriagePlan`]s, which `crate::pr_triage::apply` is the only module that
//! acts on.
//!
//! ## What it costs
//!
//! One list request per hundred pull requests, one changed-files request per
//! pull request, and — only for the ones that are not already duplicates — one
//! file read per changed file, bounded by `pr_triage.max_base_reads` across the
//! whole sweep. No model calls, at all: the sweep on a repository with a
//! hundred open pull requests costs nothing but rate limit.
//!
//! ## The order of the questions
//!
//! Duplicate first, superseded second, and that ordering is not arbitrary.
//! Deciding a pull request is a duplicate needs data the sweep already has in
//! memory, where deciding it is superseded needs a file read per changed file.
//! Asking the free question first is what keeps the budget for the pull
//! requests that actually need it.

use std::collections::BTreeMap;

use crate::config::types::Config;
use crate::error::Result;
use crate::forge::types::{ChangedFile, PullRequest, RepoId};
use crate::issues::labels::{LabelPolicy, plan as plan_labels};
use crate::ports::forge::ForgeRead;
use crate::pr_triage::comment;
use crate::pr_triage::dedupe::{Shape, duplicate_of};
use crate::pr_triage::gate::{self, Outcome};
use crate::pr_triage::landed::landed;
use crate::pr_triage::types::{TriagePlan, Verdict};

/// What one sweep produced.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SweepOutcome {
    /// One plan per pull request considered, in ascending number order.
    pub plans: Vec<TriagePlan>,
    /// Why the sweep did nothing, when it did nothing.
    pub skipped: Option<&'static str>,
}

/// Triage a repository's open pull requests.
///
/// `only` narrows the *output* to one pull request without narrowing the
/// input: the other open pull requests are still read, because a duplicate is a
/// statement about a pair and a sweep that could only see one of them would
/// find nothing. That is the single-pull-request path a webhook uses.
pub async fn sweep(
    read: &dyn ForgeRead,
    config: &Config,
    repo: &RepoId,
    only: Option<u64>,
    maintainers: &[String],
) -> Result<SweepOutcome> {
    let policy = &config.pr_triage;
    if !policy.enabled {
        return Ok(SweepOutcome {
            skipped: Some("pr_triage.enabled is off"),
            ..SweepOutcome::default()
        });
    }

    let pulls = read
        .open_pull_requests(repo, policy.max_pull_requests)
        .await?;
    if pulls.is_empty() {
        return Ok(SweepOutcome {
            skipped: Some("the repository has no open pull requests"),
            ..SweepOutcome::default()
        });
    }

    // Ascending, so "the older one is the original" is a property of the loop
    // rather than of whatever order the forge answered in.
    let mut pulls = pulls;
    pulls.sort_by_key(|pull_request| pull_request.number);

    let mut files: BTreeMap<u64, Vec<ChangedFile>> = BTreeMap::new();
    for pull_request in &pulls {
        // One pull request whose files cannot be read must not sink the sweep:
        // it is read as "nothing comparable", which reaches `Verdict::Review`
        // and leaves the pull request exactly as it was.
        let changed = read
            .changed_files(repo, pull_request.number)
            .await
            .unwrap_or_default();
        files.insert(pull_request.number, changed);
    }

    let mut earlier: Vec<Shape> = Vec::new();
    let mut budget = policy.max_base_reads;
    let mut plans = Vec::new();

    for pull_request in &pulls {
        let changed = files.get(&pull_request.number).cloned().unwrap_or_default();
        let shape = Shape::of(pull_request.number, &changed);

        if only.is_none_or(|number| number == pull_request.number) {
            let verdict = verdict_for(
                read,
                config,
                repo,
                pull_request,
                &changed,
                &shape,
                &earlier,
                &mut budget,
            )
            .await;
            plans.push(build_plan(config, pull_request, verdict, maintainers));
        }

        // Pushed after the verdict, never before: a pull request must not be
        // able to be a duplicate of itself.
        earlier.push(shape);
    }

    Ok(SweepOutcome {
        plans,
        skipped: None,
    })
}

/// Decide what one pull request is, cheapest question first.
#[allow(clippy::too_many_arguments)]
async fn verdict_for(
    read: &dyn ForgeRead,
    config: &Config,
    repo: &RepoId,
    pull_request: &PullRequest,
    changed: &[ChangedFile],
    shape: &Shape,
    earlier: &[Shape],
    budget: &mut usize,
) -> Verdict {
    let policy = &config.pr_triage;

    if let Some((of, overlap)) = duplicate_of(
        shape,
        earlier,
        policy.duplicate_path_overlap_min,
        policy.duplicate_line_overlap_min,
    ) {
        return Verdict::Duplicate {
            of,
            path_overlap: overlap.paths,
            line_overlap: overlap.lines,
        };
    }

    if changed.is_empty() {
        return Verdict::Review {
            because: "its changed files could not be read",
        };
    }
    if changed.len() > policy.max_landed_files {
        return Verdict::Review {
            because: "it changes too many files to compare against the base branch",
        };
    }
    if changed.len() > *budget {
        return Verdict::Review {
            because: "the sweep's base-branch read budget ran out",
        };
    }

    let mut bases = Vec::with_capacity(changed.len());
    for file in changed {
        // Read at the base *branch*, not at `base_sha`. The question this
        // answers is "is this change on the branch today", which is a question
        // about the moving ref: a pull request opened six weeks ago carries a
        // `base_sha` from before the change it duplicates landed, and reading
        // there would answer "no" to every superseded pull request there is.
        let content = read
            .file_at(repo, &file.path, &pull_request.base_ref)
            .await
            .unwrap_or(None);
        bases.push(content);
        *budget -= 1;
    }

    match landed(changed, &bases, policy.min_landed_lines) {
        Ok(lines_checked) => Verdict::Superseded {
            base_ref: pull_request.base_ref.clone(),
            lines_checked,
        },
        Err(why) => Verdict::Review {
            because: why.reason(),
        },
    }
}

/// Turn a verdict into the plan that `apply` acts on.
///
/// Deterministic and I/O-free, so the whole label-and-close decision for one
/// pull request is testable without a forge.
pub fn build_plan(
    config: &Config,
    pull_request: &PullRequest,
    verdict: Verdict,
    maintainers: &[String],
) -> TriagePlan {
    let mut plan = TriagePlan::new(pull_request.number, verdict);

    // The kill-switch labels come from `[issues] block_labels` rather than a
    // second list of their own: an item two jobs disagree about leaving alone
    // is worse than one setting in one place.
    let label_policy =
        LabelPolicy::from(&config.pr_triage).blocking(&config.issues.block_labels);
    let planned = plan_labels(
        &pull_request.labels,
        &[plan.verdict.label().to_string()],
        label_policy,
    );
    plan.add_labels = planned.add;
    plan.remove_labels = planned.remove;
    plan.declined_labels = planned.declined;

    match gate::decide(gate::Inputs {
        subject: pull_request,
        verdict: &plan.verdict,
        maintainers,
        policy: &config.pr_triage.close,
    }) {
        Outcome::Close(close) => plan.close = Some(close),
        // A pull request that was never a close candidate gets no refusal
        // recorded: "we did not close the thing we were not considering
        // closing" is not a fact worth putting in a comment.
        Outcome::Refuse(reason) => {
            if plan.verdict.is_closeable() {
                plan.close_refusal = Some(reason);
            }
        }
    }

    if config.pr_triage.comment {
        plan.comment = comment::render(&plan);
    }

    plan
}

#[cfg(test)]
#[path = "sweep_test.rs"]
mod tests;
