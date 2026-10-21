//! Per-review telemetry shared by the server and the read-only review pipeline.

use std::collections::BTreeMap;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tracing::Instrument;
use tracing::Span;

use crate::ports::model::Usage;

/// Aggregated completed-pass yields with no group identifiers or content.
#[derive(Debug, Clone, Copy, Default, serde::Serialize)]
pub(crate) struct PassYield {
    /// Groups that completed this pass.
    pub(crate) groups: u64,
    /// Candidate findings before falsification.
    pub(crate) candidates: u64,
    /// Findings remaining after falsification.
    pub(crate) survivors: u64,
}

/// Stable phase keys included in every terminal timing record.
#[cfg(any(feature = "serve", test))]
pub(crate) const PHASES: &[&str] = &[
    "permit",
    "open_status",
    "checkout",
    "context",
    "knowledge",
    "retrieval",
    "graph",
    "lanes",
    "summary",
    "threads",
    "wireframe",
    "publish",
];

/// Classify a review error for timing and operator dashboards.
#[cfg(feature = "serve")]
pub(crate) fn outcome_for_error(error: &crate::error::Error) -> &'static str {
    if matches!(error, crate::error::Error::Timeout { .. }) {
        "deadline"
    } else {
        "failure"
    }
}

#[derive(Debug, Clone)]
/// Shared, cancellation-safe telemetry for a single review worker.
pub(crate) struct ReviewTiming(Arc<Inner>);

#[derive(Debug)]
struct Inner {
    record: Mutex<Record>,
    worker_span: Span,
}

/// Snapshot of the server's structured telemetry for one review run.
#[derive(Debug, Clone, Default)]
pub(crate) struct Record {
    /// GitHub repository slug.
    pub(crate) repo: String,
    /// Pull-request number.
    pub(crate) pr: u64,
    /// Commit SHA being reviewed, when known.
    pub(crate) head_sha: String,
    /// Review mode selected by the server.
    pub(crate) mode: String,
    /// Terminal outcome, defaulting to `cancelled` until explicitly finished.
    pub(crate) outcome: String,
    /// Worker start instant used to derive total wall time.
    pub(crate) started: Option<Instant>,
    /// Time spent waiting for a review permit.
    pub(crate) queue_wait_ms: u64,
    /// Accumulated duration per top-level phase.
    pub(crate) phases_ms: BTreeMap<String, u64>,
    /// Accumulated duration per review lane.
    pub(crate) lanes_ms: BTreeMap<String, u64>,
    /// Physical model dispatches; pre-dispatch refusals are counted separately.
    pub(crate) calls_attempted: u64,
    /// Provider calls returning a response, including truncated or invalid output.
    pub(crate) calls_succeeded: u64,
    /// Model requests refused by admission or the provider.
    pub(crate) calls_refused: u64,
    /// Model attempts that failed without a refusal.
    pub(crate) calls_errored: u64,
    /// Aggregated tokens and cost retained by model adapters.
    pub(crate) usage: Usage,
    /// Authoritative accounting for a completed proposal, including retrieval
    /// work that does not pass through the model observer.
    finalized_usage: Option<FinalizedUsage>,
    /// Bounded yield totals keyed by stable lane id and pass number.
    pub(crate) pass_yields: BTreeMap<String, PassYield>,
    /// Files changed in the pull request.
    pub(crate) files_total: usize,
    /// Files without any known unanswered coverage.
    pub(crate) files_answered: usize,
    /// Files omitted or unanswered by review lanes.
    pub(crate) files_unanswered: usize,
}

#[derive(Debug, Clone, serde::Serialize)]
struct FinalizedUsage {
    usage: Usage,
    models: Vec<String>,
    lane_usage: Vec<(String, Usage, Vec<String>)>,
}

impl ReviewTiming {
    /// Start a timing record for the repository, PR, and review mode.
    #[cfg(any(feature = "serve", test))]
    pub(crate) fn new(repo: impl Into<String>, pr: u64, mode: impl Into<String>) -> Self {
        let mut record = Record {
            repo: repo.into(),
            pr,
            mode: mode.into(),
            outcome: "cancelled".into(),
            started: Some(Instant::now()),
            ..Record::default()
        };
        for phase in PHASES {
            record.phases_ms.insert((*phase).into(), 0);
        }
        Self(Arc::new(Inner {
            record: Mutex::new(record),
            worker_span: tracing::Span::current(),
        }))
    }

