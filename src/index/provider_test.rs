//! Behaviour tests for the provider-backed embedder. Requires `harness`.
//!
//! Offline throughout. Every test here runs against tinyagents'
//! `MockEmbeddingModel`, which hashes its input and makes no request, so
//! `cargo test --features harness` still touches no network. The one thing
//! these tests cannot cover offline — that a live provider returns what it says
//! it will — is covered by the `#[ignore]`d test at the bottom, which skips
//! itself when its key is absent.

use super::*;

fn config(provider: &str, model: &str, dims: usize) -> Embeddings {
    Embeddings {
        enabled: true,
        provider: provider.into(),
        model: model.into(),
        dimensions: dims,
        api_key_env: "TINYSWEEPER_TEST_KEY_ABSENT".into(),
        base_url: String::new(),
        batch: 8,
        max_request_tokens: 0,
        requests_per_minute: 0,
        budget_usd_per_index: 1.0,
    }
}

#[test]
fn our_signature_and_the_harness_signature_describe_the_same_space() {
    // The correspondence the index partition depends on. tinyagents writes
    // `provider=…;model=…;dims=…`; if `EmbedSignature::harness_key` ever drifts
    // from it, the two sides are naming spaces by different rules and a model
    // swap stops being detectable.
    let model = MockEmbeddingModel::new(16);
    let signature = EmbedSignature::new(model.name(), model.model_id(), model.dimensions());
    assert_eq!(signature.harness_key(), model.signature());
}

#[test]
fn a_signature_that_does_not_match_the_model_is_refused_rather_than_used() {
    // The failure this prevents is silent: 1024 declared against a 1536-wide
    // model indexes vectors the search index will reject, and a *renamed* model
    // at the same width would be accepted forever and answer from a different
    // embedding space. Refusing at construction turns both into a startup
    // error with the two keys side by side.
    let model = Arc::new(MockEmbeddingModel::new(16));
    let wrong_width = EmbedSignature::new(model.name(), model.model_id(), 32);
    let err = ProviderEmbedder::new(model.clone(), wrong_width)
        .expect_err("a width mismatch must not construct")
        .to_string();
    assert!(err.contains("mock:deterministic-hash:16"), "{err}");
    assert!(err.contains("mock:deterministic-hash:32"), "{err}");

    let wrong_model = EmbedSignature::new(model.name(), "some-other-model", 16);
    assert!(
        ProviderEmbedder::new(model.clone(), wrong_model).is_err(),
        "a model rename at the same width must not construct either"
    );

    let right = EmbedSignature::new(model.name(), model.model_id(), model.dimensions());
    assert!(ProviderEmbedder::new(model, right).is_ok());
}

#[tokio::test]
async fn embedding_reports_one_priced_vector_per_input() {
    let model = Arc::new(MockEmbeddingModel::new(16));
    let signature = EmbedSignature::new(model.name(), model.model_id(), model.dimensions());
    let embedder = ProviderEmbedder::new(model, signature.clone()).expect("constructs");

    let texts = vec!["fn main() {}".to_string(), "struct Chunk;".to_string()];
    let embedded = embedder.embed(&texts).await.expect("embeds");
    assert_eq!(embedded.vectors.len(), 2);
    assert!(embedded.vectors.iter().all(|v| v.len() == 16));
    // Unpriced by the table, so charged at the ceiling rather than excused —
    // an embedder that could produce vectors for free is a hole in the budget.
    assert!(embedded.usage.embed_tokens > 0);
    assert!(embedded.usage.cost_usd > 0.0);
    assert_eq!(embedded.usage.input_tokens, 0, "embeddings are not prompts");

    let query = embedder.embed_query("fn main").await.expect("embeds");
    assert_eq!(query.vectors.len(), 1);
    assert_eq!(embedder.signature(), signature);
}

#[tokio::test]
async fn an_empty_batch_costs_nothing_and_makes_no_call() {
    let model = Arc::new(MockEmbeddingModel::new(8));
    let signature = EmbedSignature::new(model.name(), model.model_id(), 8);
    let embedder = ProviderEmbedder::new(model, signature).expect("constructs");
    let embedded = embedder.embed(&[]).await.expect("embeds nothing");
    assert!(embedded.vectors.is_empty());
    assert_eq!(embedded.usage.cost_usd, 0.0);
}

#[test]
fn a_disabled_section_yields_no_embedder_rather_than_an_error() {
    let mut disabled = config("voyage", "voyage-code-3", 1024);
    disabled.enabled = false;
    assert!(
        ProviderEmbedder::from_config(&disabled)
            .expect("a disabled section is a choice, not a mistake")
            .is_none()
    );
}

#[test]
fn a_missing_key_names_the_variable_rather_than_suggesting_a_file() {
    let err = ProviderEmbedder::from_config(&config("voyage", "voyage-code-3", 1024))
        .expect_err("a provider with no key cannot be built")
        .to_string();
    assert!(err.contains("TINYSWEEPER_TEST_KEY_ABSENT"), "{err}");
    assert!(err.contains("voyage"), "{err}");
}

#[test]
fn an_unknown_provider_lists_the_ones_that_exist() {
    let err = ProviderEmbedder::from_config(&config("acme", "acme-embed", 256))
        .expect_err("an unknown provider is a config error")
        .to_string();
    assert!(err.contains("voyage"), "{err}");
    assert!(err.contains("ollama"), "{err}");
}

#[test]
fn the_offline_provider_builds_without_a_key_and_agrees_with_itself() {
    let embedder = ProviderEmbedder::from_config(&config("mock", "deterministic-hash", 32))
        .expect("builds")
        .expect("enabled");
    assert_eq!(embedder.signature().dims, 32);
    assert_eq!(
        embedder.signature().harness_key(),
        embedder.model().signature()
    );
}

#[test]
fn debug_never_prints_a_key() {
    let embedder = ProviderEmbedder::from_config(&config("mock", "deterministic-hash", 4))
        .expect("builds")
        .expect("enabled");
    let rendered = format!("{embedder:?}");
    assert!(rendered.contains("mock:deterministic-hash:4"), "{rendered}");
    assert!(!rendered.contains("api_key"), "{rendered}");
}

/// A real call against a real provider.
///
/// `#[ignore]`d and additionally skipped when the key is absent, matching
/// `crate::index::mongo_test`: the default `cargo test` must never touch the
/// network, and a live test that fails for a missing credential is noise rather
/// than a signal. Run it with
/// `VOYAGE_API_KEY=… cargo test --features harness -- --ignored live_`.
#[tokio::test]
#[ignore = "calls a real embedding provider"]
async fn live_voyage_returns_vectors_of_the_declared_width() {
    let Ok(key) = std::env::var("VOYAGE_API_KEY") else {
        eprintln!("VOYAGE_API_KEY is not set; skipping");
        return;
    };
    if key.trim().is_empty() {
        eprintln!("VOYAGE_API_KEY is empty; skipping");
        return;
    }

    let mut live = config("voyage", "voyage-code-3", 1024);
    live.api_key_env = "VOYAGE_API_KEY".into();
    let embedder = ProviderEmbedder::from_config(&live)
        .expect("builds")
        .expect("enabled");

    let embedded = embedder
        .embed(&["fn main() { println!(\"hi\"); }".to_string()])
        .await
        .expect("embeds");
    assert_eq!(embedded.vectors.len(), 1);
    assert_eq!(embedded.vectors[0].len(), 1024);
    assert!(embedded.usage.cost_usd > 0.0);
}
