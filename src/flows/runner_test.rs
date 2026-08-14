//! What `ask_all` must guarantee to the lanes that build on it.

use super::*;
use crate::config::types::Config;
use crate::harness::mock::MockModel;
use crate::ports::model::Usage;

fn config() -> Config {
    crate::config::DEFAULTS
        .parse::<toml::Table>()
        .unwrap()
        .try_into()
        .expect("defaults load")
}

fn call(id: &str) -> Call {
    Call {
        id: id.into(),
        model: "vendor/flash".into(),
        system: format!("system for {id}"),
        prompt: "the evidence".into(),
        schema_name: "tinysweeper_critique".into(),
    }
}

fn schema() -> Value {
    json!({ "type": "object" })
}

async fn ask(model: MockModel, ids: &[&str], budget: f64) -> Vec<Answer> {
    let calls: Vec<Call> = ids.iter().map(|id| call(id)).collect();
    let llm = lane_llm(Arc::new(model), &config(), budget);

    ask_all(llm, LaneId::Critique, &calls, &schema(), None,
        None)
        .await
        .expect("the graph runs")
}

#[tokio::test]
async fn every_reviewer_gets_an_answer_in_the_order_asked() {
    // Lanes zip this against their reviewer list, so a reordering here would
    // attribute one reviewer's findings to another silently.
    let answers = ask(
        MockModel::always(json!({ "summary": "s", "findings": [] })),
        &["a", "b", "c"],
        100.0,
    )
    .await;

    let ids: Vec<&str> = answers.iter().map(|a| a.id.as_str()).collect();
    assert_eq!(ids, vec!["a", "b", "c"]);
    assert!(answers.iter().all(|a| a.value.is_some()));
}

#[tokio::test]
async fn one_reviewer_failing_leaves_the_others_answered() {
    // A council that returns nothing because one member timed out is a review
    // that reads "all clear" for an infrastructure reason.
    let model = MockModel::new()
        .then_error("provider exploded")
        .then(json!({ "summary": "s", "findings": [] }))
        .then(json!({ "summary": "s", "findings": [] }));

    let answers = ask(model, &["a", "b", "c"], 100.0).await;

    let answered = answers.iter().filter(|a| a.value.is_some()).count();
    let failed = answers.iter().filter(|a| a.error.is_some()).count();

    assert_eq!(answered, 2);
    assert_eq!(failed, 1);
}

#[tokio::test]
async fn a_failure_is_reported_rather_than_returned_as_an_empty_answer() {
    // The distinction the lanes depend on: `value: None` means nobody read it,
    // and an empty response means somebody read it and found nothing.
    // Collapsing the two is how an unreviewed file comes back clean.
    let answers = ask(MockModel::new().then_error("down"), &["a"], 100.0).await;

    assert!(answers[0].value.is_none());
    assert!(answers[0].error.is_some());
}

#[tokio::test]
async fn the_model_that_answered_is_reported_not_the_one_requested() {
    // A fallback taking over is exactly the case worth surfacing, and it is
    // invisible by the time findings reach the merge.
    let model = MockModel::always(json!({ "summary": "s", "findings": [] }))
        .answering_as("vendor/fallback");

    let answers = ask(model, &["a"], 100.0).await;
    assert_eq!(answers[0].model, "vendor/fallback");
}

#[tokio::test]
async fn the_budget_refuses_reviewers_once_the_ceiling_is_reached() {
    // Enforced in the capability, so it holds however many calls are in flight
    // — which is what let the fan-out stop being serial.
    let model = MockModel::always(json!({ "summary": "s", "findings": [] })).with_usage(Usage {
        cost_usd: 10.0,
        ..Usage::default()
    });

    let answers = ask(model, &["a", "b", "c"], 1.0).await;

    // Concurrent calls may all start before any has returned, so the guarantee
    // is that the ceiling refuses *some* of them, not exactly which.
    assert!(
        answers.iter().any(|a| a.error.is_some()),
        "the ceiling refused nothing"
    );
}

