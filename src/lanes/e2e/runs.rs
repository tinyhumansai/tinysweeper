//! What the end-to-end jobs did on this head, decided from check runs.
//!
//! Pure functions over a [`Harness`] and the [`CheckStatus`] list the forge
//! reported for the head SHA. Nothing here consults a model, and nothing a
//! model says can change the answer: "the job did not run" and "the job
//! failed" are facts the forge holds, republished on the `commits`-lane
//! principle — a verdict nobody can be talked out of.
//!
//! The timing is the awkward part. A review runs on the push; an e2e suite
//! takes twenty minutes; so when this is first computed the honest state of
//! most jobs is *pending*, and neither pass nor fail is a claim the lane can
//! make. [`settle`] is the second half: called again when a check completes,
//! with the same harness and fresh check runs, it decides whether every job
//! has now spoken.

use std::fmt::Write as _;

use serde::{Deserialize, Serialize};

use crate::config::types::{LaneId, Severity};
use crate::findings::types::Finding;
use crate::forge::types::{CheckConclusion, CheckStatus};
use crate::lanes::e2e::inventory::{Applies, Harness, Job, Workflow};

/// Where one e2e job stands on the head commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    /// Ran and concluded success.
    Passed,
    /// Ran and concluded failure, cancelled, timed out, or action required.
    Failed(CheckConclusion),
    /// Queued, in progress, or not yet reported though the trigger says it
    /// will be. Never a pass.
    Pending,
    /// Will not run on this pull request, and this is why.
    NotTriggered(Reason),
}

/// Why a job will not run for this pull request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reason {
    /// The workflow's `paths:` filter matched nothing this change touched.
    PathsExcluded,
    /// The workflow's `paths-ignore:` filter matched everything it touched.
    AllIgnored,
    /// The workflow does not run on pull requests at all.
    NotOnPullRequests(String),
    /// The job is gated on a label the pull request does not carry.
    LabelMissing(String),
    /// The forge reported the job as skipped: a job-level condition the
    /// outline could not read was false on this pull request.
    SkippedByCondition,
}

impl Reason {
    fn describe(&self) -> String {
        match self {
            Reason::PathsExcluded => {
                "its workflow's `paths:` filter matches nothing this pull request changed".into()
            }
            Reason::AllIgnored => {
                "its workflow's `paths-ignore:` filter matches everything this pull request changed"
                    .into()
            }
            Reason::NotOnPullRequests(on) => {
                format!("its workflow does not run on pull requests (it runs on: {on})")
            }
            Reason::LabelMissing(label) => {
                format!("it is gated on the `{label}` label, which this pull request does not carry")
            }
            Reason::SkippedByCondition => {
                "the forge reports it as skipped, so a job condition was false for this pull request"
                    .into()
            }
        }
    }
}

/// Replace prompt-facing, untrusted metadata without changing matching state.
pub fn scrub_for_render(run: &mut JobRun) {
    run.workflow = crate::scan::scrub(&run.workflow);
    run.job = crate::scan::scrub(&run.job);
    if let State::NotTriggered(reason) = &mut run.state {
        match reason {
            Reason::NotOnPullRequests(on) | Reason::LabelMissing(on) => {
                *on = crate::scan::scrub(on);
            }
            Reason::PathsExcluded | Reason::AllIgnored | Reason::SkippedByCondition => {}
        }
    }
}

/// One job's verdict, with what it belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobRun {
    /// The workflow file.
    pub workflow: String,
    /// The job's check-run name.
    pub job: String,
    /// Where the `paths:` filter sits, for anchoring a not-triggered finding.
    pub filter_line: Option<u64>,
    /// The verdict.
    pub state: State,
    /// Whether this job's workflow is `pull_request_target`.
    ///
    /// Its check run is not attached to this pull request's head SHA — as
    /// of GitHub's November 2025 change, `pull_request_target` executes
    /// (and reports check runs) against the repository's default branch
    /// tip, whatever that happens to be at run time, not against any
    /// commit of this pull request at all. `pending()` uses this to keep
    /// such a job out of the watch: `check_runs(repo, head_sha)` will never
    /// see its check run, so watching it would mean a `Neutral`
    /// `tinysweeper/e2e` that no completion event can ever settle.
    pub target: bool,
}

