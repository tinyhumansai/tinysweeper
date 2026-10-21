//! Fan-out: one conversation per unit of review, bounded and isolated.
//!
//! A lane that reviews a forty-file pull request in one conversation reviews
//! the first few files carefully and the rest as an afterthought. Splitting it
//! fixes that, and buys two more things:
//!
//! - **Isolation of failure.** One unit's model call failing must not fail the
//!   lane. The rest are still reviewed and the failure is counted into the
//!   summary, where a human can see it — a lane that returns nothing because
//!   one call timed out is a lane that quietly reports "all clear".
//! - **Isolation of subject.** Each conversation is told exactly which files it
//!   owns (see `isolation_clause` in `harness::prompt`). Without that, every
//!   one of the N reviewers notices the same cross-file problem and reports it.
//!
//! The unit is one file for `security`; for `critique` it is one
//! [`crate::lanes::grouping::FileGroup`] — a handful of related changed files
//! reviewed together so a bug spanning them is visible to one reviewer instead
//! of hidden between two isolated ones. [`per_unit`] is generic over which;
//! [`per_file`] is the plain-path convenience both lanes used before grouping
//! existed, kept because `security` and every test predating grouping still
//! read most naturally as "one file, one conversation".
//!
//! The concurrency cap is the third thing. Each call is a model call, and an
//! unbounded fan-out over a large pull request is an unbounded bill and a rate
//! limit at the same time.

use std::future::Future;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::Semaphore;

use crate::error::Error;
use crate::findings::types::Finding;
use crate::lanes::LaneOutcome;
use crate::ports::model::Spend;

/// How many files a lane reviews at once.
///
/// Matches open-code-review's cap. The number is about spend and provider rate
/// limits, not CPU: these tasks are almost entirely waiting on a model.
pub const MAX_CONCURRENT_FILES: usize = 8;

/// What reviewing one file produced.
#[derive(Debug, Clone, Default)]
pub struct FileReview {
    /// The file's own one-line verdict.
    pub summary: String,
    /// Its findings, already anchored and filtered.
    pub findings: Vec<Finding>,
    /// Earlier findings this file's revision fixed.
    pub resolved: Vec<String>,
    /// What the call cost, and which model answered.
    pub spend: Spend,
    /// A required host-controlled stage stopped after this review was produced.
    /// The findings and spend remain useful, but member paths are not complete.
    pub execution_refusal: Option<crate::ports::model_execution::ModelExecutionRefusal>,
}

/// Why a prepared group did not receive a model review.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OmissionReason {
    /// The global Large-review plan left this group outside its selected set.
    NotSelected,
    /// The original absolute soft deadline stopped new group work.
    SoftDeadline,
    /// The bounded drain ended before this in-flight group returned.
    DrainDeadline,
    /// The shared physical-call ceiling refused a call in this group.
    CallLimit,
    /// The review owner cancelled the active execution scope.
    Cancelled,
}

impl OmissionReason {
    pub(crate) fn description(self) -> &'static str {
        match self {
            Self::NotSelected => "not selected by the large-review budget",
            Self::SoftDeadline => "not started before the soft deadline",
            Self::DrainDeadline => "did not finish before the drain deadline",
            Self::CallLimit => "stopped at the physical-call limit",
            Self::Cancelled => "cancelled with the owning review",
        }
    }
}

/// A planned group omitted without claiming that a provider failed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OmittedGroup {
    /// Every file whose group received no complete review answer.
    pub paths: Vec<String>,
    /// Host-side reason the group was omitted.
    pub reason: OmissionReason,
}

/// Review `units` concurrently, at most [`MAX_CONCURRENT_FILES`] at a time.
///
/// `T` is one changed file for `security`, and one [`crate::lanes::grouping::FileGroup`]
/// for `critique` — either way, one item is one conversation and `label`
/// names what goes in the failure list and the summary. `member_paths` gives
/// the individual file paths one unit stands for, so a multi-file group's
/// success or failure is still counted and named file-by-file downstream
/// rather than collapsed into one synthetic entry per conversation. Failures
/// are collected rather than propagated: see the module doc.
pub async fn per_unit<T, F, Fut>(
    units: &[T],
    label: impl Fn(&T) -> String,
    member_paths: impl Fn(&T) -> Vec<String>,
    review: F,
) -> FanOut
where
    T: Clone + Send + 'static,
    F: Fn(T) -> Fut,
    Fut: Future<Output = crate::error::Result<FileReview>>,
{
    let permits = Arc::new(Semaphore::new(MAX_CONCURRENT_FILES));

    // The future is built eagerly but polled only after a permit is held, so
    // the cap bounds work in flight rather than merely futures allocated.
    let tasks = units.iter().cloned().map(|unit| {
        let permits = permits.clone();
        let name = label(&unit);
        let paths = member_paths(&unit);
        let task = review(unit);
        async move {
            let _permit = permits
                .acquire_owned()
                .await
                .expect("the permit pool is never closed");
            (name, paths, task.await)
        }
    });

    let mut out = FanOut::default();
    for (name, paths, result) in futures::future::join_all(tasks).await {
        match result {
            Ok(review) => out.reviews.push((paths, review)),
            Err(err) => out.failures.push((name, paths, err)),
        }
    }
    out
}

