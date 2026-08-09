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