    /// Drop the previous proposal snapshot when a retry starts. Observer
    /// totals remain cumulative across attempts in the same worker run.
    pub(crate) fn begin_attempt(&self) {
        self.record(|record| record.finalized_usage = None);
    }

    /// Measure a top-level phase and attach correlation fields.
    pub(crate) fn phase(&self, name: &'static str) -> PhaseGuard {
        let record = self.snapshot();
        PhaseGuard {
            timing: self.clone(),
            name,
            lane: None,
            started: Instant::now(),
            span: tracing::info_span!(
                "review.phase",
                repo = %record.repo,
                pr = record.pr,
                head_sha = %record.head_sha,
                mode = %record.mode,
                phase = name,
                duration_ms = tracing::field::Empty
            ),
        }
    }

    /// Measure one lane without adding overlapping lane time to phase totals.
    pub(crate) fn lane(&self, name: &'static str) -> PhaseGuard {
        self.lane_under(name, &Span::current())
    }

    /// Measure a lane beneath the enclosing `lanes` span without entering it
    /// in thread-local state across an await.
    pub(crate) fn lane_under(&self, name: &'static str, parent: &Span) -> PhaseGuard {
        let record = self.snapshot();
        PhaseGuard {
            timing: self.clone(),
            name: "lanes",
            lane: Some(name),
            started: Instant::now(),
            span: tracing::info_span!(parent: parent,
                "review.phase",
                repo = %record.repo,
                pr = record.pr,
                head_sha = %record.head_sha,
                mode = %record.mode,
                phase = "lane",
                lane = name,
                duration_ms = tracing::field::Empty
            ),
        }
    }

    /// Apply a synchronized update to the telemetry snapshot.
    pub(crate) fn record(&self, update: impl FnOnce(&mut Record)) {
        let head_sha = {
            let mut record = self.0.record.lock().expect("review timing lock");
            update(&mut record);
            record.head_sha.clone()
        };
        if !head_sha.is_empty() {
            self.0.worker_span.record("head_sha", head_sha.as_str());
        }
    }

    /// Add one completed group-pass yield to the bounded terminal aggregate.
    fn record_pass_yield(
        &self,
        lane: crate::config::types::LaneId,
        pass: u8,
        candidates: usize,
        survivors: usize,
    ) {
        if !(1..=3).contains(&pass) {
            return;
        }
        self.record(|record| {
            let key = format!("{}:{pass}", lane.as_str());
            let yield_totals = record.pass_yields.entry(key).or_default();
            yield_totals.groups = yield_totals.groups.saturating_add(1);
            yield_totals.candidates = yield_totals
                .candidates
                .saturating_add(candidates.min(u64::MAX as usize) as u64);
            yield_totals.survivors = yield_totals
                .survivors
                .saturating_add(survivors.min(u64::MAX as usize) as u64);
        });
    }

    /// Clone the current record for assertions or intermediate reporting.
    pub(crate) fn snapshot(&self) -> Record {
        self.0.record.lock().expect("review timing lock").clone()
    }

    /// Attach finalized proposal accounting without changing physical-call
    /// counters collected by the observer.
    pub(crate) fn finalize_usage(
        &self,
        usage: Usage,
        models: &[String],
        lane_usage: &[(String, Usage, Vec<String>)],
    ) {
        let clean_models = |models: &[String]| {
            models
                .iter()
                .filter(|model| !model.trim().is_empty())
                .cloned()
                .collect()
        };
        self.record(|record| {
            record.finalized_usage = Some(FinalizedUsage {
                usage,
                models: clean_models(models),
                lane_usage: lane_usage
                    .iter()
                    .map(|(lane, usage, models)| (lane.clone(), *usage, clean_models(models)))
                    .collect(),
            });
        });
    }

    /// Mark the terminal outcome emitted when the final handle is dropped.
    #[cfg(any(feature = "serve", test))]
    pub(crate) fn finish(&self, outcome: &str) {
        self.record(|r| r.outcome = outcome.to_owned());
    }
}

/// Run a future inside a named phase, recording time on cancellation too.
pub(crate) async fn measure<F: Future>(
    timing: Option<&ReviewTiming>,
    name: &'static str,
    future: F,
) -> F::Output {
    if let Some(timing) = timing {
        let guard = timing.phase(name);
        let output = future.instrument(guard.span.clone()).await;
        drop(guard);
        output
    } else {
        future.await
    }
}

