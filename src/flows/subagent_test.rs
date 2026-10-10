//! The depth bound, and the things that would quietly remove it.

use super::*;

#[test]
fn a_sub_agent_has_no_way_to_spawn_another() {
    // This is the depth bound. Not a counter that a future edit forgets to
    // thread through, but the absence of anywhere to ask from: the schema a
    // sub-agent answers offers neither questions nor lookups.
    let schema = answer_schema();
    let properties = schema["properties"].as_object().unwrap();
    assert!(!properties.contains_key("questions"));
    assert!(!properties.contains_key("lookups"));
    assert_eq!(schema["additionalProperties"], json!(false));
}

#[test]
fn a_sub_agent_runs_on_the_model_it_was_given_with_the_answer_system() {
    let call = answer_call("vendor/flash", 2, "Does the caller validate?", "diff");
    assert_eq!(call.model, "vendor/flash");
    assert_eq!(call.id, "answer_2");
    assert_eq!(call.system, ANSWER_SYSTEM);
    assert_eq!(call.schema_name, "tinysweeper_subagent_answer");
    assert!(call.prompt.starts_with("diff"));
    assert!(call.prompt.contains("Does the caller validate?"));
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
    let schema = answer_schema();
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
