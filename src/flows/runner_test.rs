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

    ask_all(llm, LaneId::Critique, &calls, &schema(), Asking::default())
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

    let answers = ask_all(llm, LaneId::Critique, &[], &schema(), Asking::default())
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

    ask_all(llm, LaneId::Critique, &calls, &schema(), Asking::default())
        .await
        .expect("runs");

    let systems: Vec<String> = model
        .requests()
        .iter()
        .map(|r| r.messages[0].content.clone())
        .collect();

    assert!(systems.iter().any(|s| s.starts_with("system for a")));
    assert!(systems.iter().any(|s| s.starts_with("system for b")));
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

    let answers = ask_all(
        llm,
        LaneId::Critique,
        &[awkward],
        &schema(),
        Asking::default(),
    )
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

    ask_all(llm, LaneId::Critique, &calls, &schema(), Asking::default())
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
        Asking {
            subagent_model: Some("vendor/flash"),
            ..Asking::default()
        },
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
        Asking {
            subagent_model: Some("vendor/flash"),
            ..Asking::default()
        },
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
        Asking {
            subagent_model: Some("vendor/flash"),
            ..Asking::default()
        },
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
        Asking {
            subagent_model: Some("vendor/flash"),
            ..Asking::default()
        },
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
        Asking {
            subagent_model: Some("vendor/flash"),
            ..Asking::default()
        },
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

    ask_all(
        llm,
        LaneId::Critique,
        &[call("a")],
        &schema(),
        Asking::default(),
    )
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

fn lookup_policy(rounds: u8) -> crate::config::types::LookupPolicy {
    crate::config::types::LookupPolicy {
        enabled: true,
        rounds,
        per_round: 3,
        max_chars: 10_000,
        checkout: false,
    }
}

#[tokio::test]
async fn a_reviewer_that_looks_something_up_is_asked_again_with_what_it_read() {
    // The whole point of the loop: the doubt the production model wrote into
    // its summary on opencompany#2313 now becomes a read, and the verdict is
    // taken from the turn that saw the answer.
    let model = MockModel::new()
        .then(json!({
            "summary": "not sure yet",
            "findings": [],
            "lookups": [
                { "kind": "read", "path": "src/ports/events.rs", "start": 1, "end": 3, "why": "cursor semantics" },
                { "kind": "search", "pattern": "fn read_before", "why": "where it lives" }
            ]
        }))
        .then(json!({ "summary": "settled", "findings": [] }));
    let tree = crate::ports::tree::MockTree::from_files([(
        "src/ports/events.rs",
        "/// Reads events with sequence `< before`.\nfn read_before() {}\nfn other() {}\n",
    )]);
    let llm = lane_llm(Arc::new(model.clone()), &config(), 100.0);
    let policy = lookup_policy(2);

    let answers = ask_all(
        llm,
        LaneId::Critique,
        &[call("a")],
        &schema(),
        Asking {
            subagent_model: None,
            tree: Some(&tree),
            lookup: Some(&policy),
            seed: &[],
        },
    )
    .await
    .expect("runs");

    assert_eq!(answers[0].value.as_ref().unwrap()["summary"], "settled");

    let requests = model.requests();
    assert_eq!(requests.len(), 2, "one asking turn, one settled turn");
    let first = &requests[0];
    assert!(
        first.messages[0].content.contains("## Looking things up"),
        "the first turn is told it may look up"
    );
    assert!(first.schema["properties"].get("lookups").is_some());

    let second = &requests[1];
    let evidence = &second.messages[1].content;
    assert!(evidence.contains("## What you looked up"), "{evidence}");
    assert!(
        evidence.contains("sequence `< before`"),
        "the read reached the model"
    );
    assert!(
        evidence.contains("src/ports/events.rs:2: fn read_before() {}"),
        "the search hit reached the model"
    );
    assert!(
        second.schema["properties"].get("lookups").is_some(),
        "with a round left, the second turn may still ask"
    );

    // Sub-agents are off (`subagent_model: None`, the default), so the early
    // return past the sub-agent branch must not skip filling `looked_up` —
    // otherwise the falsifier sees no evidence for a finding this reviewer
    // only reached because it looked something up.
    assert!(
        answers[0].looked_up.contains("## What you looked up"),
        "{}",
        answers[0].looked_up
    );
}