/// Run one lane as a child of the enclosing lanes span.
pub(crate) async fn measure_lane_under<F: Future>(
    timing: Option<&ReviewTiming>,
    name: &'static str,
    parent: Option<&Span>,
    future: F,
) -> F::Output {
    if let Some(timing) = timing {
        let guard = match parent {
            Some(parent) => timing.lane_under(name, parent),
            None => timing.lane(name),
        };
        let output = future.instrument(guard.span.clone()).await;
        drop(guard);
        output
    } else {
        future.await
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        let r = self.record.get_mut().expect("review timing lock");
        let (usage, usage_source, models, lane_usage) = match &r.finalized_usage {
            Some(finalized) => (
                finalized.usage,
                "proposal",
                serde_json::to_string(&finalized.models).unwrap_or_default(),
                serde_json::to_string(&finalized.lane_usage).unwrap_or_default(),
            ),
            None => (r.usage, "observed", "[]".into(), "[]".into()),
        };
        let total_ms = r
            .started
            .map(|s| s.elapsed().as_millis() as u64)
            .unwrap_or_default();
        tracing::info!(target: "tinysweeper::review_timing", repo = %r.repo, pr = r.pr,
            head_sha = %r.head_sha, mode = %r.mode, outcome = %r.outcome, total_ms,
            queue_wait_ms = r.queue_wait_ms,
            phases_ms = %serde_json::to_string(&r.phases_ms).unwrap_or_default(),
            lanes_ms = %serde_json::to_string(&r.lanes_ms).unwrap_or_default(),
            calls_attempted = r.calls_attempted, calls_succeeded = r.calls_succeeded,
            calls_refused = r.calls_refused, calls_errored = r.calls_errored,
            input_tokens = r.usage.input_tokens, output_tokens = r.usage.output_tokens,
            reasoning_tokens = r.usage.reasoning_tokens, cached_tokens = r.usage.cached_tokens,
            usage_source, cost_usd = usage.cost_usd, embed_tokens = usage.embed_tokens,
            models = %models, lane_usage = %lane_usage,
            pass_yields = %serde_json::to_string(&r.pass_yields).unwrap_or_default(),
            files_total = r.files_total, files_answered = r.files_answered,
            files_unanswered = r.files_unanswered, "review timing");
    }
}

impl crate::lanes::PassYieldObserver for ReviewTiming {
    fn record_pass_yield(
        &self,
        lane: crate::config::types::LaneId,
        pass: u8,
        candidates: usize,
        survivors: usize,
    ) {
        ReviewTiming::record_pass_yield(self, lane, pass, candidates, survivors);
    }
}

/// RAII timer that records elapsed time even when its future is cancelled.
pub(crate) struct PhaseGuard {
    timing: ReviewTiming,
    name: &'static str,
    lane: Option<&'static str>,
    started: Instant,
    /// Tracing span wrapped around the measured future.
    pub(crate) span: Span,
}
impl Drop for PhaseGuard {
    fn drop(&mut self) {
        let elapsed = self.started.elapsed().as_millis() as u64;
        self.span.record("duration_ms", elapsed);
        self.timing.record(|r| {
            if self.name == "permit" {
                r.queue_wait_ms += elapsed;
            }
            if let Some(lane) = self.lane {
                *r.lanes_ms.entry(lane.into()).or_default() += elapsed;
            } else {
                *r.phases_ms.entry(self.name.into()).or_default() += elapsed;
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pass_yield_keys_are_bounded_to_known_lanes_and_configured_passes() {
        let timing = ReviewTiming::new("owner/repo", 7, "Incremental");
        for lane in crate::config::types::LaneId::ALL {
            for pass in 1..=4 {
                timing.record_pass_yield(lane, pass, 1, 1);
            }
        }

        let record = timing.snapshot();
        assert_eq!(
            record.pass_yields.len(),
            crate::config::types::LaneId::ALL.len() * 3
        );
        assert!(!record.pass_yields.contains_key("critique:4"));
        assert_eq!(record.pass_yields["critique:1"].groups, 1);
    }

    #[test]
    fn pass_yield_totals_saturate_instead_of_overflowing() {
        let timing = ReviewTiming::new("owner/repo", 7, "Incremental");
        timing.record_pass_yield(
            crate::config::types::LaneId::Critique,
            1,
            usize::MAX,
            usize::MAX,
        );
        timing.record_pass_yield(
            crate::config::types::LaneId::Critique,
            1,
            usize::MAX,
            usize::MAX,
        );

        let totals = timing.snapshot().pass_yields["critique:1"];
        assert_eq!(totals.groups, 2);
        assert_eq!(totals.candidates, u64::MAX);
        assert_eq!(totals.survivors, u64::MAX);
    }
}