/// Whether `check` reports on the job named `job`.
///
/// GitHub names a job's check run after the job, appends the matrix values in
/// parentheses for a matrix job, and prefixes the calling job for a reusable
/// workflow. The name is matched exactly and as a prefix of those two forms;
/// a bare substring match would let `e2e` claim `e2e-lint`.
pub fn matches_check(check: &CheckStatus, job: &str) -> bool {
    let name = check.name.trim();
    name == job
        || name.starts_with(&format!("{job} ("))
        || name.starts_with(&format!("{job} /"))
        || name.ends_with(&format!(" / {job}"))
}

/// Decide every e2e job's state on the head.
pub fn job_runs(
    harness: &Harness,
    checks: &[CheckStatus],
    changed: &[String],
    labels: &[String],
) -> Vec<JobRun> {
    harness
        .jobs()
        .map(|(workflow, job)| JobRun {
            workflow: workflow.path.clone(),
            job: job.name.clone(),
            filter_line: filter_line(workflow),
            state: state_of(workflow, job, checks, changed, labels),
            target: matches!(
                workflow.trigger,
                crate::lanes::e2e::inventory::Trigger::PullRequest { target: true, .. }
            ),
        })
        .collect()
}

fn filter_line(workflow: &Workflow) -> Option<u64> {
    match &workflow.trigger {
        crate::lanes::e2e::inventory::Trigger::PullRequest { filter_line, .. } => *filter_line,
        crate::lanes::e2e::inventory::Trigger::Never(_) => None,
    }
}

fn state_of(
    workflow: &Workflow,
    job: &Job,
    checks: &[CheckStatus],
    changed: &[String],
    labels: &[String],
) -> State {
    // A check run that exists is the fact; the trigger analysis only explains
    // an absence. A job that ran despite a filter the outline misread must
    // be reported as what it did, not as what the outline expected.
    let reported: Vec<&CheckStatus> = checks
        .iter()
        .filter(|check| matches_check(check, &job.name))
        .collect();
    if !reported.is_empty() {
        // Several check runs for one job name is a matrix; the worst of them
        // is the job's verdict, and any still running keeps it pending.
        let mut worst: Option<CheckConclusion> = None;
        let mut skipped = false;
        for check in reported {
            match check.conclusion {
                None => return State::Pending,
                Some(conclusion) if conclusion.blocks() => worst = Some(conclusion),
                // Neutral is not a pass: it is the forge's own "this job
                // formed no verdict", the same as a skip from the lane's
                // point of view. Falling through to the catch-all below
                // would let a job that never actually tested anything read
                // as `Passed`.
                Some(CheckConclusion::Skipped | CheckConclusion::Neutral) => skipped = true,
                Some(_) => {}
            }
        }
        return match worst {
            Some(conclusion) => State::Failed(conclusion),
            None if skipped => State::NotTriggered(Reason::SkippedByCondition),
            None => State::Passed,
        };
    }

    match workflow.applies_to(changed) {
        Applies::PathsExcluded => State::NotTriggered(Reason::PathsExcluded),
        Applies::AllIgnored => State::NotTriggered(Reason::AllIgnored),
        Applies::NotOnPullRequests(on) => State::NotTriggered(Reason::NotOnPullRequests(on)),
        Applies::Yes => match &job.label_gate {
            Some(label) if !labels.iter().any(|l| l.eq_ignore_ascii_case(label)) => {
                State::NotTriggered(Reason::LabelMissing(label.clone()))
            }
            // The trigger says it runs and nothing has reported yet: GitHub
            // creates the check run when the job is queued, which can be
            // after the review starts. Waiting is the honest answer.
            _ => State::Pending,
        },
    }
}

