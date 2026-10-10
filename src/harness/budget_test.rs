//! Admission bounds include schemas and reject unknown model prices.
use super::*;
use openhuman_embed::complete::{ChatMessage, ResponseFormat};

fn request() -> CompletionRequest {
    CompletionRequest::new(
        "deepseek/deepseek-v4-flash",
        vec![ChatMessage::user("Review")],
    )
    .max_tokens(128)
}

#[test]
fn schemas_and_tool_evidence_increase_the_physical_call_reservation() {
    let ledger = ledger(1.0);
    let plain = call(&ledger, &request(), 0, &Default::default()).unwrap();
    let shaped = call(
        &ledger,
        &request().response_format(ResponseFormat::JsonSchema {
            name: "review".into(),
            schema: serde_json::json!({"description":"x".repeat(4096),"type":"object"}),
        }),
        8192,
        &Default::default(),
    )
    .unwrap();
    assert!(shaped.call.input_tokens > plain.call.input_tokens + 8192);
    assert!(shaped.call.cost_micros > plain.call.cost_micros);
}

#[test]
fn unknown_prices_and_missing_output_caps_cannot_admit_paid_work() {
    let ledger = ledger(1.0);
    let mut unknown = request();
    unknown.model = "unknown/unpriced".into();
    assert!(call(&ledger, &unknown, 0, &Default::default()).is_err());
    let mut uncapped = request();
    uncapped.max_tokens = None;
    assert!(call(&ledger, &uncapped, 0, &Default::default()).is_err());
    uncapped.max_tokens = Some(0);
    assert!(call(&ledger, &uncapped, 0, &Default::default()).is_err());
    assert_eq!(ledger.snapshot().spent.cost_micros, 0);
}

#[test]
fn explicit_alias_bounds_admit_known_rates_and_refuse_invalid_rates() {
    let ledger = ledger(1.0);
    let mut alias = request();
    alias.model = "deep".into();
    let mut prices = std::collections::BTreeMap::from([(
        "deep".into(),
        crate::config::types::BudgetPriceBound {
            input: 0.4,
            cached: 0.4,
            output: 1.8,
        },
    )]);
    let bound = call(&ledger, &alias, 0, &prices).unwrap();
    assert_eq!(bound.call.output_tokens, 128);
    assert_eq!(
        bound.call.cost_micros,
        (bound.call.input_tokens as f64 * 0.4 + 128.0 * 1.8).ceil() as u64
    );
    for invalid in [f64::NAN, f64::INFINITY, -1.0, 0.0] {
        prices.get_mut("deep").unwrap().output = invalid;
        assert!(call(&ledger, &alias, 0, &prices).is_err());
    }
    assert_eq!(ledger.snapshot().spent.cost_micros, 0);
}