/// Review units in input order with the run's absolute stop window.
///
/// Work is started lazily, with the same concurrency bound as [`per_unit`].
/// Completed results are folded in input order so scheduling completion order
/// cannot change finding deduplication or summaries. The legacy [`per_unit`]
/// API remains unbounded in time for callers without a review execution.
pub(crate) async fn per_unit_bounded<T, F, Fut>(
    units: &[T],
    label: impl Fn(&T) -> String,
    member_paths: impl Fn(&T) -> Vec<String>,
    review: F,
    execution: Arc<crate::app::review_execution::ReviewExecution>,
) -> FanOut
where
    T: Clone,
    F: Fn(T) -> Fut,
    Fut: Future<Output = crate::error::Result<FileReview>> + Send,
{
    use futures::stream::{FuturesUnordered, StreamExt};
    use std::collections::BTreeMap;

    let soft_deadline = execution.soft_deadline();
    let hard_deadline = execution.hard_deadline();
    let now = tokio::time::Instant::now();
    let requested_drain = soft_deadline + std::time::Duration::from_secs(execution.drain_seconds());
    let hard_safety = hard_deadline
        .checked_duration_since(now)
        .filter(|remaining| *remaining > std::time::Duration::from_secs(5))
        .map(|_| hard_deadline - std::time::Duration::from_secs(5))
        .unwrap_or(now);
    let drain_deadline = requested_drain.min(hard_safety);

    let mut results = (0..units.len())
        .map(|_| None)
        .collect::<Vec<Option<crate::error::Result<FileReview>>>>();
    let mut omitted = vec![None; units.len()];
    let mut active_paths = BTreeMap::<usize, Vec<String>>::new();
    let mut active: FuturesUnordered<
        futures::future::BoxFuture<'_, (usize, crate::error::Result<FileReview>)>,
    > = FuturesUnordered::new();
    let mut next = 0usize;
    let mut draining = false;

    loop {
        if execution.is_cancelled() {
            for reason in omitted.iter_mut().skip(next) {
                *reason = Some(OmissionReason::Cancelled);
            }
            for index in active_paths.keys() {
                omitted[*index] = Some(OmissionReason::Cancelled);
            }
            break;
        }

        let now = tokio::time::Instant::now();
        if !draining && now >= soft_deadline {
            draining = true;
            for reason in omitted.iter_mut().skip(next) {
                *reason = Some(OmissionReason::SoftDeadline);
            }
        }
        if draining && now >= drain_deadline {
            for index in active_paths.keys() {
                omitted[*index] = Some(OmissionReason::DrainDeadline);
            }
            break;
        }

        while !draining && active.len() < MAX_CONCURRENT_FILES && next < units.len() {
            let index = next;
            next += 1;
            active_paths.insert(index, member_paths(&units[index]));
            let future = review(units[index].clone());
            active.push(Box::pin(async move { (index, future.await) }));
        }

        if active.is_empty() {
            if next >= units.len() || draining {
                break;
            }
            continue;
        }

        let deadline = if draining {
            drain_deadline
        } else {
            soft_deadline
        };
        tokio::select! {
            completed = active.next() => {
                if let Some((index, result)) = completed {
                    active_paths.remove(&index);
                    results[index] = Some(result);
                }
            }
            _ = tokio::time::sleep_until(deadline) => {}
            _ = execution.cancelled() => {}
        }
    }
    drop(active);

    let mut out = FanOut::default();
    for (index, unit) in units.iter().enumerate() {
        let paths = member_paths(unit);
        match results[index].take() {
            Some(Ok(review)) if let Some(refusal) = review.execution_refusal => {
                out.partial.push((paths.clone(), review));
                out.omitted.push(OmittedGroup {
                    paths,
                    reason: omission_reason(refusal),
                });
            }
            Some(Ok(review)) => out.reviews.push((paths, review)),
            Some(Err(crate::error::Error::ModelExecutionRefused { refusal, .. })) => {
                out.omitted.push(OmittedGroup {
                    paths,
                    reason: omission_reason(refusal),
                });
            }
            Some(Err(error)) => out.failures.push((label(unit), paths, error)),
            None => out.omitted.push(OmittedGroup {
                paths,
                reason: omitted[index].unwrap_or(OmissionReason::Cancelled),
            }),
        }
    }
    out
}