/// The deterministic findings: one per job that will not run, one per job
/// that failed. Pending and passed jobs produce nothing.
pub fn findings(runs: &[JobRun]) -> Vec<Finding> {
    runs.iter()
        .filter_map(|run| match &run.state {
            State::NotTriggered(reason) => Some(Finding {
                lane: LaneId::E2e,
                // A path filter that excludes the change is a silent gap the
                // author almost certainly did not intend. A workflow that
                // never runs on pull requests, or wants a label, is a policy
                // — possibly deliberate, worth saying, not worth blocking on
                // by default.
                severity: match reason {
                    Reason::PathsExcluded | Reason::AllIgnored => Severity::High,
                    Reason::NotOnPullRequests(_)
                    | Reason::LabelMissing(_)
                    | Reason::SkippedByCondition => Severity::Medium,
                },
                confidence: 1.0,
                path: run.workflow.clone(),
                line: run.filter_line,
                end_line: None,
                rule: "e2e-not-triggered".into(),
                title: format!("End-to-end job `{}` will not run on this change", run.job),
                body: format!(
                    "`{}` in `{}` will not run for this pull request: {}. The change is \
                     merged without its end-to-end suite having seen it.",
                    run.job,
                    run.workflow,
                    reason.describe()
                ),
                suggestion: None,
                applicable: None,
                late: false,
                identity: None,
                corroboration: 1,
            }),
            State::Failed(conclusion) => Some(Finding {
                lane: LaneId::E2e,
                severity: Severity::High,
                confidence: 1.0,
                path: run.workflow.clone(),
                line: None,
                end_line: None,
                rule: "e2e-failed".into(),
                title: format!("End-to-end job `{}` did not pass on this head", run.job),
                body: format!(
                    "`{}` in `{}` concluded **{}** on this pull request's head commit.",
                    run.job,
                    run.workflow,
                    conclusion_name(*conclusion)
                ),
                suggestion: None,
                applicable: None,
                late: false,
                identity: None,
                corroboration: 1,
            }),
            State::Passed | State::Pending => None,
        })
        .collect()
}

/// The jobs still to hear from.
pub fn pending(runs: &[JobRun]) -> Vec<String> {
    runs.iter()
        // `target` jobs are excluded on purpose, not merely left out by
        // accident of never matching a check: their check run is not on
        // this pull request's head SHA at all (see `JobRun::target`), so
        // `check_runs(repo, head_sha)` — what `settle` reads — can never
        // find it and `settle` would then never conclude. Watching one
        // would mean a `tinysweeper/e2e` stuck `Neutral` forever, worse
        // than not watching it.
        .filter(|run| run.state == State::Pending && !run.target)
        .map(|run| run.job.clone())
        .collect()
}

/// Render the run table for the prompt and the check-run summary.
pub fn render(runs: &[JobRun], head_sha: &str) -> String {
    let mut out = format!("End-to-end jobs on this head ({}):\n", short(head_sha));
    if runs.is_empty() {
        out.push_str("- none: no e2e workflow in the tree\n");
        return out;
    }
    for run in runs {
        let state = match &run.state {
            State::Passed => "PASSED".to_string(),
            State::Failed(conclusion) => format!("FAILED ({})", conclusion_name(*conclusion)),
            State::Pending => "PENDING — not yet concluded".to_string(),
            State::NotTriggered(reason) => format!("NOT TRIGGERED — {}", reason.describe()),
        };
        let _ = writeln!(out, "- `{}` ({}): {state}", run.job, run.workflow);
    }
    out
}

fn short(sha: &str) -> &str {
    sha.get(..7).unwrap_or(sha)
}

/// The conclusion as GitHub spells it.
fn conclusion_name(conclusion: CheckConclusion) -> &'static str {
    match conclusion {
        CheckConclusion::Success => "success",
        CheckConclusion::Failure => "failure",
        CheckConclusion::ActionRequired => "action_required",
        CheckConclusion::Neutral => "neutral",
        CheckConclusion::Skipped => "skipped",
    }
}

