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
    let monetary_input_tokens =
        (serde_json::to_vec(&alias).unwrap().len() as u64).div_ceil(4) + 1024;
    assert_eq!(
        bound.call.cost_micros,
        (monetary_input_tokens as f64 * 0.4 + 16.0 * 1.8).ceil() as u64
    );
    for invalid in [f64::NAN, f64::INFINITY, -1.0, 0.0] {
        prices.get_mut("deep").unwrap().output = invalid;
        assert!(call(&ledger, &alias, 0, &prices).is_err());
    }
    assert_eq!(ledger.snapshot().spent.cost_micros, 0);
}

/// Production deep-route prices, including the bounded 64k retry cap.
fn deep_prices() -> std::collections::BTreeMap<String, crate::config::types::BudgetPriceBound> {
    std::collections::BTreeMap::from([(
        "deep".into(),
        crate::config::types::BudgetPriceBound {
            input: 0.4,
            cached: 0.4,
            output: 1.8,
        },
    )])
}

fn typical_deep_request(cap: u32) -> CompletionRequest {
    CompletionRequest::new("deep", vec![ChatMessage::user("code".repeat(18_000))]).max_tokens(cap)
}

#[test]
fn budget_input_money_estimate_uses_quarter_bytes_with_bounded_framing() {
    // CallBudget.input_tokens is also Embed's serialized-byte safety bound.
    // Observe the monetary input estimate without weakening that independent guard.
    let request = typical_deep_request(1);
    let bytes = serde_json::to_vec(&request).unwrap().len() as u64;
    let extra_bytes = 8192;
    let bound = call(&ledger(1.0), &request, extra_bytes, &deep_prices()).unwrap();
    let estimated_tokens = (bytes + extra_bytes).div_ceil(4);
    let minimum = (estimated_tokens as f64 * 0.4 + 1.8).floor() as u64;
    let maximum = ((estimated_tokens + 4096) as f64 * 0.4 + 1.8).ceil() as u64;
    assert!(
        (minimum..=maximum).contains(&bound.call.cost_micros),
        "input reservation {} micros should be within {minimum}..={maximum} for {bytes} request bytes and {extra_bytes} tool bytes",
        bound.call.cost_micros
    );
}

#[test]
fn budget_expected_output_reservation_preserves_the_provider_output_cap() {
    let request = typical_deep_request(16_000);
    let bound = call(&ledger(1.0), &request, 0, &deep_prices()).unwrap();
    assert_eq!(
        bound.call.output_tokens, 16_000,
        "the provider cap stays intact"
    );
    assert_eq!(request.max_tokens, Some(16_000));
    assert!(
        bound.call.cost_micros < 28_800,
        "a cold-start expected output plus this input should reserve less than the full output cap alone; got {} micros",
        bound.call.cost_micros
    );
}

#[test]
fn output_reservation_uses_the_lane_model_rolling_p90_bounded_by_the_cap() {
    let history = OutputHistory::default();
    for output in [
        1_000, 2_000, 4_000, 8_000, 12_000, 20_000, 30_000, 40_000, 50_000, 60_000,
    ] {
        history.record("review", "deep", output, 64_000);
    }
    let request = typical_deep_request(64_000).response_format(ResponseFormat::JsonSchema {
        name: "review".into(),
        schema: serde_json::json!({"type":"object"}),
    });
    let bound = call_with_history(
        &ledger(1.0),
        &request,
        0,
        &deep_prices(),
        "review",
        &history,
    )
    .unwrap();
    assert_eq!(
        bound.call.output_tokens, 64_000,
        "physical cap remains unchanged"
    );
    let bytes = serde_json::to_vec(&request).unwrap().len() as u64;
    let input = (bytes.div_ceil(4) + 1024) as f64 * 0.4;
    assert_eq!(
        bound.call.cost_micros,
        (input + 50_000.0 * 1.8).ceil() as u64
    );
    assert!(bound.call.cost_micros < (input + 64_000.0 * 1.8).ceil() as u64);

    let small_cap = typical_deep_request(10_000).response_format(ResponseFormat::JsonSchema {
        name: "review".into(),
        schema: serde_json::json!({"type":"object"}),
    });
    let capped = call_with_history(
        &ledger(1.0),
        &small_cap,
        0,
        &deep_prices(),
        "review",
        &history,
    )
    .unwrap();
    assert_eq!(capped.call.output_tokens, 10_000);
    assert_eq!(
        capped.call.cost_micros,
        (((serde_json::to_vec(&small_cap).unwrap().len() as u64).div_ceil(4) + 1024) as f64)
            .mul_add(0.4, 10_000.0 * 1.8)
            .ceil() as u64
    );
    assert_eq!(history.p90("unseen_lane", "deep", 64_000), 8_000);
    history.record("zero_output_lane", "deep", 0, 1_000);
    assert_eq!(history.p90("zero_output_lane", "deep", 1_000), 0);
}