fn omission_reason(
    refusal: crate::ports::model_execution::ModelExecutionRefusal,
) -> OmissionReason {
    match refusal {
        crate::ports::model_execution::ModelExecutionRefusal::CallLimit => {
            OmissionReason::CallLimit
        }
        crate::ports::model_execution::ModelExecutionRefusal::SoftDeadline => {
            OmissionReason::SoftDeadline
        }
        crate::ports::model_execution::ModelExecutionRefusal::DrainDeadline => {
            OmissionReason::DrainDeadline
        }
        crate::ports::model_execution::ModelExecutionRefusal::Cancelled => {
            OmissionReason::Cancelled
        }
    }
}

/// [`per_unit`] over plain paths, where the label and the sole member path
/// are both the path itself.
///
/// The common case — `security`, and `critique` before grouping — kept as its
/// own name so a call site reads as "one file, one conversation" rather than
/// spelling out an identity label closure every time.
pub async fn per_file<F, Fut>(paths: &[String], review: F) -> FanOut
where
    F: Fn(String) -> Fut,
    Fut: Future<Output = crate::error::Result<FileReview>>,
{
    per_unit(paths, String::clone, |path| vec![path.clone()], review).await
}

/// The results of a fan-out, successes and failures kept apart.
///
/// Each entry carries the individual file paths its unit stood for, so a
/// multi-file [`crate::lanes::grouping::FileGroup`] contributes one entry per
/// member file to the counts and lists `into_outcome` builds, not one per
/// conversation.
#[derive(Debug, Default)]
pub struct FanOut {
    /// One entry per unit that was reviewed: its member file paths, and the
    /// review produced for the whole unit.
    pub reviews: Vec<(Vec<String>, FileReview)>,
    /// Results with findings/spend retained, but an essential review stage
    /// refused by host policy. These paths are omitted from completed coverage.
    pub partial: Vec<(Vec<String>, FileReview)>,
    /// The units whose review failed: the unit's label, its member file
    /// paths, and why.
    pub failures: Vec<(String, Vec<String>, Error)>,
    /// Groups the host did not review, kept distinct from provider failures.
    pub omitted: Vec<OmittedGroup>,
}

