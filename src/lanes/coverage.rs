//! Adaptive extra looks: one reviewer, told cumulatively what it already found.
//!
//! `review.passes = 3` ships as the maximum adaptive depth. Small groups still
//! cost nothing beyond round one. Above one, a lane that already placed and
//! filtered round one's findings for a group may repeatedly ask the group's
//! first council reviewer — index `0`, deterministically, never the whole
//! council again — to look at the *same* evidence, each time told plainly what
//! earlier passes reported and asked to find what they missed. The sequence
//! stops as soon as a pass contributes nothing distinct and surviving.
//!
//! This is not a second opinion. `src/falsify` already exists for "is this
//! correct", and asking a fresh reviewer "did you miss anything" is the
//! opposite direction: recall, not verification. What makes it cheap enough
//! to offer at all is that it reuses everything round one already paid for —
//! the same prompt prefix, the same evidence, the same [`runner::ask_all`]
//! entry point a whole council would use — for one call per adaptive pass,
//! gated behind a per-group line count so a two-line diff never builds the
//! second prompt.
//!
//! Anchoring the findings this pass returns is deliberately left to the
//! caller. `critique` resolves a quoted snippet through `Positioner`,
//! sometimes with a relocation call of its own; `security` anchors more
//! simply through [`crate::lanes::LaneOutcome::from_response`]. Redoing either
//! of those here would mean inventing a third anchoring rule instead of
//! reusing round one's own — so this module hands back the parsed response,
//! unplaced, and stops.

use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::config::types::LaneId;
use crate::council::Reviewer;
use crate::error::Result;
use crate::findings::types::Finding;
use crate::flows::caps::ModelCapability;
use crate::flows::panel::Call;
use crate::flows::runner::{self, Asking};
use crate::harness::prompt::Prompt;
use crate::harness::schema::LaneResponse;
use crate::lanes::reviewer_responses;
use crate::ports::model::{Spend, Usage};
use crate::ports::model_execution::ModelExecutionRefusal;

/// Why an adaptive sequence did not ask for another pass.
#[derive(Debug, Clone, Copy)]
pub enum StopReason {
    /// The configured maximum adaptive depth was reached.
    Ceiling,
    /// The reviewer returned no findings.
    Empty,
    /// The call failed or its structured response was malformed.
    Failed,
    /// Host review policy refused another pass after work was already kept.
    Refused,
    /// The lane could not place the pass's findings against the diff.
    PlacementFailure,
    /// Every proposed finding repeated one already confirmed.
    Duplicate,
    /// Every distinct proposal was removed by the lane's noise filter.
    NonSurviving,
}

impl StopReason {
    fn as_str(self) -> &'static str {
        match self {
            Self::Ceiling => "ceiling",
            Self::Empty => "empty",
            Self::Failed => "failed",
            Self::Refused => "refused",
            Self::PlacementFailure => "placement_failure",
            Self::Duplicate => "duplicate",
            Self::NonSurviving => "non_surviving",
        }
    }
}

/// Per-group adaptive-pass telemetry, emitted once after the sequence stops.
pub struct Metrics {
    started: Instant,
    attempted: usize,
    candidates_per_pass: Vec<usize>,
    survivors_per_pass: Vec<usize>,
    usage: Usage,
    elapsed: Duration,
    stop_reason: StopReason,
}

impl Metrics {
    /// Start accounting with round one's already-completed review.
    pub(crate) fn start(
        usage: Usage,
        elapsed: Duration,
        candidates: usize,
        survivors: usize,
    ) -> Self {
        Self {
            started: Instant::now(),
            attempted: 1,
            candidates_per_pass: vec![candidates],
            survivors_per_pass: vec![survivors],
            usage,
            elapsed,
            stop_reason: StopReason::Ceiling,
        }
    }

    /// Record one attempted extra pass and what survived it.
    pub fn record(&mut self, usage: Usage, elapsed: Duration, candidates: usize, survivors: usize) {
        self.attempted += 1;
        self.candidates_per_pass.push(candidates);
        self.survivors_per_pass.push(survivors);
        self.usage.add(usage);
        self.elapsed += elapsed;
    }