/// What the review recorded so a later check completion can conclude the
/// lane without re-running it.
///
/// Stored in `ReviewedState`, keyed to the head SHA it was computed for: a
/// new push starts over, and a completion event for an older commit is
/// ignored rather than applied to the wrong review.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Watch {
    /// The head the review ran against.
    pub head_sha: String,
    /// The check-run names still to hear from when the review finished.
    pub jobs: Vec<String>,
    /// The lane summary the review published, to be carried into the
    /// settled check run above the final run table.
    pub summary: String,
    /// Whether the review's own findings already failed the lane. A failed
    /// static half stays failed whatever the jobs say.
    pub failed: bool,
    /// Distinguishes this watch from any other, even one that is otherwise
    /// byte-identical (same head, same jobs, same summary, same verdict).
    ///
    /// A same-head manual re-review (the `/admin/reviews` route can trigger
    /// one at any time) can save a new watch that happens to match the old
    /// one on every other field. `ReviewStateStore::clear_e2e_watch`
    /// compares the whole `Watch` so a settlement in flight for the old one
    /// cannot clear the new one out from under it — without a field that
    /// changes on every save regardless of content, "otherwise identical"
    /// would still compare equal and defeat that guard. `#[serde(default)]`
    /// so a record written before this field existed deserializes to an
    /// empty generation, which — correctly — never matches a freshly
    /// created watch's generation, rather than being read as a match.
    #[serde(default)]
    pub generation: String,
}

/// The settled verdict, once every watched job has concluded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settled {
    /// The conclusion to publish.
    pub conclusion: CheckConclusion,
    /// The check-run summary.
    pub summary: String,
}