impl FanOut {
    /// Fold the per-file results into one lane outcome.
    ///
    /// The summary reports the failures explicitly. A partial review that reads
    /// like a complete one is worse than no review at all, because a human
    /// stops looking.
    pub fn into_outcome(self) -> LaneOutcome {
        // A multi-file group is one conversation but several files: counted
        // and named per member path here, so "Reviewed N files" and the
        // unanswered list stay file-accurate rather than reporting one
        // synthetic entry per conversation.
        let completed = !self.reviews.is_empty();
        let reviewed: usize = self.reviews.iter().map(|(paths, _)| paths.len()).sum();
        let one_conversation = self.reviews.len() == 1;
        let mut reviewed_paths = Vec::with_capacity(reviewed);
        let mut findings = Vec::new();
        let mut resolved = Vec::new();
        let mut spend = Spend::default();
        let mut only_summary = None;

        for (paths, review) in self.reviews {
            reviewed_paths.extend(paths);
            spend.merge(review.spend);
            findings.extend(review.findings);
            resolved.extend(review.resolved);
            // A lone unit's own sentence stands in for the count whether it
            // covered one file or a whole group: it is the one verdict this
            // fan-out reached, and reads better than a bare tally.
            only_summary = Some(review.summary);
        }

        for (_, review) in self.partial {
            spend.merge(review.spend);
            findings.extend(review.findings);
            // A partial group stopped before its required review stages
            // completed, so its resolutions are not safe continuity evidence.
        }

        // One conversation is the common case for a small pull request or a
        // single group, and its own sentence says more than a count would.
        let mut summary = match (one_conversation, only_summary) {
            (true, Some(single)) if !single.trim().is_empty() => single.trim().to_string(),
            _ => format!(
                "Reviewed {reviewed} file{}; {} finding{}.",
                plural(reviewed),
                findings.len(),
                plural(findings.len())
            ),
        };

        // Every path behind a failed unit, not one synthetic entry per
        // conversation: a failed two-file group must count and name both.
        let failed_paths: Vec<String> = self
            .failures
            .iter()
            .flat_map(|(_, paths, _)| paths.iter().cloned())
            .collect();
        let mut unanswered = failed_paths.clone();
        unanswered.extend(
            self.omitted
                .iter()
                .flat_map(|omitted| omitted.paths.iter().cloned()),
        );

        if !unanswered.is_empty() {
            let total = reviewed + unanswered.len();
            summary = format!(
                "Reviewed {reviewed} of {total} files — {} not reviewed. {summary}",
                unanswered.len()
            );
        }

        if !self.failures.is_empty() {
            summary.push_str(&format!(
                " {} file{} could not be reviewed: {}.",
                failed_paths.len(),
                plural(failed_paths.len()),
                failed_paths.join(", ")
            ));
        }

        if !self.omitted.is_empty() {
            let omitted_count: usize = self.omitted.iter().map(|group| group.paths.len()).sum();
            let reasons = self
                .omitted
                .iter()
                .map(|group| group.reason.description())
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .collect::<Vec<_>>()
                .join(", ");
            summary.push_str(&format!(
                " {omitted_count} file{} were omitted: {reasons}.",
                plural(omitted_count)
            ));
        }

        let skipped = (reviewed == 0
            && findings.is_empty()
            && (!self.failures.is_empty() || !self.omitted.is_empty()))
        .then(|| {
            "No files received a complete review; see the listed failures and omissions."
                .to_string()
        });
        let coverage = (!reviewed_paths.is_empty() || !unanswered.is_empty()).then_some(
            crate::lanes::FileCoverage {
                reviewed: reviewed_paths,
                unanswered: unanswered.clone(),
            },
        );
        LaneOutcome {
            summary,
            context_degraded: false,
            completed,
            findings,
            resolved,
            spend,
            skipped,
            pending: Vec::new(),
            unanswered,
            coverage,
            omissions: self.omitted,
        }
    }
}

