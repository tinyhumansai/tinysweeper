//! Wire-format tests for the OpenRouter embeddings client.
//!
//! These are offline. They exercise the part that goes wrong quietly — reading
//! the response — rather than the part that goes wrong loudly. A malformed
//! request fails on the first call and somebody notices; a response decoded in
//! the wrong order produces an index where every chunk is filed under its
//! neighbour's vector, retrieval that is confidently irrelevant, and no error
//! anywhere.

use super::*;

fn signature(dims: usize) -> EmbedSignature {
    EmbedSignature {
        provider: "openrouter".into(),
        model: "openai/text-embedding-3-small".into(),
        dims,
    }
}

fn body(rows: &[(usize, Vec<f32>)], usage: &str) -> String {
    let data: Vec<String> = rows
        .iter()
        .map(|(index, embedding)| {
            format!(
                r#"{{"object":"embedding","index":{index},"embedding":{}}}"#,
                serde_json::to_string(embedding).expect("serialises")
            )
        })
        .collect();
    format!(r#"{{"object":"list","data":[{}]{usage}}}"#, data.join(","))
}

#[test]
fn vectors_come_back_in_input_order_not_arrival_order() {
    // The gateway is under no obligation to return rows in order, and several
    // upstreams do not. Trusting arrival order would put each vector on the
    // wrong chunk — silently, and undetectably from the retrieval layer.
    let raw = body(
        &[
            (2, vec![0.3, 0.3]),
            (0, vec![0.1, 0.1]),
            (1, vec![0.2, 0.2]),
        ],
        "",
    );
    let parsed = parse(&raw).expect("parses");
    let vectors = parsed.vectors(3, 2).expect("orders");

    assert_eq!(vectors[0], vec![0.1, 0.1]);
    assert_eq!(vectors[1], vec![0.2, 0.2]);
    assert_eq!(vectors[2], vec![0.3, 0.3]);
}

#[test]
fn a_duplicated_index_is_refused_rather_than_silently_reordered() {
    // Two rows claiming index 0 sort into a stable but arbitrary order, and one
    // input would end up with no vector of its own.
    let raw = body(&[(0, vec![0.1, 0.1]), (0, vec![0.2, 0.2])], "");
    let parsed = parse(&raw).expect("parses");
    let err = parsed.vectors(2, 2).expect_err("refuses");
    assert!(err.to_string().contains("index"), "{err}");
}

#[test]
fn a_short_response_is_refused() {
    // One vector for two chunks: whichever chunk lost is indexed against
    // nothing, and the caller cannot tell which.
    let raw = body(&[(0, vec![0.1, 0.1])], "");
    let parsed = parse(&raw).expect("parses");
    let err = parsed.vectors(2, 2).expect_err("refuses");
    assert!(err.to_string().contains("for 2 inputs"), "{err}");
}

#[test]
fn a_vector_of_the_wrong_width_is_refused() {
    // The signature is the index partition key and the search index declares
    // the width at creation. A vector of another width means the configured
    // signature and the model disagree, and the whole index would be built
    // against the wrong one.
    let raw = body(&[(0, vec![0.1, 0.2, 0.3])], "");
    let parsed = parse(&raw).expect("parses");
    let err = parsed.vectors(1, 2).expect_err("refuses");
    assert!(err.to_string().contains("dimensional"), "{err}");
}

#[test]
fn a_reported_cost_is_preferred_over_the_local_table() {
    // The gateway is quoting what it billed, including routing markup. The
    // local table is hand-maintained and goes stale silently.
    let usage = r#","usage":{"prompt_tokens":11,"total_tokens":11,"cost":0.00000022}"#;
    let parsed = parse(&body(&[(0, vec![0.1, 0.2])], usage)).expect("parses");
    let reported = parsed.usage.as_ref().expect("usage");

    let embedded = Embedded::charged(
        reported.prompt_tokens.max(reported.total_tokens),
        reported.cost.expect("cost"),
    );

    assert_eq!(embedded.usage.embed_tokens, 11);
    assert!(
        (embedded.usage.cost_usd - 0.00000022).abs() < f64::EPSILON,
        "{}",
        embedded.usage.cost_usd
    );
}

#[test]
fn a_surplus_micro_dollar_cost_is_read_through_the_ladder() {
    // The body the ladder relays from a Surplus seller: `cost: 0` from the
    // seller's own upstream, `buyer_cost_micro` from Surplus.
    let parsed = parse(&body(
        &[(0, vec![0.0; 4])],
        r#","usage":{"prompt_tokens":10,"cost":0,"is_byok":true,"buyer_cost_micro":3}"#,
    ))
    .expect("parses");
    let usage = parsed.usage.expect("usage");
    assert!((usage.charged().unwrap() - 0.000003).abs() < 1e-12);

    let negative = parse(&body(
        &[(0, vec![0.0; 4])],
        r#","usage":{"prompt_tokens":10,"buyer_cost_micro":-3}"#,
    ))
    .expect("parses");
    assert_eq!(negative.usage.expect("usage").charged(), None);
}

#[test]
fn a_response_without_a_cost_still_uses_the_real_token_count() {
    // Tokens but no price: the count is authoritative even when the price is
    // not, so it must not fall all the way back to estimating both.
    let usage = r#","usage":{"prompt_tokens":97,"total_tokens":97}"#;
    let parsed = parse(&body(&[(0, vec![0.1, 0.2])], usage)).expect("parses");
    let reported = parsed.usage.as_ref().expect("usage");

    assert!(reported.cost.is_none());
    let embedded = Embedded::metered(
        &signature(2),
        reported.prompt_tokens.max(reported.total_tokens),
        vec![vec![0.1, 0.2]],
    );
    assert_eq!(embedded.usage.embed_tokens, 97);
}

#[test]
fn a_response_with_no_usage_at_all_still_parses() {
    // Degrading to an estimate is correct here; failing the call because the
    // gateway omitted an optional block would take the index down over
    // accounting.
    let parsed = parse(&body(&[(0, vec![0.1, 0.2])], "")).expect("parses");
    assert!(parsed.usage.is_none());
    assert_eq!(parsed.vectors(1, 2).expect("orders").len(), 1);
}

#[test]
fn a_body_that_is_not_json_names_itself_in_the_error() {
    // A proxy returning an HTML error page is a common misconfiguration, and
    // "expected value at line 1 column 1" tells nobody what happened.
    let err = parse("<html><body>502 Bad Gateway</body></html>").expect_err("refuses");
    let message = err.to_string();
    assert!(message.contains("cannot read"), "{message}");
    assert!(message.contains("502"), "{message}");
}

#[test]
fn the_key_never_reaches_the_debug_output() {
    // Secrets are reported by type and location only, and a derived `Debug` on
    // a struct holding an API key is the easiest way to break that.
    let embedder =
        OpenRouterEmbedder::with_key(signature(1536), "sk-or-v1-not-a-real-key".to_string(), "")
            .expect("builds");
    let rendered = format!("{embedder:?}");

    assert!(!rendered.contains("not-a-real-key"), "{rendered}");
    assert!(rendered.contains("redacted"), "{rendered}");
}

#[test]
fn a_missing_key_is_a_configuration_error_naming_the_variable() {
    // A variable name no process would have set, so this needs no mutation of
    // the environment to be a reliable "absent" case.
    let err = OpenRouterEmbedder::new(signature(1536), "TINYSWEEPER_ABSENT_KEY_5f3a2b1c", "")
        .expect_err("refuses");
    assert!(
        err.to_string().contains("TINYSWEEPER_ABSENT_KEY_5f3a2b1c"),
        "{err}"
    );
}

#[test]
fn the_ladder_is_built_through_the_same_client_at_the_address_it_was_given() {
    // A ladder-shaped signature: the model is a ladder name, and the width is
    // the one every rung in it returns.
    let ladder = EmbedSignature {
        provider: "ladder".into(),
        model: "vectors".into(),
        dims: 1024,
    };
    let embedder = OpenRouterEmbedder::with_key(
        ladder.clone(),
        "unused".to_string(),
        "http://host.docker.internal:6969/v1/embeddings",
    )
    .expect("builds");
    assert_eq!(embedder.signature(), ladder);
    assert_eq!(
        embedder.url,
        "http://host.docker.internal:6969/v1/embeddings"
    );

    // And the errors it raises name the provider it is, not OpenRouter.
    let err = OpenRouterEmbedder::new(ladder, "TINYSWEEPER_ABSENT_KEY_5f3a2b1c", "")
        .expect_err("refuses");
    assert!(err.to_string().contains("`ladder`"), "{err}");
}

#[tokio::test]
async fn requests_are_paced_to_the_configured_rate() {
    // 600 a minute is one every 100ms: three paced calls take at least 200ms
    // between the first and the last. Zero means no pacing at all.
    let paced = OpenRouterEmbedder::with_key(signature(4), "unused".into(), "")
        .expect("builds")
        .with_requests_per_minute(600);
    let started = std::time::Instant::now();
    paced.pace().await;
    paced.pace().await;
    paced.pace().await;
    assert!(started.elapsed() >= std::time::Duration::from_millis(200));

    let unpaced = OpenRouterEmbedder::with_key(signature(4), "unused".into(), "")
        .expect("builds")
        .with_requests_per_minute(0);
    for _ in 0..3 {
        unpaced.pace().await;
    }
    assert!(
        unpaced.last_sent.lock().await.is_none(),
        "no cap means nothing is timed at all"
    );
}

#[test]
fn a_ladder_error_names_the_ladder() {
    let bad = parse("not json").expect_err("refused");
    assert!(bad.to_string().contains("openrouter embeddings"));
    let relabelled = relabel(bad, "ladder");
    assert!(relabelled.to_string().starts_with("model: ladder embeddings") || relabelled.to_string().contains("ladder embeddings"), "{relabelled}");
    assert!(!relabelled.to_string().contains("openrouter"), "{relabelled}");
}

#[test]
fn a_provider_this_client_does_not_serve_is_refused() {
    let voyage = EmbedSignature {
        provider: "voyage".into(),
        model: "voyage-code-3".into(),
        dims: 1024,
    };
    let err = OpenRouterEmbedder::with_key(voyage, "unused".to_string(), "").expect_err("refuses");
    assert!(err.to_string().contains("`voyage`"), "{err}");
}

#[test]
fn a_ladder_without_an_address_is_refused_before_the_first_push() {
    let config = crate::config::types::Embeddings {
        enabled: true,
        provider: "ladder".into(),
        model: "vectors".into(),
        dimensions: 1024,
        api_key_env: "TINYSWEEPER_ABSENT_KEY_5f3a2b1c".into(),
        base_url: String::new(),
        ..crate::config::DEFAULTS
            .parse::<toml::Table>()
            .unwrap()
            .try_into::<crate::config::types::Config>()
            .unwrap()
            .embeddings
    };
    let err = match crate::index::embedder_from_config(&config) {
        Err(err) => err,
        Ok(_) => panic!("a ladder with no address must be refused"),
    };
    assert!(
        err.to_string().contains("needs `embeddings.base_url`"),
        "{err}"
    );
}

/// The live check. Ignored by default: it spends money and needs a key.
///
/// Run with `OPENROUTER_API_KEY=… cargo test --features harness --lib
/// openrouter -- --ignored`.
#[tokio::test]
#[ignore = "calls the OpenRouter API"]
async fn live_embeddings_report_real_usage() {
    let Ok(key) = std::env::var("OPENROUTER_API_KEY") else {
        eprintln!("OPENROUTER_API_KEY unset; skipping");
        return;
    };
    if key.trim().is_empty() {
        return;
    }

    let embedder =
        OpenRouterEmbedder::new(signature(1536), "OPENROUTER_API_KEY", "").expect("builds");
    let embedded = embedder
        .embed(&["fn main() {}".to_string(), "let x = 1;".to_string()])
        .await
        .expect("embeds");

    assert_eq!(embedded.vectors.len(), 2);
    assert_eq!(embedded.vectors[0].len(), 1536);
    assert!(embedded.usage.embed_tokens > 0, "no token count reported");
    assert!(embedded.usage.cost_usd > 0.0, "no cost reported");
}