#[tokio::test]
async fn no_reviewers_is_no_calls() {
    let model = MockModel::new();
    let llm = lane_llm(Arc::new(model.clone()), &config(), 100.0);

    let answers = ask_all(llm, LaneId::Critique, &[], &schema(), None,
        None)
        .await
        .expect("an empty council is not an error");

    assert!(answers.is_empty());
    assert_eq!(model.calls(), 0);
}

#[tokio::test]
async fn each_reviewer_is_asked_with_its_own_prompt() {
    let model = MockModel::always(json!({ "summary": "s", "findings": [] }));
    let llm = lane_llm(Arc::new(model.clone()), &config(), 100.0);
    let calls = vec![call("a"), call("b")];

    ask_all(llm, LaneId::Critique, &calls, &schema(), None,
        None)
        .await
        .expect("runs");

    let systems: Vec<String> = model
        .requests()
        .iter()
        .map(|r| r.messages[0].content.clone())
        .collect();

    assert!(systems.iter().any(|s| s == "system for a"));
    assert!(systems.iter().any(|s| s == "system for b"));
}

#[tokio::test]
async fn a_reviewer_id_that_is_not_a_legal_node_id_still_gets_its_answer() {
    // Agent ids are operator config. If `panel::node_id` and the lookup ever
    // disagree the answer is silently lost and the reviewer reads as failed.
    let awkward = Call {
        id: "security-focused reviewer!".into(),
        ..call("ignored")
    };

    let llm = lane_llm(
        Arc::new(MockModel::always(json!({ "summary": "s", "findings": [] }))),
        &config(),
        100.0,
    );

    let answers = ask_all(llm, LaneId::Critique, &[awkward], &schema(), None,
        None)
        .await
        .expect("runs");

    assert!(answers[0].value.is_some(), "{:?}", answers[0].error);
    assert_eq!(answers[0].id, "security-focused reviewer!");
}

#[tokio::test]
async fn reviewers_run_concurrently_rather_than_one_after_another() {
    // The claim the graph exists to make good on. A council multiplies calls by
    // the number of agents, and run serially that multiplies wall clock too —
    // which is what the per-file loop used to do, because a budget could only
    // be checked once a call had returned.
    //
    // Measured by overlap rather than by clock: each call reports itself in and
    // out, and the assertion is that the peak in-flight count reached the
    // number of reviewers. A serial runner never exceeds one.
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Default)]
    struct Overlapping {
        in_flight: AtomicUsize,
        peak: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl crate::ports::model::Model for Overlapping {
        async fn complete(
            &self,
            _request: crate::ports::model::ModelRequest,
        ) -> crate::error::Result<crate::ports::model::ModelResponse> {
            let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak.fetch_max(now, Ordering::SeqCst);

            // Long enough that a serial runner could not overlap them by
            // accident, short enough not to slow the suite.
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;

            self.in_flight.fetch_sub(1, Ordering::SeqCst);
            Ok(crate::ports::model::ModelResponse {
                value: json!({ "summary": "s", "findings": [] }),
                model: "vendor/flash".into(),
                usage: Usage::default(),
            })
        }
    }

    let model = Arc::new(Overlapping::default());
    let llm = lane_llm(model.clone(), &config(), 100.0);
    let calls: Vec<Call> = ["a", "b", "c"].iter().map(|id| call(id)).collect();

    ask_all(llm, LaneId::Critique, &calls, &schema(), None,
        None)
        .await
        .expect("runs");

    assert_eq!(
        model.peak.load(Ordering::SeqCst),
        3,
        "the reviewers were asked one at a time"
    );
}

// --- sub-agents ---------------------------------------------------------

