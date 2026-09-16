//! The opt-in second look: one reviewer, once more, told what it already
//! found.
//!
//! `review.passes = 1` ships as the default and costs nothing beyond round
//! one. Above one, a lane that already placed and falsified round one's
//! findings for a group may ask the group's first council reviewer — index
//! `0`, deterministically, never the whole council again — to look at the
//! *same* evidence once more, this time told plainly what it already
//! reported and asked to find what a first pass misses.
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
use crate::ports::model::Spend;

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

    let answers = runner::ask_all(llm, lane, std::slice::from_ref(&call), schema, asking).await?;
    let Some(answer) = answers.into_iter().next() else {
        return Ok(CoverageOutcome::default());
    };

    let mut spend = Spend::default();
    spend.note(&answer.model);
    let looked_up = answer.looked_up.clone();

    let responses = reviewer_responses(
        lane,
        std::slice::from_ref(reviewer),
        std::slice::from_ref(&answer),
    )?;

    Ok(CoverageOutcome {
        response: responses.into_iter().next().map(|r| r.response),
        spend,
        looked_up,
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
}