    /// Record why the adaptive sequence stopped early.
    pub fn stop(&mut self, reason: StopReason) {
        self.stop_reason = reason;
    }

    /// Emit one structured event for this qualifying group.
    pub fn emit(self, lane: LaneId, paths: &[String]) {
        tracing::info!(
            lane = lane.as_str(),
            group = %paths.join(" + "),
            passes_attempted = self.attempted,
            stop_reason = self.stop_reason.as_str(),
            candidate_findings_per_pass = ?self.candidates_per_pass,
            surviving_findings_per_pass = ?self.survivors_per_pass,
            input_tokens = self.usage.input_tokens,
            output_tokens = self.usage.output_tokens,
            cached_tokens = self.usage.cached_tokens,
            cost_usd = self.usage.cost_usd,
            model_elapsed_ms = self.elapsed.as_millis() as u64,
            elapsed_ms = self.started.elapsed().as_millis() as u64,
            "adaptive review passes"
        );
    }
}

/// What the extra reviewer said, before any lane-specific anchoring runs.
#[derive(Debug, Default)]
pub struct CoverageOutcome {
    /// The reviewer's structured answer, when it answered and it parsed.
    /// `None` when the call failed outright or the answer did not parse —
    /// the caller then has nothing new to place or falsify, exactly as a
    /// solo reviewer failing on round one leaves nothing to place either.
    pub response: Option<LaneResponse>,
    /// A host policy stop after the adaptive invocation's earlier turns.
    /// Cancellation remains an error so the owning review can yield; other
    /// refusals are partial results with their scoped usage and elapsed time.
    pub execution_refusal: Option<ModelExecutionRefusal>,
    /// What the call cost. Dollars are already tallied inside `llm` — every
    /// call here goes through the same [`ModelCapability`] round one used —
    /// so this only carries the model name, for the cost line's own model
    /// list.
    pub spend: Spend,
    /// What the reviewer looked up this round, if lookups are enabled.
    /// Empty when nothing was looked up.
    pub looked_up: String,
    /// Tokens and cost attributable to this call.
    pub usage: Usage,
    /// Wall-clock time spent awaiting this call.
    pub elapsed: Duration,
}

/// Ask `reviewer` once more over the evidence `prompt` already carries.
///
/// `prompt` must already have the "what you already found" suffix baked in
/// — see [`crate::harness::prompt::PromptInputs::confirmed_this_round`] — and
/// `schema`/`schema_name` must be round one's own, so the reviewer answers
/// the identical shape. Goes through [`runner::ask_all`] with a call list of
/// one rather than a new call path, which is what lets the pull request's
/// budget and lookup policy apply to this call exactly as they do to every
/// other one in the lane.
pub async fn coverage_pass(
    llm: Arc<ModelCapability>,
    lane: LaneId,
    reviewer: &Reviewer<'_>,
    prompt: &Prompt,
    schema: &Value,
    schema_name: &str,
    asking: Asking<'_>,
) -> Result<CoverageOutcome> {
    let call = Call {
        id: reviewer.id.to_string(),
        model: reviewer.model.to_string(),
        system: prompt.prefix().to_string(),
        prompt: prompt.suffix().to_string(),
        schema_name: schema_name.to_string(),
    };

    let asked = runner::ask_all_accounted(
        llm.clone(),
        lane,
        std::slice::from_ref(&call),
        schema,
        asking,
    )
    .await?;
    let usage = asked.usage;
    let elapsed = asked.elapsed;
    let answers = asked.answers;
    let Some(answer) = answers.into_iter().next() else {
        return Ok(CoverageOutcome {
            usage,
            elapsed,
            ..CoverageOutcome::default()
        });
    };

    let mut spend = Spend::default();
    spend.note(&answer.model);
    let looked_up = answer.looked_up.clone();

    // `reviewer_responses` treats a malformed response from a solo reviewer
    // (a slice of one, exactly what this call always passes) as fatal — the
    // right call for round one, where a lane with no usable answer has
    // nothing to report at all. This call is different: it is one optional
    // extra look on top of round one's already-successful findings, so a
    // schema-invalid answer here must be swallowed the same way a failed
    // call already is above, not propagated to fail the whole group and
    // discard what round one found. `CoverageOutcome::response`'s own
    // documentation promises exactly this — `None`, not an error.
    let mut execution_refusal = None;
    let response = match reviewer_responses(
        lane,
        std::slice::from_ref(reviewer),
        std::slice::from_ref(&answer),
    ) {
        Ok(mut responses) => responses.pop().map(|r| r.response),
        Err(
            err @ crate::error::Error::ModelExecutionRefused {
                refusal: ModelExecutionRefusal::Cancelled,
                ..
            },
        ) => return Err(err),
        Err(crate::error::Error::ModelExecutionRefused { refusal, .. }) => {
            execution_refusal = Some(refusal);
            None
        }
        Err(err) => {
            tracing::warn!(agent = reviewer.id, %err, "the coverage pass reviewer's answer did not parse");
            None
        }
    };

    Ok(CoverageOutcome {
        response,
        execution_refusal,
        spend,
        looked_up,
        usage,
        elapsed,
    })
}