fn plural(count: usize) -> &'static str {
    if count == 1 { "" } else { "s" }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn review_of(summary: &str) -> FileReview {
        FileReview {
            summary: summary.to_string(),
            ..FileReview::default()
        }
    }

    #[tokio::test]
    async fn every_path_is_reviewed() {
        let paths = vec!["a.rs".to_string(), "b.rs".into(), "c.rs".into()];
        let out = per_file(&paths, |path| async move { Ok(review_of(&path)) }).await;

        assert_eq!(out.reviews.len(), 3);
        assert!(out.failures.is_empty());
    }

    #[tokio::test]
    async fn one_files_failure_does_not_fail_the_lane() {
        let paths = vec!["good.rs".to_string(), "bad.rs".into()];
        let out = per_file(&paths, |path| async move {
            if path == "bad.rs" {
                Err(Error::Model("upstream exploded".into()))
            } else {
                Ok(review_of(&path))
            }
        })
        .await;

        assert_eq!(out.reviews.len(), 1);
        assert_eq!(out.failures.len(), 1);

        let outcome = out.into_outcome();
        assert!(outcome.skipped.is_none());
        assert!(
            outcome.summary.contains("bad.rs"),
            "a partial review must say so: {}",
            outcome.summary
        );
        assert_eq!(
            outcome.unanswered,
            vec!["bad.rs".to_string()],
            "the file nobody answered for is named, so the verdict cannot vouch for it"
        );
    }

    #[tokio::test]
    async fn every_failure_makes_the_lane_neutral() {
        let paths = vec!["bad.rs".to_string()];
        let outcome = per_file(&paths, |_| async { Err(Error::Model("nope".into())) })
            .await
            .into_outcome();

        assert!(outcome.skipped.is_some());
        assert_eq!(outcome.unanswered, vec!["bad.rs".to_string()]);
    }

    #[test]
    fn omitted_multifile_groups_keep_every_path_and_host_reason() {
        let outcome = FanOut {
            omitted: vec![OmittedGroup {
                paths: vec!["src/a.rs".into(), "src/a_test.rs".into()],
                reason: OmissionReason::SoftDeadline,
            }],
            ..FanOut::default()
        }
        .into_outcome();

        assert!(!outcome.completed);
        assert!(outcome.skipped.is_some());
        assert_eq!(
            outcome.unanswered,
            ["src/a.rs".to_string(), "src/a_test.rs".to_string()]
        );
        assert_eq!(
            outcome
                .coverage
                .expect("coverage records omissions")
                .unanswered,
            ["src/a.rs".to_string(), "src/a_test.rs".to_string()]
        );
        assert_eq!(outcome.omissions[0].reason, OmissionReason::SoftDeadline);
        assert!(
            outcome
                .summary
                .contains("not started before the soft deadline")
        );
    }

    #[test]
    fn partial_group_does_not_resolve_threads_for_unreviewed_members() {
        let paths = vec!["src/a.rs".to_string(), "src/b.rs".to_string()];
        let outcome = FanOut {
            reviews: vec![(
                vec!["src/complete.rs".to_string()],
                FileReview {
                    resolved: vec!["Completed sibling finding".to_string()],
                    ..FileReview::default()
                },
            )],
            partial: vec![(
                paths.clone(),
                FileReview {
                    resolved: vec!["Old finding title".to_string()],
                    spend: Spend {
                        usage: crate::ports::model::Usage {
                            input_tokens: 7,
                            output_tokens: 2,
                            cost_usd: 0.125,
                            ..crate::ports::model::Usage::default()
                        },
                        models: vec!["reviewer-model".to_string()],
                    },
                    ..FileReview::default()
                },
            )],
            omitted: vec![OmittedGroup {
                paths: paths.clone(),
                reason: OmissionReason::CallLimit,
            }],
            ..FanOut::default()
        }
        .into_outcome();

        let resolved = std::collections::BTreeSet::from_iter(outcome.resolved.iter().cloned());
        let outdated_thread = crate::forge::types::ReviewThread {
            id: "PRRT_old-finding".to_string(),
            is_outdated: true,
            comments: vec![crate::forge::types::ThreadComment {
                author: "tinysweeper[bot]".to_string(),
                body: "**Old finding title**\n\n<!-- tinysweeper:fp=old -->".to_string(),
                bot: true,
                ..crate::forge::types::ThreadComment::default()
            }],
            ..crate::forge::types::ReviewThread::default()
        };
        let explicitly_resolved =
            std::collections::BTreeSet::from(["Old finding title".to_string()]);
        assert!(matches!(
            crate::threads::decide(&outdated_thread, &explicitly_resolved),
            crate::threads::Decision::Resolve(_)
        ));
        assert!(matches!(
            crate::threads::decide(&outdated_thread, &resolved),
            crate::threads::Decision::Leave(_)
        ));
        assert_eq!(outcome.resolved, ["Completed sibling finding"]);
        assert!(outcome.completed, "the completed sibling remains completed");
        assert_eq!(outcome.unanswered, paths);
        assert_eq!(outcome.spend.usage.input_tokens, 7);
        assert_eq!(outcome.spend.usage.output_tokens, 2);
        assert_eq!(outcome.spend.usage.cost_usd, 0.125);
        assert_eq!(outcome.spend.models, ["reviewer-model"]);
    }

    #[tokio::test]
    async fn a_multi_file_groups_success_counts_every_member_file() {
        // Two two-file groups: the fan-out ran two conversations, but the
        // outcome must report four files reviewed, not two conversations.
        let groups = vec![
            vec!["a.rs".to_string(), "b.rs".to_string()],
            vec!["c.rs".to_string(), "d.rs".to_string()],
        ];
        let outcome = per_unit(
            &groups,
            |g| g.join(" + "),
            |g| g.clone(),
            |g| async move { Ok(review_of(&format!("Reviewed {}.", g.join(" + ")))) },
        )
        .await
        .into_outcome();

        assert!(
            outcome.summary.starts_with("Reviewed 4 files"),
            "{}",
            outcome.summary
        );
    }

    #[tokio::test]
    async fn a_failed_multi_file_group_names_every_member_file_as_unanswered() {
        let groups = vec![vec!["a.rs".to_string(), "b.rs".to_string()]];
        let outcome = per_unit(
            &groups,
            |g| g.join(" + "),
            |g| g.clone(),
            |_| async { Err(Error::Model("upstream exploded".into())) },
        )
        .await
        .into_outcome();

        assert_eq!(
            outcome.unanswered,
            vec!["a.rs".to_string(), "b.rs".to_string()],
            "a failed two-file group must name both files, not one synthetic label"
        );
        assert!(
            outcome
                .summary
                .contains("2 files could not be reviewed: a.rs, b.rs"),
            "{}",
            outcome.summary
        );
    }

    #[tokio::test]
    async fn concurrency_is_bounded() {
        // A lane fanning out over a large pull request with no cap is an
        // unbounded bill and a provider rate limit at the same time.
        let live = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let paths: Vec<String> = (0..MAX_CONCURRENT_FILES * 3)
            .map(|i| format!("f{i}.rs"))
            .collect();

        let out = per_file(&paths, |path| {
            let live = live.clone();
            let peak = peak.clone();
            async move {
                let now = live.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(now, Ordering::SeqCst);
                tokio::task::yield_now().await;
                live.fetch_sub(1, Ordering::SeqCst);
                Ok(review_of(&path))
            }
        })
        .await;

        assert_eq!(out.reviews.len(), paths.len());
        assert!(
            peak.load(Ordering::SeqCst) <= MAX_CONCURRENT_FILES,
            "peaked at {}",
            peak.load(Ordering::SeqCst)
        );
    }

    #[tokio::test]
    async fn bounded_fanout_folds_out_of_order_completions_in_input_order() {
        use tokio::sync::{mpsc, watch};

        let started_at = tokio::time::Instant::now();
        let execution = Arc::new(crate::app::review_execution::ReviewExecution::new(
            crate::app::review_execution::ReviewExecutionOptions {
                started_at,
                hard_deadline: started_at + std::time::Duration::from_secs(100),
                mode: crate::lanes::large_pr::ReviewMode::Normal,
                max_calls: None,
                soft_deadline_percent: 80,
                drain_seconds: 5,
            },
        ));
        let units = vec![
            "first".to_string(),
            "second".to_string(),
            "third".to_string(),
        ];
        let (started_tx, mut started_rx) = mpsc::unbounded_channel();
        let (finished_tx, mut finished_rx) = mpsc::unbounded_channel();
        let (release_tx, release_rx) = watch::channel(false);
        let task = tokio::spawn({
            let execution = execution.clone();
            async move {
                per_unit_bounded(
                    &units,
                    |unit| unit.clone(),
                    |unit| vec![unit.clone()],
                    move |unit| {
                        let started_tx = started_tx.clone();
                        let finished_tx = finished_tx.clone();
                        let mut release_rx = release_rx.clone();
                        async move {
                            started_tx
                                .send(unit.clone())
                                .expect("start observer stays open");
                            if unit == "first" {
                                while !*release_rx.borrow() {
                                    if release_rx.changed().await.is_err() {
                                        break;
                                    }
                                }
                            }
                            finished_tx
                                .send(unit.clone())
                                .expect("completion observer stays open");
                            Ok(review_of(&unit))
                        }
                    },
                    execution,
                )
                .await
            }
        });

        let completion_order = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            let mut started = Vec::new();
            for _ in 0..3 {
                started.push(started_rx.recv().await.expect("all units are polled"));
            }
            let mut finished = Vec::new();
            for _ in 0..2 {
                finished.push(
                    finished_rx
                        .recv()
                        .await
                        .expect("later units complete first"),
                );
            }
            (started, finished)
        })
        .await;
        release_tx.send_replace(true);
        let result = tokio::time::timeout(std::time::Duration::from_secs(2), task)
            .await
            .expect("the released first unit completes")
            .expect("fan-out task stays alive");
        let (started, finished) = completion_order.expect("later units finish before first");
        assert_eq!(started, ["first", "second", "third"]);
        assert_eq!(finished, ["second", "third"]);
        assert_eq!(
            result
                .reviews
                .into_iter()
                .map(|(_, review)| review.summary)
                .collect::<Vec<_>>(),
            ["first", "second", "third"]
        );
    }

    #[tokio::test]
    async fn a_single_file_keeps_its_own_verdict() {
        let paths = vec!["only.rs".to_string()];
        let outcome = per_file(&paths, |_| async { Ok(review_of("Looks fine.")) })
            .await
            .into_outcome();

        assert_eq!(outcome.summary, "Looks fine.");
    }

    #[tokio::test]
    async fn several_files_are_summarised_by_count() {
        let paths = vec!["a.rs".to_string(), "b.rs".into()];
        let outcome = per_file(&paths, |p| async move { Ok(review_of(&p)) })
            .await
            .into_outcome();

        assert!(outcome.summary.starts_with("Reviewed 2 files"));
    }
}