/// Decide whether the watched jobs have all spoken, and with what.
///
/// `None` while any is still pending: nothing to publish yet. A watched job
/// that has no check run at all is still pending — it was expected to run
/// when the watch was written and nothing since says otherwise.
pub fn settle(watch: &Watch, checks: &[CheckStatus], fail_on: Severity) -> Option<Settled> {
    let mut failed: Vec<(String, CheckConclusion)> = Vec::new();
    let mut skipped: Vec<String> = Vec::new();
    let mut passed: Vec<String> = Vec::new();
    for job in &watch.jobs {
        let reported: Vec<&CheckStatus> = checks
            .iter()
            .filter(|check| matches_check(check, job))
            .collect();
        if reported.is_empty() || reported.iter().any(|check| check.conclusion.is_none()) {
            return None;
        }
        let conclusions: Vec<CheckConclusion> = reported
            .iter()
            .filter_map(|check| check.conclusion)
            .collect();
        if let Some(conclusion) = conclusions.iter().find(|c| c.blocks()) {
            failed.push((job.clone(), *conclusion));
        } else if conclusions
            .iter()
            .any(|c| matches!(c, CheckConclusion::Skipped | CheckConclusion::Neutral))
        {
            // Same reasoning as `state_of`: neutral is not an affirmative
            // pass, so it is settled the same way a skip is.
            skipped.push(job.clone());
        } else {
            passed.push(job.clone());
        }
    }

    let mut summary = watch.summary.trim().to_string();
    let _ = write!(
        summary,
        "\n\nEnd-to-end jobs on {}:",
        short(&watch.head_sha)
    );
    for job in &passed {
        let _ = write!(summary, "\n- `{}`: passed", crate::scan::scrub(job));
    }
    for job in &skipped {
        let _ = write!(
            summary,
            "\n- `{}`: **skipped** — did not run on this pull request",
            crate::scan::scrub(job)
        );
    }
    for (job, conclusion) in &failed {
        let _ = write!(
            summary,
            "\n- `{}`: **{}**",
            crate::scan::scrub(job),
            conclusion_name(*conclusion)
        );
    }

    // The same levels `findings` gives them: a failed job is High, a skipped
    // one Medium. Whether either fails the lane is the operator's `fail_on`,
    // the same dial the review itself used.
    let jobs_fail = (!failed.is_empty() && Severity::High >= fail_on)
        || (!skipped.is_empty() && Severity::Medium >= fail_on);
    let conclusion = if watch.failed || jobs_fail {
        CheckConclusion::Failure
    } else {
        CheckConclusion::Success
    };
    Some(Settled {
        conclusion,
        summary,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lanes::e2e::inventory::{Trigger, classify_workflow};

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    fn harness(paths: &[&str]) -> Harness {
        let text = format!(
            "name: e2e\non:\n  pull_request:\n    paths:\n{}\njobs:\n  playwright:\n    steps:\n      - run: npx playwright test\n",
            paths
                .iter()
                .map(|p| format!("      - '{p}'"))
                .collect::<Vec<_>>()
                .join("\n")
        );
        Harness {
            tests: strings(&["e2e/login.spec.ts"]),
            workflows: vec![classify_workflow(".github/workflows/e2e.yml", &text, &[]).unwrap()],
            truncated: false,
        }
    }

    fn check(name: &str, conclusion: Option<CheckConclusion>) -> CheckStatus {
        CheckStatus {
            name: name.into(),
            conclusion,
        }
    }

    #[test]
    fn a_reported_check_run_is_the_fact_whatever_the_filter_says() {
        let harness = harness(&["src/**"]);
        let runs = job_runs(
            &harness,
            &[check("playwright", Some(CheckConclusion::Success))],
            &strings(&["docs/x.md"]),
            &[],
        );
        assert_eq!(runs[0].state, State::Passed);
    }

    #[test]
    fn a_filtered_out_job_is_not_triggered_and_a_high_finding() {
        let harness = harness(&["src/server/**"]);
        let runs = job_runs(&harness, &[], &strings(&["src/preview/apply.rs"]), &[]);
        assert_eq!(runs[0].state, State::NotTriggered(Reason::PathsExcluded));
        let findings = findings(&runs);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].rule, "e2e-not-triggered");
        assert_eq!(findings[0].severity, Severity::High);
        assert_eq!(findings[0].path, ".github/workflows/e2e.yml");
        assert_eq!(findings[0].line, Some(4), "anchored on the `paths:` line");
        assert!(pending(&runs).is_empty());
    }

    #[test]
    fn an_expected_job_with_no_check_run_yet_is_pending_not_missing() {
        let harness = harness(&["src/**"]);
        let runs = job_runs(&harness, &[], &strings(&["src/main.rs"]), &[]);
        assert_eq!(runs[0].state, State::Pending);
        assert!(findings(&runs).is_empty());
        assert_eq!(pending(&runs), vec!["playwright".to_string()]);
    }

    #[test]
    fn a_pending_pull_request_target_job_is_never_watched() {
        // Its check run lands on GitHub's chosen default-branch tip, not
        // this pull request's head — `check_runs(repo, head_sha)` will
        // never see it, so watching it would mean a `tinysweeper/e2e` stuck
        // `Neutral` forever with nothing left to settle it.
        let text = "name: e2e\non: pull_request_target\njobs:\n  playwright:\n    steps:\n      - run: npx playwright test\n";
        let harness = Harness {
            tests: strings(&["e2e/login.spec.ts"]),
            workflows: vec![classify_workflow(".github/workflows/e2e.yml", text, &[]).unwrap()],
            truncated: false,
        };
        let runs = job_runs(&harness, &[], &strings(&["src/main.rs"]), &[]);
        assert_eq!(runs[0].state, State::Pending);
        assert!(runs[0].target);
        assert!(
            pending(&runs).is_empty(),
            "a target job must never be added to the watch"
        );
    }

    #[test]
    fn a_matrix_job_is_as_bad_as_its_worst_leg_and_pending_while_any_runs() {
        let harness = harness(&["src/**"]);
        let changed = strings(&["src/main.rs"]);
        let runs = job_runs(
            &harness,
            &[
                check("playwright (chromium)", Some(CheckConclusion::Success)),
                check("playwright (firefox)", Some(CheckConclusion::Failure)),
            ],
            &changed,
            &[],
        );
        assert_eq!(runs[0].state, State::Failed(CheckConclusion::Failure));
        assert_eq!(findings(&runs)[0].rule, "e2e-failed");

        let runs = job_runs(
            &harness,
            &[
                check("playwright (chromium)", Some(CheckConclusion::Success)),
                check("playwright (firefox)", None),
            ],
            &changed,
            &[],
        );
        assert_eq!(runs[0].state, State::Pending);
    }

    #[test]
    fn a_similarly_named_check_does_not_claim_the_job() {
        assert!(!matches_check(
            &check("playwright-lint", Some(CheckConclusion::Success)),
            "playwright"
        ));
        assert!(matches_check(
            &check("ci / playwright", Some(CheckConclusion::Success)),
            "playwright"
        ));
    }

    #[test]
    fn a_label_gate_is_honoured_against_the_pull_request_labels() {
        let text = "name: e2e\non: pull_request\njobs:\n  run:\n    if: contains(github.event.pull_request.labels.*.name, 'run-e2e')\n    steps:\n      - run: make e2e\n";
        let harness = Harness {
            tests: vec![],
            workflows: vec![classify_workflow(".github/workflows/e2e.yml", text, &[]).unwrap()],
            truncated: false,
        };
        assert!(matches!(
            harness.workflows[0].trigger,
            Trigger::PullRequest { .. }
        ));
        let changed = strings(&["src/main.rs"]);
        let runs = job_runs(&harness, &[], &changed, &[]);
        assert_eq!(
            runs[0].state,
            State::NotTriggered(Reason::LabelMissing("run-e2e".into()))
        );
        assert_eq!(findings(&runs)[0].severity, Severity::Medium);

        let runs = job_runs(&harness, &[], &changed, &strings(&["run-e2e"]));
        assert_eq!(runs[0].state, State::Pending);
    }

    #[test]
    fn settle_waits_for_every_watched_job_then_concludes() {
        let watch = Watch {
            head_sha: "abc1234567".into(),
            jobs: strings(&["playwright", "cypress"]),
            summary: "Coverage looks complete.".into(),
            failed: false,
            generation: String::new(),
        };
        assert_eq!(
            settle(
                &watch,
                &[check("playwright", Some(CheckConclusion::Success))],
                Severity::High
            ),
            None,
            "cypress has not reported"
        );
        assert_eq!(
            settle(
                &watch,
                &[
                    check("playwright", Some(CheckConclusion::Success)),
                    check("cypress", None)
                ],
                Severity::High
            ),
            None,
            "cypress is still running"
        );
        let settled = settle(
            &watch,
            &[
                check("playwright", Some(CheckConclusion::Success)),
                check("cypress", Some(CheckConclusion::Success)),
            ],
            Severity::High,
        )
        .expect("settled");
        assert_eq!(settled.conclusion, CheckConclusion::Success);
        assert!(settled.summary.starts_with("Coverage looks complete."));
        assert!(
            settled.summary.contains("`cypress`: passed"),
            "{}",
            settled.summary
        );
    }

    #[test]
    fn a_failed_job_fails_the_settled_lane_under_fail_on_high_but_not_critical() {
        let watch = Watch {
            head_sha: "abc".into(),
            jobs: strings(&["playwright"]),
            summary: String::new(),
            failed: false,
            generation: String::new(),
        };
        let checks = [check("playwright", Some(CheckConclusion::Failure))];
        assert_eq!(
            settle(&watch, &checks, Severity::High).unwrap().conclusion,
            CheckConclusion::Failure
        );
        assert_eq!(
            settle(&watch, &checks, Severity::Critical)
                .unwrap()
                .conclusion,
            CheckConclusion::Success
        );
    }

    #[test]
    fn a_static_half_that_already_failed_stays_failed() {
        let watch = Watch {
            head_sha: "abc".into(),
            jobs: strings(&["playwright"]),
            summary: String::new(),
            failed: true,
            generation: String::new(),
        };
        let settled = settle(
            &watch,
            &[check("playwright", Some(CheckConclusion::Success))],
            Severity::High,
        )
        .unwrap();
        assert_eq!(settled.conclusion, CheckConclusion::Failure);
    }
}