#[test]
fn budget_eight_typical_deep_calls_fit_even_with_the_bounded_retry_cap() {
    use openhuman_embed::budget::Spend;
    let ledger = ledger(1.0);
    let bound = call(&ledger, &typical_deep_request(64_000), 0, &deep_prices()).unwrap();
    // Keep all permits live: sequential settle would hide fan-out serialization.
    let mut reservations = Vec::new();
    for index in 0..8 {
        let reservation = ledger.reserve(Spend {
            tokens: bound.call.input_tokens + u64::from(bound.call.output_tokens),
            cost_micros: bound.call.cost_micros,
        });
        assert!(
            reservation.is_ok(),
            "typical deep call {} of 8 must fit under $1: {reservation:?}",
            index + 1
        );
        reservations.push(reservation.unwrap());
    }
    assert!(ledger.snapshot().reserved.cost_micros <= 1_000_000);
    for reservation in reservations {
        reservation.settle(Spend {
            tokens: 18_500,
            cost_micros: 1000,
        });
    }
    assert_eq!(ledger.snapshot().reserved.cost_micros, 0);
    assert_eq!(ledger.snapshot().spent.cost_micros, 8000);
}

#[tokio::test]
async fn budget_reported_five_cent_review_spend_does_not_exhaust_admission() {
    use crate::harness::fake_gateway::{FakeGateway, Reply};
    use crate::ports::model::{Message, Model, ModelRequest};
    use serde_json::json;
    let gateway = FakeGateway::start(
        (0..50)
            .map(|_| {
                Reply::completion(
                    "deep",
                    r#"{"summary":"checked"}"#,
                    "stop",
                    json!({"prompt_tokens":18000,"completion_tokens":500,"cost":0.001}),
                )
            })
            .collect(),
    )
    .await;
    let mut config: crate::config::types::Config = crate::config::DEFAULTS
        .parse::<toml::Table>()
        .unwrap()
        .try_into()
        .unwrap();
    config.models.base_url = gateway.base_url.clone();
    config.models.budget_prices = deep_prices();
    config.models.routes.clear();
    config.models.fallback.clear();
    let model = crate::harness::embed::GatewayModel::with_key(&config.models, "fixture".into());
    let model = model.scoped_budget(1.0).unwrap();
    let request = ModelRequest {
        model: "deep".into(),
        messages: vec![Message::user("code".repeat(18_000))],
        schema: json!({"type":"object","properties":{"summary":{"type":"string"}},"required":["summary"],"additionalProperties":false}),
        schema_name: "review".into(),
        reasoning_effort: None,
        max_tokens: 64_000,
    };
    let mut reported = 0.0;
    for index in 0..50 {
        let response = model
            .complete(request.clone())
            .await
            .unwrap_or_else(|error| {
                panic!(
                    "review refused at call {} with only {reported} real USD spent: {error}",
                    index + 1
                )
            });
        reported += response.usage.cost_usd;
    }
    assert_eq!(gateway.requests().len(), 50);
    assert!((reported - 0.05).abs() < 1e-9);
}
