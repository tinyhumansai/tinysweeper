//! Metadata-only Langfuse batches for agent model and repository tool events.
use super::{LangfuseExporter, epoch_ms, iso};
use openhuman_embed::observe::{TurnObservation, TurnObserver, TurnTrace};
use serde_json::{Value, json};

fn batch(kind: &str, id: &str, body: Value) -> Value {
    json!({"batch":[{"id":format!("{id}-{kind}"),"timestamp":iso(epoch_ms()),"type":kind,"body":body}]})
}

fn observation(event: &TurnObservation) -> Value {
    match event {
        TurnObservation::Model { run_id, call_id, requested_model, answered_model,
            finish_reason, duration_ms, failed, usage, .. } => {
            let id = format!("{run_id}-{}", call_id.as_deref().unwrap_or("model"));
            let end = epoch_ms();
            batch("generation-create", &id, json!({
                "id":id,"traceId":run_id,"name":"review model call",
                "startTime":iso(end.saturating_sub((*duration_ms).min(i64::MAX as u64) as i64)),"endTime":iso(end),
                "model":answered_model.as_ref().or(requested_model.as_ref()),
                "level":if *failed {"ERROR"} else {"DEFAULT"},
                "metadata":{"requestedModel":requested_model,"finishReason":finish_reason},
                "usage":usage.as_ref().map(|usage|json!({"input":usage.input_tokens,"output":usage.output_tokens,
                    "total":usage.input_tokens.saturating_add(usage.output_tokens)})),
                "costDetails":usage.as_ref().and_then(|usage|usage.cost_usd).map(|cost|json!({"total":cost})),
            }))
        }
        TurnObservation::Tool { run_id, call_id, name, failed, duration_ms, .. } => {
            let id = format!("{}-{call_id}", run_id.as_deref().unwrap_or("review"));
            let mut body = json!({"id":id,"traceId":run_id,"name":name,
                "metadata":{"failed":failed,"durationMs":duration_ms}});
            let kind = if let Some(failed) = failed {
                body["endTime"] = json!(iso(epoch_ms()));
                body["level"] = json!(if *failed {"ERROR"} else {"DEFAULT"});
                "span-update"
            } else {
                body["startTime"] = json!(iso(epoch_ms()));
                "span-create"
            };
            batch(kind, &id, body)
        }
    }
}

impl TurnObserver for LangfuseExporter {
    fn on_event(&self, event: &TurnObservation) {
        self.enqueue(observation(event));
    }

    fn on_turn(&self, trace: &TurnTrace<'_>) {
        let id = trace.run_id.as_deref().unwrap_or(trace.session_id);
        let mut body = json!({"id":id,"name":"tinysweeper agentic review",
            "sessionId":trace.session_id,"timestamp":iso(epoch_ms()),
            "metadata":{"success":trace.success,"durationMs":trace.latency.as_millis(),
                "finishReason":trace.finish_reason,"answeredModel":trace.answered_model,
                "failure":trace.failure.map(|failure|format!("{failure:?}"))}});
        if let Some(environment) = &self.environment {
            body["environment"] = json!(environment);
        }
        self.enqueue(batch("trace-create", id, body));
    }
}

#[cfg(test)]
#[path = "langfuse_turn_test.rs"]
mod tests;
