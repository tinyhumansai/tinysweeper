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

    ask_all(llm, LaneId::Critique, &calls, &schema(), None)
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

    let answers = ask_all(llm, LaneId::Critique, &[], &schema(), None)
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

    ask_all(llm, LaneId::Critique, &calls, &schema(), None)
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

    let answers = ask_all(llm, LaneId::Critique, &[awkward], &schema(), None)
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

    ask_all(llm, LaneId::Critique, &calls, &schema(), None)
        .await
        .expect("runs");

    assert_eq!(
        model.peak.load(Ordering::SeqCst),
        3,
        "the reviewers were asked one at a time"
    );
}