#[tokio::test]
async fn a_reviewer_asking_to_read_a_dotenv_file_is_told_it_is_unavailable() {
    // The lookup loop is a second way for a secret to reach a model: a
    // reviewer that asks to read `.env` must be refused by the tree reader
    // itself, not merely have the answer scrubbed afterwards.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join(".env"), "AWS_SECRET=super-secret-value\n").unwrap();
    let tree = crate::ports::tree::DirTree::new(dir.path());

    let model = MockModel::new()
        .then(json!({
            "summary": "not sure yet",
            "findings": [],
            "lookups": [
                { "kind": "read", "path": ".env", "start": 1, "end": 5, "why": "check config" }
            ]
        }))
        .then(json!({ "summary": "settled", "findings": [] }));
    let llm = lane_llm(Arc::new(model.clone()), &config(), 100.0);
    let policy = lookup_policy(2);

    let answers = ask_all(
        llm,
        LaneId::Critique,
        &[call("a")],
        &schema(),
        Asking {
            subagent_model: None,
            tree: Some(&tree),
            lookup: Some(&policy),
            seed: &[],
        },
    )
    .await
    .expect("runs");

    let requests = model.requests();
    let second = &requests[1];
    let evidence = &second.messages[1].content;
    assert!(evidence.contains("## What you looked up"), "{evidence}");
    assert!(
        !evidence.contains("super-secret-value"),
        "the secret must never reach the rendered lookup block: {evidence}"
    );
    assert!(
        evidence.contains("Not available"),
        "the refusal is said, not silently empty: {evidence}"
    );
    assert!(
        !answers[0].looked_up.contains("super-secret-value"),
        "{}",
        answers[0].looked_up
    );
}

#[tokio::test]
async fn the_last_permitted_round_offers_no_lookups_and_the_loop_ends() {
    // One round: the turn after the lookups answers the plain schema and is
    // not told it may look up, so a reviewer cannot ask for something no
    // turn will answer. A reviewer that keeps asking anyway is settled on
    // what it said.
    let model = MockModel::new()
        .then(json!({
            "summary": "asking",
            "findings": [],
            "lookups": [{ "kind": "read", "path": "a.rs", "why": "x" }]
        }))
        .then(json!({
            "summary": "still asking",
            "findings": [],
            "lookups": [{ "kind": "read", "path": "b.rs", "why": "x" }]
        }))
        .then(json!({ "summary": "never reached", "findings": [] }));
    let tree = crate::ports::tree::MockTree::from_files([("a.rs", "x"), ("b.rs", "y")]);
    let llm = lane_llm(Arc::new(model.clone()), &config(), 100.0);
    let policy = lookup_policy(1);

    let answers = ask_all(
        llm,
        LaneId::Critique,
        &[call("a")],
        &schema(),
        Asking {
            subagent_model: None,
            tree: Some(&tree),
            lookup: Some(&policy),
            seed: &[],
        },
    )
    .await
    .expect("runs");

    let requests = model.requests();
    assert_eq!(requests.len(), 2);
    let last = requests.last().unwrap();
    assert!(last.schema["properties"].get("lookups").is_none());
    assert!(!last.messages[0].content.contains("## Looking things up"));
    assert_eq!(
        answers[0].value.as_ref().unwrap()["summary"],
        "still asking"
    );
}

#[tokio::test]
async fn a_lookup_follow_up_that_fails_does_not_leave_the_provisional_verdict_standing() {
    // The first turn is told its verdict is provisional; if the turn that
    // was to settle it never answers, the file is unreviewed, not approved.
    let model = MockModel::new()
        .then(json!({
            "summary": "provisional",
            "findings": [],
            "lookups": [{ "kind": "read", "path": "a.rs", "why": "x" }]
        }))
        .then_error("provider down");
    let tree = crate::ports::tree::MockTree::from_files([("a.rs", "x")]);
    let llm = lane_llm(Arc::new(model), &config(), 100.0);
    let policy = lookup_policy(1);

    let answers = ask_all(
        llm,
        LaneId::Critique,
        &[call("a")],
        &schema(),
        Asking {
            subagent_model: None,
            tree: Some(&tree),
            lookup: Some(&policy),
            seed: &[],
        },
    )
    .await
    .expect("runs");

    assert!(answers[0].value.is_none(), "{:?}", answers[0]);
    assert!(answers[0].error.is_some());
}

#[tokio::test]
async fn without_a_tree_the_prompt_is_the_plain_one() {
    // Every cassette recorded before lookups existed depends on this: a
    // deployment with no tree sends exactly the prompt it always sent.
    let model = MockModel::always(json!({ "summary": "s", "findings": [] }));
    let llm = lane_llm(Arc::new(model.clone()), &config(), 100.0);
    let policy = lookup_policy(2);

    ask_all(
        llm,
        LaneId::Critique,
        &[call("a")],
        &schema(),
        Asking {
            subagent_model: None,
            tree: None,
            lookup: Some(&policy),
            seed: &[],
        },
    )
    .await
    .expect("runs");

    let request = &model.requests()[0];
    assert_eq!(
        request.messages[0].content,
        format!("system for a{}", crate::harness::prompt::SETTLE_INSTRUCTION),
        "the one turn is the last turn, and is told so; nothing about lookups"
    );
    assert!(request.schema["properties"].is_null());
}
