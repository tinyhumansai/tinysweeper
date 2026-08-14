//! The depth bound, and the things that would quietly remove it.

use super::*;

#[test]
fn a_sub_agent_has_no_way_to_spawn_another() {
    // This is the depth bound. Not a counter that a future edit forgets to
    // thread through, but the absence of any node kind that could recurse.
    let graph = answer_graph("vendor/flash", "system", "prompt");

    for node in &graph.nodes {
        assert!(
            !matches!(node.kind, NodeKind::SubWorkflow),
            "the child graph gained a sub_workflow node: depth is no longer bounded"
        );
    }
}

#[test]
fn the_child_graph_is_only_a_trigger_and_one_agent() {
    // Anything else in here is a new capability a sub-agent has, and every
    // capability is something the security boundary has to re-argue.
    let graph = answer_graph("vendor/flash", "system", "prompt");
    assert_eq!(graph.nodes.len(), 2);

    let kinds: Vec<_> = graph.nodes.iter().map(|n| &n.kind).collect();
    assert!(kinds.iter().any(|k| matches!(k, NodeKind::Trigger)));
    assert!(kinds.iter().any(|k| matches!(k, NodeKind::Agent)));
}

#[test]
fn the_child_graph_compiles() {
    tinyflows::compiler::compile(&answer_graph("vendor/flash", "system", "prompt"))
        .expect("child graph");
}

#[test]
fn a_sub_agent_runs_on_the_model_it_was_given() {
    let graph = answer_graph("vendor/flash", "system", "prompt");
    let agent = graph.nodes.iter().find(|n| n.id == "answer").unwrap();
    assert_eq!(agent.config["model"], json!("vendor/flash"));
}

#[test]
fn the_question_schema_caps_how_many_may_be_asked() {
    // Without the cap a panellist stops reviewing the diff and starts
    // exploring the repository, and the answers arrive too late to be worth it.
    assert_eq!(
        questions_schema()["maxItems"],
        json!(MAX_QUESTIONS_PER_REVIEWER)
    );
}

#[test]
fn a_sub_agent_is_told_it_may_not_reach_a_verdict() {
    // Its output is evidence for the verify round. Evidence that has already
    // made up its mind is worth less than none.
    assert!(ANSWER_SYSTEM.contains("not reviewing"));
    assert!(ANSWER_SYSTEM.contains("do not report problems"));
}

#[test]
fn an_answer_may_be_an_admission_that_the_evidence_does_not_say() {
    let schema = answer_schema(false);
    assert_eq!(schema["required"], json!(["answer", "confident"]));
}

#[test]
fn an_unconfident_answer_is_rendered_as_such_rather_than_dropped() {
    // "The evidence does not say" is a real input to whether a finding
    // survives: it is the difference between a verifier confirming a claim and
    // a verifier having had no way to check it.
    let rendered = render(&[Answered {
        question: "Does the caller validate this?".into(),
        answer: "No caller is visible in the supplied evidence.".into(),
        confident: false,
    }]);

    assert!(rendered.contains("did not settle"), "{rendered}");
    assert!(rendered.contains("No caller is visible"), "{rendered}");
}

#[test]
fn a_confident_answer_carries_no_caveat() {
    let rendered = render(&[Answered {
        question: "Does the caller validate this?".into(),
        answer: "Yes, `parse` rejects empty input at line 10.".into(),
        confident: true,
    }]);

    assert!(!rendered.contains("did not settle"), "{rendered}");
}

#[test]
fn nothing_asked_renders_to_nothing() {
    // An empty block in the prompt is not free: it is prompt suffix, and this
    // lane's whole caching story is about what the suffix contains.
    assert!(render(&[]).is_empty());
}

#[test]
fn a_schema_with_no_properties_still_gains_the_questions_key() {
    // The silent-no-op case. The instruction and the schema are set together,
    // so a schema returned unchanged means a reviewer invited to ask a question
    // it has nowhere to write — rejected under strict mode, dropped under
    // `json_object`, and in both cases the follow-up turn never happens.
    let widened = with_questions(json!({ "type": "object" }));
    assert!(widened["properties"]["questions"].is_object());
}

#[test]
fn widening_a_schema_leaves_its_own_contract_alone() {
    let widened = with_questions(json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["summary"],
        "properties": { "summary": { "type": "string" } }
    }));

    // Optional: a reviewer with nothing to ask answers the schema it always did.
    assert_eq!(widened["required"], json!(["summary"]));
    assert!(widened["properties"]["summary"].is_object());
    assert!(widened["properties"]["questions"].is_object());
}

#[test]
fn a_batch_of_questions_is_one_concurrent_graph() {
    let graph = answers_graph("vendor/flash", &["a".into(), "b".into(), "c".into()], false);

    let fan_out = graph
        .edges
        .iter()
        .filter(|e| e.from_node == "trigger")
        .count();
    assert_eq!(fan_out, 3);

    // And still nothing that could run another graph.
    for node in &graph.nodes {
        assert!(!matches!(node.kind, NodeKind::SubWorkflow), "{}", node.id);
    }
    tinyflows::compiler::compile(&graph).expect("the batch graph compiles");
}

#[test]
fn one_question_failing_leaves_the_others_answerable() {
    for node in answers_graph("vendor/flash", &["a".into(), "b".into()], false)
        .nodes
        .iter()
        .filter(|n| n.id.starts_with("answer_"))
    {
        assert_eq!(node.config["on_error"], json!("continue"), "{}", node.id);
    }
}
