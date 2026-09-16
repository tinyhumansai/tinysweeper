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
}

/// Review `units` concurrently, at most [`MAX_CONCURRENT_FILES`] at a time.
///
/// `T` is one changed file for `security`, and one [`crate::lanes::grouping::FileGroup`]
/// for `critique` — either way, one item is one conversation and `label`
/// names what goes in the failure list and the summary. Failures are
/// collected rather than propagated: see the module doc.
pub async fn per_unit<T, F, Fut>(units: &[T], label: impl Fn(&T) -> String, review: F) -> FanOut
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
        let task = review(unit);
        async move {
            let _permit = permits
                .acquire_owned()
                .await
                .expect("the permit pool is never closed");
            (name, task.await)
        }
    });

    let mut out = FanOut::default();
    for (name, result) in futures::future::join_all(tasks).await {
        match result {
            Ok(review) => out.reviews.push(review),
            Err(err) => out.failures.push((name, err)),
        }
    }
    out
}

/// [`per_unit`] over plain paths, where the label is the path itself.
///
/// The common case — `security`, and `critique` before grouping — kept as its
/// own name so a call site reads as "one file, one conversation" rather than
/// spelling out an identity label closure every time.
pub async fn per_file<F, Fut>(paths: &[String], review: F) -> FanOut
where
    F: Fn(String) -> Fut,
    Fut: Future<Output = crate::error::Result<FileReview>>,
{
    per_unit(paths, String::clone, review).await
}

/// The results of a fan-out, successes and failures kept apart.
#[derive(Debug, Default)]
pub struct FanOut {
    /// One entry per file that was reviewed.
    pub reviews: Vec<FileReview>,
    /// The files whose review failed, with why.
    pub failures: Vec<(String, Error)>,
}

impl FanOut {
    /// Fold the per-file results into one lane outcome.
    ///
    /// The summary reports the failures explicitly. A partial review that reads
    /// like a complete one is worse than no review at all, because a human
    /// stops looking.
    pub fn into_outcome(self) -> LaneOutcome {
        let reviewed = self.reviews.len();
        let mut findings = Vec::new();
        let mut resolved = Vec::new();
        let mut spend = Spend::default();
        let mut only_summary = None;

        for review in self.reviews {
            spend.merge(review.spend);
            findings.extend(review.findings);
            resolved.extend(review.resolved);
            only_summary = Some(review.summary);
        }

        // One file is the common case for a small pull request, and its own
        // sentence says more than a count would.
        let mut summary = match (reviewed, only_summary) {
            (1, Some(single)) if !single.trim().is_empty() => single.trim().to_string(),
            _ => format!(
                "Reviewed {reviewed} file{}; {} finding{}.",
                plural(reviewed),
                findings.len(),
                plural(findings.len())
            ),
        };

        if !self.failures.is_empty() {
            let names: Vec<&str> = self
                .failures
                .iter()
                .map(|(path, _)| path.as_str())
                .collect();
            summary.push_str(&format!(
                " {} file{} could not be reviewed: {}.",
                self.failures.len(),
                plural(self.failures.len()),
                names.join(", ")
            ));
        }

        let skipped = (reviewed == 0 && !self.failures.is_empty())
            .then(|| "No files could be reviewed; see the listed provider failures.".to_string());
        // Every file that got no answer, whether or not others did: a lane that
        // reviewed two files of three cannot vouch for the third.
        let unanswered = self.failures.iter().map(|(path, _)| path.clone()).collect();
        LaneOutcome {
            summary,
            findings,
            resolved,
            spend,
            skipped,
            unanswered,
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
