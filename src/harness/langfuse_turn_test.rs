//! Agent telemetry cannot copy repository content into Langfuse batches.
use super::*;

#[test]
fn tool_payloads_are_omitted_even_when_the_observer_receives_them() {
    let event = TurnObservation::Tool {
        run_id: Some("run-1".into()), call_id: "read-1".into(), name: "repo_read".into(),
        failed: Some(false), duration_ms: Some(10),
        input: Some(json!({"secret":"private request"})),
        output: Some(json!({"secret":"private repository content"})),
    };
    let payload = observation(&event);
    assert_eq!(payload["batch"][0]["type"], "span-update");
    assert_eq!(payload["batch"][0]["body"]["traceId"], "run-1");
    assert!(!payload.to_string().contains("private"));
}

#[test]
fn model_observations_preserve_actual_model_and_cost_without_messages() {
    let event = TurnObservation::Model {
        run_id: "run-1".into(), call_id: Some("call-1".into()),
        requested_model: Some("primary".into()), answered_model: Some("actual".into()),
        finish_reason: Some("stop".into()), duration_ms: 5, failed: false,
        usage: Some(openhuman_embed::observe::ObservedUsage {
            input_tokens: 10, output_tokens: 2, cost_usd: Some(0.01), ..Default::default()
        }), input: Some(json!("private prompt")), output: Some(json!("private answer")),
    };
    let payload = observation(&event);
    assert_eq!(payload["batch"][0]["body"]["model"], "actual");
    assert_eq!(payload["batch"][0]["body"]["costDetails"]["total"], 0.01);
    assert!(!payload.to_string().contains("private"));
}