/// A reviewer answer that asks `questions`.
fn asking(questions: &[&str]) -> Value {
    json!({
        "summary": "I need to check something.",
        "findings": [],
        "questions": questions
            .iter()
            .map(|q| json!({ "question": q, "why": "it decides the finding" }))
            .collect::<Vec<_>>()
    })
}

fn answered(confident: bool) -> Value {
    json!({ "answer": "The caller validates it at line 10.", "confident": confident })
}

fn found(title: &str) -> Value {
    json!({
        "summary": "Settled.",
        "findings": [{
            "path": "a.rs", "existing_code": "x.unwrap()", "rule": "r",
            "title": title, "body": "b", "severity": "high", "confidence": 0.9
        }]
    })
}

async fn ask_with_subagents(model: MockModel, ids: &[&str]) -> Vec<Answer> {
    let calls: Vec<Call> = ids.iter().map(|id| call(id)).collect();
    let llm = lane_llm(Arc::new(model), &config(), 100.0);

    ask_all(
        llm,
        LaneId::Critique,
        &calls,
        &schema(),
        Some("vendor/flash"),
    )
    .await
    .expect("the graph runs")
}

#[tokio::test]
async fn a_reviewer_that_asks_gets_a_second_turn_with_the_answers() {
    // The whole point: the verdict taken is the one made *after* the question
    // was answered, not the hedge that preceded it.
    let model = MockModel::new()
        .then(asking(&["Does the caller validate this?"]))
        .then(answered(true))
        .then(found("Settled finding"));

    let answers = ask_with_subagents(model, &["a"]).await;

    let value = answers[0].value.as_ref().expect("answered");
    assert_eq!(value["findings"][0]["title"], json!("Settled finding"));
}

#[tokio::test]
async fn the_second_turn_sees_the_answer_and_the_first_turn_does_not() {
    let model = MockModel::new()
        .then(asking(&["Does the caller validate this?"]))
        .then(answered(true))
        .then(found("Settled finding"));

    let recorded = MockModel::new()
        .then(asking(&["Does the caller validate this?"]))
        .then(answered(true))
        .then(found("Settled finding"));
    let llm = lane_llm(Arc::new(recorded.clone()), &config(), 100.0);

    ask_all(
        llm,
        LaneId::Critique,
        &[call("a")],
        &schema(),
        Some("vendor/flash"),
    )
    .await
    .expect("runs");
    let _ = model;

    let prompts: Vec<String> = recorded
        .requests()
        .iter()
        .map(|r| r.messages[1].content.clone())
        .collect();

    assert!(
        !prompts[0].contains("validates it at line 10"),
        "the first turn cannot have seen an answer that did not exist yet"
    );
    assert!(
        prompts[2].contains("validates it at line 10"),
        "the second turn must carry the answer: {}",
        prompts[2]
    );
}

#[tokio::test]
async fn a_reviewer_with_no_questions_costs_exactly_one_call() {
    // The common case has to stay free. A follow-up turn for a reviewer that
    // asked nothing is a second call that cannot say anything the first did not.
    let model = MockModel::always(json!({ "summary": "s", "findings": [] }));
    let llm = lane_llm(Arc::new(model.clone()), &config(), 100.0);

    ask_all(
        llm,
        LaneId::Critique,
        &[call("a")],
        &schema(),
        Some("vendor/flash"),
    )
    .await
    .expect("runs");

    assert_eq!(model.calls(), 1);
}

#[tokio::test]
async fn no_second_turn_when_every_sub_agent_failed() {
    // Re-asking with no new evidence is the same turn again, at full price.
    let model = MockModel::new()
        .then(asking(&["Does the caller validate this?"]))
        .then_error("sub-agent down");

    let answers = ask_with_subagents(model, &["a"]).await;

    // The asking turn stands as the reviewer's answer.
    let value = answers[0].value.as_ref().expect("the first turn survives");
    assert_eq!(value["summary"], json!("I need to check something."));
}

