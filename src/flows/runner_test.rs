//! End-to-end panel behaviour, against a canned model.
//!
//! These are the tests that pin the three-round contract: what reaches a
//! contributor, what is suppressed, and what a failure looks like from outside.

use super::*;
use crate::config::types::Config;
use crate::harness::mock::MockModel;
use crate::ports::model::Usage;

fn config() -> Config {
    let defaults = crate::config::DEFAULTS;
    defaults
        .parse::<toml::Table>()
        .unwrap()
        .try_into()
        .expect("defaults load")
}

fn finding_json(rule: &str, title: &str) -> Value {
    json!({
        "path": "a.rs",
        "existing_code": "x.unwrap()",
        "rule": rule,
        "title": title,
        "body": "it can panic",
        "severity": "high",
        "confidence": 0.9
    })
}

/// A response every lens gives: one finding.
fn proposing(rule: &str, title: &str) -> Value {
    json!({
        "summary": "Looked at the diff.",
        "findings": [finding_json(rule, title)],
        "resolved": []
    })
}

fn silent() -> Value {
    json!({ "summary": "Nothing to report.", "findings": [], "resolved": [] })
}

fn verdict(real: bool) -> Value {
    json!({ "real": real, "why": "because" })
}

async fn run_panel(model: MockModel, lane: LaneId, budget: f64) -> PanelOutcome {
    let config = config();
    let schema = json!({ "type": "object", "properties": {} });

    run(
        Arc::new(model),
        &config,
        budget,
        PanelRequest {
            lane,
            schema,
            suffix: "the diff",
            system_of: &|lens: &Lens| format!("system {}", lens.id),
        },
    )
    .await
}

#[tokio::test]
async fn a_finding_every_verifier_confirms_survives() {
    // `tests` has two lenses, then three verifiers for the one deduped
    // proposal.
    let model = MockModel::new()
        .then(proposing("unwrap", "Avoid unwrap"))
        .then(proposing("unwrap", "Avoid unwrap"))
        .then(verdict(true))
        .then(verdict(true))
        .then(verdict(true));

    let outcome = run_panel(model, LaneId::Tests, 100.0).await;

    assert_eq!(outcome.findings.len(), 1);
    assert_eq!(outcome.findings[0].title, "Avoid unwrap");
}

#[tokio::test]
async fn a_finding_the_verifiers_refute_never_reaches_a_contributor() {
    // The whole point of the verify round. Both lenses proposed it and it is
    // still dropped, because agreement between proposers is not evidence.
    let model = MockModel::new()
        .then(proposing("unwrap", "Avoid unwrap"))
        .then(proposing("unwrap", "Avoid unwrap"))
        .then(verdict(false))
        .then(verdict(false))
        .then(verdict(true));

    let outcome = run_panel(model, LaneId::Tests, 100.0).await;

    assert!(outcome.findings.is_empty());
}

#[tokio::test]
async fn one_lens_failing_does_not_fail_the_panel() {
    // A panel that fails whole because one member timed out is a review that
    // reports "all clear" for an infrastructure reason.
    let model = MockModel::new()
        .then_error("provider exploded")
        .then(proposing("unwrap", "Avoid unwrap"))
        .then(verdict(true))
        .then(verdict(true))
        .then(verdict(true));

    let outcome = run_panel(model, LaneId::Tests, 100.0).await;

    assert_eq!(outcome.findings.len(), 1);
    assert!(!outcome.failures.is_empty(), "the failure must be reported");
}

#[tokio::test]
async fn a_lens_answering_off_schema_is_dropped_rather_than_guessed_at() {
    let model = MockModel::new()
        // `findings` must be an array; a string genuinely fails to deserialize.
        // (An answer that merely omits keys does not: every `LaneResponse`
        // field carries `serde(default)`, so `{}` is a valid empty review.)
        .then(json!({ "summary": "s", "findings": "not an array" }))
        .then(proposing("unwrap", "Avoid unwrap"))
        .then(verdict(true))
        .then(verdict(true))
        .then(verdict(true));

    let outcome = run_panel(model, LaneId::Tests, 100.0).await;

    assert_eq!(outcome.findings.len(), 1);
    assert!(!outcome.failures.is_empty());
}

#[tokio::test]
async fn a_panel_that_finds_nothing_reports_nothing_and_costs_two_calls() {
    let model = MockModel::new().then(silent()).then(silent());

    let outcome = run_panel(model, LaneId::Tests, 100.0).await;

    assert!(outcome.findings.is_empty());
    assert!(outcome.failures.is_empty());
}

