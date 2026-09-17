//! Adaptive extra looks: one reviewer, told cumulatively what it already found.
//!
//! `review.passes = 3` ships as the maximum adaptive depth. Small groups still
//! cost nothing beyond round one. Above one, a lane that already placed and
//! falsified round one's
//! findings for a group may ask the group's first council reviewer — index
//! `0`, deterministically, never the whole council again — to look at the
//! *same* evidence once more, this time told plainly what it already
//! reported and asked to find what earlier passes missed. The sequence stops
//! as soon as a pass contributes nothing distinct and surviving.
//!
//! This is not a second opinion. `src/falsify` already exists for "is this
//! correct", and asking a fresh reviewer "did you miss anything" is the
//! opposite direction: recall, not verification. What makes it cheap enough
//! to offer at all is that it reuses everything round one already paid for —
//! the same prompt prefix, the same evidence, the same [`runner::ask_all`]
//! entry point a whole council would use — for exactly one more call, gated
//! behind a per-group line count so a two-line diff never builds the second
//! prompt.
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

/// Why an adaptive sequence did not ask for another pass.
#[derive(Debug, Clone, Copy)]
pub enum StopReason {
    /// The configured maximum adaptive depth was reached.
    Ceiling,
    /// The reviewer returned no findings.
    Empty,
    /// The call failed or its structured response was malformed.
    Failed,
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
    new_findings: Vec<usize>,
    usage: Usage,
    elapsed: Duration,
    stop_reason: StopReason,
}

impl Metrics {
    /// Start accounting with round one's already-completed review.
    pub fn start(usage: Usage, elapsed: Duration, new_findings: usize) -> Self {
        Self {
            started: Instant::now(),
            attempted: 1,
            new_findings: vec![new_findings],
            usage,
            elapsed,
            stop_reason: StopReason::Ceiling,
        }
    }

    /// Record one attempted extra pass and what survived it.
    pub fn record(&mut self, usage: Usage, elapsed: Duration, new_findings: usize) {
        self.attempted += 1;
        self.new_findings.push(new_findings);
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
            new_findings_per_pass = ?self.new_findings,
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
    let response = match reviewer_responses(
        lane,
        std::slice::from_ref(reviewer),
        std::slice::from_ref(&answer),
    ) {
        Ok(mut responses) => responses.pop().map(|r| r.response),
        Err(err) => {
            tracing::warn!(agent = reviewer.id, %err, "the coverage pass reviewer's answer did not parse");
            None
        }
    };

    Ok(CoverageOutcome {
        response,
        spend,
        looked_up,
        usage,
        elapsed,
    })
}

/// Render round one's surviving findings as the "already found" list this
/// pass's prompt shows: title first, so a reviewer skimming the block can
/// tell at a glance what not to repeat.
///
/// One line per finding, deliberately terse — this is a reminder, not the
/// finding restated in full. `line` reads `?` for a finding round one could
/// not anchor, which is still worth naming so the second pass does not
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
            cached_tokens: 80,
            embed_tokens: 0,
            cost_usd: 0.01,
        };
        let round_two = Usage {
            input_tokens: 50,
            output_tokens: 5,
            cached_tokens: 40,
            embed_tokens: 0,
            cost_usd: 0.005,
        };
        let mut metrics = Metrics::start(round_one, Duration::from_millis(20), 2);

        metrics.record(round_two, Duration::from_millis(10), 1);
        metrics.record(Usage::default(), Duration::from_millis(5), 0);

        assert_eq!(metrics.attempted, 3);
        assert_eq!(metrics.new_findings, vec![2, 1, 0]);
        assert_eq!(metrics.usage.input_tokens, 150);
        assert_eq!(metrics.usage.output_tokens, 15);
        assert_eq!(metrics.usage.cached_tokens, 120);
        assert!((metrics.usage.cost_usd - 0.015).abs() < f64::EPSILON);
        assert_eq!(metrics.elapsed, Duration::from_millis(35));
    }
}