/// Render all earlier surviving findings as the "already found" list this
/// pass's prompt shows: title first, so a reviewer skimming the block can tell
/// at a glance what not to repeat.
///
/// One line per finding, deliberately terse — this is a reminder, not the
/// finding restated in full. `line` reads `?` for a finding an earlier pass
/// could not anchor, which is still worth naming so the next pass does not
/// rediscover it and get credit for a "new" finding that is the same one.
pub fn confirmed_lines(findings: &[Finding]) -> Vec<String> {
    findings
        .iter()
        .map(|finding| {
            let line = finding
                .line
                .map(|line| line.to_string())
                .unwrap_or_else(|| "?".to_string());
            let body = finding.body.lines().next().unwrap_or("").trim();
            format!("{} ({}:{line}): {body}", finding.title.trim(), finding.path)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::types::Severity;

    fn finding(title: &str, path: &str, line: Option<u64>, body: &str) -> Finding {
        Finding {
            lane: LaneId::Critique,
            severity: Severity::High,
            confidence: 0.9,
            path: path.to_string(),
            line,
            end_line: None,
            rule: "rule".into(),
            title: title.to_string(),
            body: body.to_string(),
            suggestion: None,
            applicable: None,
            late: false,
            identity: None,
            aliases: vec![],
            grouped: false,
            review_pass: 1,
            corroboration: 1,
        }
    }

    #[test]
    fn a_confirmed_line_names_the_title_path_line_and_first_body_line() {
        let lines = confirmed_lines(&[finding(
            "Guard the index",
            "src/main.rs",
            Some(2),
            "`i` is never bounds-checked.\nMore detail below.",
        )]);

        assert_eq!(
            lines,
            vec!["Guard the index (src/main.rs:2): `i` is never bounds-checked.".to_string()]
        );
    }

    #[test]
    fn an_unanchored_finding_renders_a_question_mark_line() {
        let lines = confirmed_lines(&[finding("Something", "src/main.rs", None, "detail")]);
        assert_eq!(lines, vec!["Something (src/main.rs:?): detail".to_string()]);
    }

    #[test]
    fn metrics_begin_with_round_one_and_append_each_adaptive_attempt() {
        let round_one = Usage {
            input_tokens: 100,
            output_tokens: 10,
            reasoning_tokens: 0,
            calls_attempted: 0,
            calls_succeeded: 0,
            calls_refused: 0,
            calls_errored: 0,
            cached_tokens: 80,
            embed_tokens: 0,
            cost_usd: 0.01,
        };
        let round_two = Usage {
            input_tokens: 50,
            output_tokens: 5,
            reasoning_tokens: 0,
            calls_attempted: 0,
            calls_succeeded: 0,
            calls_refused: 0,
            calls_errored: 0,
            cached_tokens: 40,
            embed_tokens: 0,
            cost_usd: 0.005,
        };
        let mut metrics = Metrics::start(round_one, Duration::from_millis(20), 3, 2);

        metrics.record(round_two, Duration::from_millis(10), 2, 1);
        metrics.record(Usage::default(), Duration::from_millis(5), 1, 0);

        assert_eq!(metrics.attempted, 3);
        assert_eq!(metrics.candidates_per_pass, vec![3, 2, 1]);
        assert_eq!(metrics.survivors_per_pass, vec![2, 1, 0]);
        assert_eq!(metrics.usage.input_tokens, 150);
        assert_eq!(metrics.usage.output_tokens, 15);
        assert_eq!(metrics.usage.cached_tokens, 120);
        assert!((metrics.usage.cost_usd - 0.015).abs() < f64::EPSILON);
        assert_eq!(metrics.elapsed, Duration::from_millis(35));
    }
}

/// Shared fixtures for lane-level adaptive accounting regressions.
#[cfg(test)]
pub(crate) mod test_support {
    use std::future::Future;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use crate::app::review_execution::{ReviewExecution, ReviewExecutionOptions, ReviewRunContext};
    use crate::harness::mock::MockModel;
    use crate::lanes::large_pr::ReviewMode;
    use crate::ports::model::{Model, Usage};
    use crate::ports::tree::{Found, Lookup, MockTree, TreeReader};
    use tokio::time::Instant;

    pub const PAID_USAGE: Usage = Usage {
        input_tokens: 4,
        output_tokens: 2,
        reasoning_tokens: 0,
        calls_attempted: 0,
        calls_succeeded: 0,
        calls_refused: 0,
        calls_errored: 0,
        cached_tokens: 0,
        embed_tokens: 0,
        cost_usd: 0.125,
    };

    pub fn limited_model(
        first: serde_json::Value,
        second: serde_json::Value,
    ) -> (MockModel, Arc<dyn Model>, Arc<ReviewExecution>) {
        let mock = MockModel::new()
            .with_usage(PAID_USAGE)
            .then(first)
            .then(second);
        let started_at = Instant::now();
        let context = ReviewRunContext::new(ReviewExecutionOptions {
            started_at,
            hard_deadline: started_at + Duration::from_secs(100),
            mode: ReviewMode::Normal,
            max_calls: Some(2),
            soft_deadline_percent: 80,
            drain_seconds: 5,
        });
        let bound = context
            .bind_model(Arc::new(mock.clone()), 10.0)
            .model
            .clone();
        (mock, bound, context.execution())
    }

    #[derive(Debug)]
    pub struct DelayedTree {
        inner: MockTree,
        delay: Duration,
    }

    impl DelayedTree {
        pub fn new(delay: Duration) -> Self {
            Self {
                inner: MockTree::from_files([("src/defs.rs", "fn checked() {}")]),
                delay,
            }
        }
    }

    #[async_trait::async_trait]
    impl TreeReader for DelayedTree {
        async fn lookup(&self, lookup: &Lookup) -> crate::error::Result<Found> {
            tokio::time::sleep(self.delay).await;
            self.inner.lookup(lookup).await
        }

        fn describe(&self) -> String {
            "delayed test tree".into()
        }
    }

    pub async fn capture_trace<T>(run: impl Future<Output = T>) -> (T, String) {
        struct Writer(Arc<Mutex<Vec<u8>>>);

        impl std::io::Write for Writer {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0
                    .lock()
                    .expect("trace buffer")
                    .extend_from_slice(bytes);
                Ok(bytes.len())
            }

            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        // Keep a registered dispatcher alive so callsite registration remains
        // correct when this telemetry event is first emitted alongside
        // unobserved lane tests on other threads.
        let _callsite_registry_guard =
            tracing::Dispatch::new(tracing::subscriber::NoSubscriber::default());
        let bytes = Arc::new(Mutex::new(Vec::new()));
        let writer = bytes.clone();
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_writer(move || Writer(writer.clone()))
            .finish();
        let guard = tracing::subscriber::set_default(subscriber);
        let result = run.await;
        drop(guard);
        let output = String::from_utf8(bytes.lock().expect("trace buffer").clone())
            .expect("trace output is UTF-8");
        (result, output)
    }
}