#[tokio::test]
async fn every_lens_failing_leaves_a_summary_that_says_so() {
    // Not an empty clean-looking result. A human reading the check has to be
    // able to tell "nothing was wrong" from "nothing was looked at".
    let model = MockModel::new()
        .then_error("down")
        .then_error("down")
        .then_error("down");

    let outcome = run_panel(model, LaneId::Tests, 100.0).await;

    assert!(outcome.findings.is_empty());
    assert!(
        outcome.summary.contains("No panellist"),
        "{}",
        outcome.summary
    );
}

#[tokio::test]
async fn the_budget_stops_a_panel_partway_rather_than_after() {
    // Enforced inside the capability, so it holds however many calls are in
    // flight — which is what let the fan-out stop being serial.
    let model = MockModel::always(proposing("unwrap", "Avoid unwrap")).with_usage(Usage {
        cost_usd: 10.0,
        ..Usage::default()
    });

    let outcome = run_panel(model, LaneId::Tests, 1.0).await;

    // The first call lands (nothing was spent when it started); every later one
    // is refused, so no proposal is ever verified and none survives.
    assert!(outcome.findings.is_empty());
    assert!(outcome.spend.cost_usd() <= 10.0);
}

#[tokio::test]
async fn the_panels_spend_counts_every_round() {
    let model = MockModel::always(json!({
        "summary": "s", "findings": [], "resolved": []
    }))
    .with_usage(Usage {
        input_tokens: 100,
        cost_usd: 0.001,
        ..Usage::default()
    });

    let outcome = run_panel(model, LaneId::Tests, 100.0).await;

    // Two lenses, no proposals, so no verify round: two calls.
    assert_eq!(outcome.spend.usage.input_tokens, 200);
}

#[tokio::test]
async fn a_lane_with_no_panel_makes_no_call_at_all() {
    // `commits` has no lenses. Running one would be spending money on a lane
    // whose verdict is a regular expression's.
    let outcome = run_panel(MockModel::new(), LaneId::Commits, 100.0).await;

    assert!(outcome.findings.is_empty());
    assert_eq!(outcome.spend.cost_usd(), 0.0);
}

#[tokio::test]
async fn resolutions_survive_to_the_outcome() {
    let model = MockModel::new()
        .then(json!({
            "summary": "s",
            "findings": [],
            "resolved": ["Handle the empty case"]
        }))
        .then(silent());

    let outcome = run_panel(model, LaneId::Tests, 100.0).await;

    assert_eq!(outcome.resolved, vec!["Handle the empty case"]);
}

#[test]
fn the_verify_prompt_asks_for_refutation_not_assessment() {
    // A model asked "is this right?" agrees. A model asked "show this is
    // wrong" goes and looks.
    assert!(VERIFY_SYSTEM.contains("refute"));
    assert!(
        VERIFY_SYSTEM.contains("set it to false"),
        "the tie-break has to default to dropping the finding"
    );
}

#[test]
fn the_questions_key_is_optional_so_existing_responses_still_validate() {
    let schema = schema_with_questions(json!({
        "type": "object",
        "required": ["summary"],
        "properties": { "summary": { "type": "string" } }
    }));

    assert!(schema["properties"]["questions"].is_object());
    assert_eq!(schema["required"], json!(["summary"]));
}

#[test]
fn a_lens_charter_tells_it_to_ask_rather_than_guess() {
    let composed = system_with_charter(
        "PREFIX",
        &Lens {
            id: "x",
            charter: "CHARTER",
        },
    );

    assert!(composed.starts_with("PREFIX"));
    assert!(composed.contains("CHARTER"));
    assert!(composed.contains("questions"));
}

#[test]
fn only_the_capped_number_of_questions_is_dispatched() {
    // The schema asks for a cap; a provider may honour it loosely. This is the
    // number of sub-agents that actually get spawned.
    let value = json!({
        "questions": (0..10)
            .map(|n| json!({ "question": format!("q{n}"), "why": "w" }))
            .collect::<Vec<_>>()
    });

    assert_eq!(
        read_questions(&value).len(),
        subagent::MAX_QUESTIONS_PER_LENS
    );
}

#[test]
fn a_blank_question_is_not_dispatched() {
    let value = json!({
        "questions": [
            { "question": "   ", "why": "w" },
            { "question": "real question", "why": "w" }
        ]
    });

    assert_eq!(read_questions(&value), vec!["real question"]);
}
