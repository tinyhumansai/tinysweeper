//! Index a checkout with a real embedder and run one retrieval against it.
//!
//! Declared with `required-features = ["serve"]` so it never builds in CI: it
//! needs an embedding provider and a MongoDB 8.2+ deployment with `mongot`, and
//! the default `cargo test` must touch neither.
//!
//! This is the smoke test for the seam W11 added. Everything under
//! `src/index/`, `src/indexer/`, `src/graph/` and `src/retrieve/` is covered
//! offline against mocks; what a mock cannot tell you is whether a real
//! provider returns vectors of the width it advertises, and whether MongoDB
//! accepts them into a `$rankFusion` query. Those two are the whole point of
//! running this.
//!
//! ```sh
//! docker compose up -d mongot
//! export TINYSWEEPER_MONGODB_URI='mongodb://…'
//! cargo run --features serve --example index_and_retrieve -- . "hybrid search"
//! ```
//!
//! The provider comes from `[embeddings]` in the configuration, exactly as the
//! server reads it, so a run that works here is a deployment that will work.

use std::path::Path;

use tinysweeper::error::{Error, Result};
use tinysweeper::index::mongo::MongoIndex;
use tinysweeper::index::provider::ProviderEmbedder;
use tinysweeper::index::types::HybridQuery;
use tinysweeper::indexer::mongo::MongoManifest;
use tinysweeper::indexer::run::Indexer;
use tinysweeper::ports::embed::Embedder;
use tinysweeper::ports::index::ChunkIndex;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "tinysweeper=info".into()),
        )
        .init();

    let mut args = std::env::args().skip(1);
    let root = args.next().unwrap_or_else(|| ".".to_string());
    let query = args
        .next()
        .unwrap_or_else(|| "the embedding signature is a partition key".to_string());

    let loaded = tinysweeper::config::load_validated(Path::new(&root), None)?;
    let config = loaded.config;

    let embedder = ProviderEmbedder::from_config(&config.embeddings)?.ok_or_else(|| {
        Error::config(
            "`[embeddings]` is disabled; set `enabled = true` in the checkout's \
             .tinysweeper.toml to run this example",
        )
    })?;
    let signature = embedder.signature();
    println!("embedding under {signature}");

    let index = MongoIndex::from_env().await?;
    index.prepare(&signature).await?;

    let uri = std::env::var("TINYSWEEPER_MONGODB_URI")
        .map_err(|_| Error::config("TINYSWEEPER_MONGODB_URI is not set"))?;
    let database =
        std::env::var("TINYSWEEPER_MONGODB_DB").unwrap_or_else(|_| "tinysweeper".to_string());
    let manifest = MongoManifest::connect(&uri, &database).await?;

    let repo_id = "local/checkout";
    // A synthetic revision: this example indexes a working tree, which has no
    // commit of its own until it is committed. It changes every run, which is
    // what a smoke test wants — the freshness short-circuit would otherwise
    // make the second run prove nothing.
    let revision = format!(
        "{:040x}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|since| since.as_nanos())
            .unwrap_or_default()
    );

    let outcome = Indexer::new(&embedder, &index.code, &manifest)?
        .with_selector(tinysweeper::chunk::Selector::new(&config.paths.ignore)?)
        .with_batch(config.embeddings.batch)
        .with_budget(config.embeddings.budget_usd_per_index)
        .as_holder("example")
        .index_repo(repo_id, &revision, Path::new(&root))
        .await?;
    println!("{outcome:?}");

    let embedded = embedder.embed_query(&query).await?;
    let spent = embedded.usage.cost_usd;
    let hits = index
        .code
        .query(
            &HybridQuery::new(signature, &query, embedded.into_query_vector()?)
                .in_repo(repo_id)
                .limit(5),
        )
        .await?;

    println!("\n{} hit(s) for {query:?} (query cost ${spent:.6})", hits.len());
    for hit in &hits {
        println!(
            "  {:.4}  {}:{}-{}  {}",
            hit.score,
            hit.chunk.path,
            hit.chunk.start_line,
            hit.chunk.end_line,
            hit.chunk.symbol.as_deref().unwrap_or("-")
        );
    }
    if hits.is_empty() {
        return Err(Error::config(
            "the index answered with no hits; hybrid search is not working end to end",
        ));
    }
    Ok(())
}