#[tokio::test]
async fn an_unconfident_answer_still_earns_a_second_turn() {
    // "The evidence does not say" is a real input to a verdict — it is the
    // difference between a doubt resolved and one that could not be. Hiding it
    // would let the reviewer read silence as confirmation.
    let model = MockModel::new()
        .then(asking(&["Does the caller validate this?"]))
        .then(answered(false))
        .then(found("Reported anyway"));

    let answers = ask_with_subagents(model, &["a"]).await;

    let value = answers[0].value.as_ref().expect("answered");
    assert_eq!(value["findings"][0]["title"], json!("Reported anyway"));
}

#[tokio::test]
async fn questions_are_capped_at_the_documented_number() {
    // The schema asks for a cap; under `json_object` the provider enforces
    // nothing. This is the number of sub-agents actually spawned.
    let many: Vec<String> = (0..10).map(|n| format!("question {n}")).collect();
    let refs: Vec<&str> = many.iter().map(String::as_str).collect();

    let model = MockModel::new()
        .then(asking(&refs))
        .then(answered(true))
        .then(answered(true))
        .then(answered(true))
        .then(found("Settled"));
    let llm = lane_llm(Arc::new(model.clone()), &config(), 100.0);

    ask_all(
        llm,
        LaneId::Critique,
        &[call("a")],
        &schema(),
        Some("vendor/flash"),
    )
    .await
    .expect("runs");

    // One asking turn + the capped sub-agents + one settling turn.
    assert_eq!(
        model.calls(),
        1 + crate::flows::subagent::MAX_QUESTIONS_PER_REVIEWER + 1
    );
}

#[tokio::test]
async fn the_final_turn_is_not_offered_a_way_to_ask_again() {
    // There is genuinely no turn after the second one, so offering `questions`
    // there invites a question nothing will ever answer.
    let model = MockModel::new()
        .then(asking(&["Does the caller validate this?"]))
        .then(answered(true))
        .then(found("Settled"));
    let llm = lane_llm(Arc::new(model.clone()), &config(), 100.0);

    ask_all(
        llm,
        LaneId::Critique,
        &[call("a")],
        &schema(),
        Some("vendor/flash"),
    )
    .await
    .expect("runs");

    let requests = model.requests();
    let first = &requests[0];
    let last = requests.last().expect("a settling turn");

    assert!(
        first.schema["properties"].get("questions").is_some(),
        "the asking turn must be able to ask"
    );
    assert!(
        last.schema["properties"].get("questions").is_none(),
        "the settling turn must not"
    );
    assert!(
        !last.messages[0]
            .content
            .contains("Asking instead of guessing"),
        "nor be told it may"
    );
}

#[tokio::test]
async fn sub_agents_off_never_mentions_them_to_the_reviewer() {
    let model = MockModel::always(json!({ "summary": "s", "findings": [] }));
    let llm = lane_llm(Arc::new(model.clone()), &config(), 100.0);

    ask_all(llm, LaneId::Critique, &[call("a")], &schema(), None,
        None)
        .await
        .expect("runs");

    let requests = model.requests();
    assert!(!requests[0].messages[0].content.contains("Asking instead"));
    assert!(requests[0].schema["properties"].get("questions").is_none());
}

#[tokio::test]
async fn one_reviewers_questions_do_not_disturb_another_reviewers_answer() {
    // The follow-up replaces one slot in a parallel vector. Getting the index
    // wrong would attribute a settled verdict to the reviewer that never asked.
    let model = MockModel::panel_matching(
        &[
            ("system for a", asking(&["Does the caller validate this?"])),
            ("system for b", found("B untouched")),
        ],
        json!({ "summary": "s", "findings": [] }),
    );

    let answers = ask_with_subagents(model, &["a", "b"]).await;

    assert_eq!(answers[1].id, "b");
    assert_eq!(
        answers[1].value.as_ref().unwrap()["findings"][0]["title"],
        json!("B untouched")
    );
}
