//! End-to-end tests for the retrieval pipeline, against the offline mocks.
//!
//! Two of these are the workstream's acceptance criteria: retrieval must
//! actually add the caller a change reaches, and a cold index must produce a
//! diff-only review that *says* it is one.

use super::*;
use async_trait::async_trait;

use std::collections::BTreeSet;

use crate::error::Error;
use crate::evidence::diff::parse_file_patch;
use crate::index::types::{
    Chunk, EdgeKind, Embedded, EmbeddedChunk, GraphEdge, GraphNode, ScoredChunk,
};
use crate::index::{MockChunkIndex, MockEmbedder, MockGraphStore};
use crate::indexer::mock::MockManifest;
use crate::indexer::types::{Claim, Settled};
use crate::ports::embed::Embedder;
use crate::ports::index::ChunkIndex;
use crate::ports::manifest::IndexManifest;

const REPO: &str = "acme/app";
const HEAD: &str = "headsha0000000000000000000000000000";

fn config() -> Config {
    crate::config::DEFAULTS
        .parse::<toml::Table>()
        .unwrap()
        .try_into()
        .unwrap()
}

fn embedder() -> MockEmbedder {
    MockEmbedder::new(32)
}

async fn index_with(chunks: &[(&str, u32, u32, &str)]) -> MockChunkIndex {
    let index = MockChunkIndex::new();
    let embedder = embedder();
    let mut embedded = Vec::new();
    for (path, start, end, text) in chunks {
        let chunk = Chunk {
            repo_id: REPO.into(),
            path: (*path).into(),
            start_line: *start,
            end_line: *end,
            text: (*text).into(),
            content_hash: format!("{path}:{start}"),
            ..Chunk::default()
        };
        let vector = embedder
            .embed(std::slice::from_ref(&chunk.text))
            .await
            .expect("embeds")
            .vectors
            .remove(0);
        embedded.push(EmbeddedChunk { chunk, vector });
    }
    index
        .upsert(&embedder.signature(), &embedded)
        .await
        .expect("upserts");
    index
}

/// A manifest reporting a repository indexed at `revision`, with `chunks` rows.
async fn manifest_at(revision: Option<&str>, chunks: u64) -> MockManifest {
    let manifest = MockManifest::new();
    let signature = embedder().signature();
    let Claim::Granted(lease) = manifest
        .claim(REPO, &signature, "test")
        .await
        .expect("claims")
    else {
        panic!("the claim must be free");
    };
    manifest
        .release(
            &lease,
            &Settled::Done {
                revision: revision.map(str::to_string),
                chunks,
                usage: Default::default(),
            },
        )
        .await
        .expect("releases");
    manifest
}

fn callee_diff() -> Vec<FileDiff> {
    vec![parse_file_patch(
        "src/billing.rs",
        "@@ -10,3 +10,4 @@ pub fn settle(order: &Order) -> Result<()>\n ctx\n+    charge(order)?;\n more\n",
    )]
}

async fn caller_graph() -> MockGraphStore {
    let graph = MockGraphStore::new();
    graph
        .upsert_nodes(&[
            GraphNode::file(REPO, "src/billing.rs"),
            GraphNode::file(REPO, "src/caller.rs"),
            GraphNode::symbol(REPO, "src/billing.rs", "settle"),
            GraphNode::symbol(REPO, "src/caller.rs", "checkout"),
        ])
        .await
        .expect("nodes");
    graph
        .upsert_edges(&[
            GraphEdge::new(
                REPO,
                "src/caller.rs#checkout",
                "src/billing.rs#settle",
                EdgeKind::Calls,
                "src/caller.rs",
            ),
            GraphEdge::new(
                REPO,
                "src/caller.rs",
                "src/caller.rs#checkout",
                EdgeKind::Defines,
                "src/caller.rs",
            ),
        ])
        .await
        .expect("edges");
    graph
}

#[tokio::test]
async fn graph_expansion_adds_a_caller_that_similarity_alone_would_miss() {
    // The setup is the whole point. `src/caller.rs` shares no vocabulary with
    // the diff and is buried behind thirty chunks that do, so it is outside the
    // search arm's limit and no weighting would bring it back. The graph is the
    // only route to it. Running retrieval with and without the graph attached,
    // over the same index, is the measurement.
    let decoys: Vec<(String, u32, u32, String)> = (0..30)
        .map(|index| {
            (
                format!("src/similar{index:02}.rs"),
                1,
                8,
                "pub fn settle(order: &Order) -> Result<()> { charge(order) }".to_string(),
            )
        })
        .collect();
    let mut rows: Vec<(&str, u32, u32, &str)> = decoys
        .iter()
        .map(|(path, start, end, text)| (path.as_str(), *start, *end, text.as_str()))
        .collect();
    rows.push((
        "src/caller.rs",
        1,
        6,
        "fn checkout(basket: &Basket) { dispatch(basket); }",
    ));
    let index = index_with(&rows).await;
    let graph = caller_graph().await;
    let embedder = embedder();
    let manifest = manifest_at(Some(HEAD), rows.len() as u64).await;
    let mut config = config();
    // Small enough that the search arm's over-fetch cannot reach the caller.
    config.retrieval.max_chunks = 6;

    let without = Retriever::new(&embedder, &index)
        .with_manifest(&manifest)
        .retrieve(&config, REPO, "Charge on settle", HEAD, &callee_diff())
        .await
        .0;
    let with = Retriever::new(&embedder, &index)
        .with_graph(&graph)
        .with_manifest(&manifest)
        .retrieve(&config, REPO, "Charge on settle", HEAD, &callee_diff())
        .await
        .0;

    assert!(
        !without.render().contains("src/caller.rs"),
        "similarity alone must not find the caller, or the test proves nothing"
    );

    let (_, graph_hits) = with.counts();
    assert!(graph_hits > 0, "graph expansion contributed nothing");
    assert!(
        with.render().contains("src/caller.rs"),
        "the caller must reach the prompt: {}",
        with.render()
    );
    assert!(with.graph_nodes > 0);
}

#[tokio::test]
async fn a_cold_index_degrades_to_a_diff_only_review_and_says_so() {
    let index = MockChunkIndex::new();
    let embedder = embedder();
    // Never claimed, never released: the repository has no record at all.
    let manifest = MockManifest::new();

    let (context, spend) = Retriever::new(&embedder, &index)
        .with_manifest(&manifest)
        .retrieve(&config(), REPO, "Anything", HEAD, &callee_diff())
        .await;

    assert_eq!(context.status, RetrievalStatus::Cold);
    assert!(context.render().is_empty());
    assert!(
        context
            .note()
            .expect("the summary says so")
            .contains("cold"),
        "{:?}",
        context.note()
    );
    // A cold index cannot be warmed by querying it, so nothing is spent.
    assert_eq!(spend.usage.embed_tokens, 0);
}

#[tokio::test]
async fn an_index_recorded_with_no_chunks_is_cold_rather_than_ready() {
    // `Ready` with zero rows is what a first indexing run that selected no
    // files leaves behind. Querying it returns nothing, so calling it fresh
    // would be a claim the review cannot support.
    let index = MockChunkIndex::new();
    let embedder = embedder();
    let manifest = manifest_at(Some(HEAD), 0).await;

    let (context, _) = Retriever::new(&embedder, &index)
        .with_manifest(&manifest)
        .retrieve(&config(), REPO, "Anything", HEAD, &callee_diff())
        .await;

    assert_eq!(context.status, RetrievalStatus::Cold);
}

#[tokio::test]
async fn an_index_behind_the_head_commit_still_retrieves_but_admits_it_is_behind() {
    let index = index_with(&[("src/caller.rs", 1, 6, "fn checkout() { settle(); }")]).await;
    let embedder = embedder();
    let manifest = manifest_at(Some("olderrevision0000000"), 1).await;

    let (context, _) = Retriever::new(&embedder, &index)
        .with_manifest(&manifest)
        .retrieve(&config(), REPO, "Charge on settle", HEAD, &callee_diff())
        .await;

    assert_eq!(
        context.status,
        RetrievalStatus::Stale {
            indexed: Some("olderrevision0000000".into())
        }
    );
    assert!(!context.chunks.is_empty(), "a stale index is still useful");
    assert!(context.note().expect("says so").contains("behind"));
}

#[tokio::test]
async fn no_manifest_means_no_freshness_claim_rather_than_a_false_alarm() {
    // A missing record is not evidence of staleness. Reporting one anyway
    // trains an operator to ignore the notice.
    let index = index_with(&[("src/caller.rs", 1, 6, "fn checkout() { settle(); }")]).await;
    let embedder = embedder();

    let (context, _) = Retriever::new(&embedder, &index)
        .retrieve(&config(), REPO, "Charge on settle", HEAD, &callee_diff())
        .await;

    assert_eq!(context.status, RetrievalStatus::Ready);
}

/// A chunk index whose hybrid query fails the way a missing `mongot` does.
#[derive(Debug, Default)]
struct SearchlessIndex;

#[async_trait]
impl ChunkIndex for SearchlessIndex {
    async fn prepare(&self, _signature: &EmbedSignature) -> crate::error::Result<()> {
        Ok(())
    }
    async fn upsert(
        &self,
        _signature: &EmbedSignature,
        _chunks: &[EmbeddedChunk],
    ) -> crate::error::Result<u64> {
        Ok(0)
    }
    async fn relocate(
        &self,
        _signature: &EmbedSignature,
        _repo_id: &str,
        _chunks: &[(String, Chunk)],
    ) -> crate::error::Result<u64> {
        Ok(0)
    }
    async fn delete_repo(&self, _repo_id: &str) -> crate::error::Result<u64> {
        Ok(0)
    }
    async fn delete_paths(&self, _repo_id: &str, _paths: &[String]) -> crate::error::Result<u64> {
        Ok(0)
    }
    async fn delete_chunks(&self, _repo_id: &str, _ids: &[String]) -> crate::error::Result<u64> {
        Ok(0)
    }
    async fn query(&self, _query: &HybridQuery) -> crate::error::Result<Vec<ScoredChunk>> {
        Err(Error::Forge(
            "this MongoDB deployment cannot serve $vectorSearch/$search".into(),
        ))
    }
    async fn chunks_in_paths(
        &self,
        _signature: &EmbedSignature,
        _repo_id: &str,
        _paths: &[String],
        _limit: usize,
    ) -> crate::error::Result<Vec<Chunk>> {
        Ok(Vec::new())
    }
}

#[tokio::test]
async fn a_deployment_without_hybrid_search_reviews_the_diff_and_names_the_problem() {
    let embedder = embedder();
    let index = SearchlessIndex;

    let (context, spend) = Retriever::new(&embedder, &index)
        .retrieve(&config(), REPO, "Charge on settle", HEAD, &callee_diff())
        .await;

    let note = context.note().expect("says so");
    assert!(note.contains("unavailable"), "{note}");
    assert!(note.contains("$vectorSearch"), "{note}");
    assert!(context.status.is_degraded());
    // The embedding call happened before the query failed, so it is billed.
    assert!(spend.usage.embed_tokens > 0);
}

/// An embedder that refuses, the way a provider outage does.
#[derive(Debug, Default)]
struct BrokenEmbedder;

#[async_trait]
impl Embedder for BrokenEmbedder {
    fn signature(&self) -> EmbedSignature {
        EmbedSignature::new("mock", "broken", 8)
    }
    async fn embed(&self, _texts: &[String]) -> crate::error::Result<Embedded> {
        Err(Error::Model("the embedding provider is down".into()))
    }
}

#[tokio::test]
async fn an_embedder_outage_costs_context_not_the_review() {
    let index = MockChunkIndex::new();
    let (context, spend) = Retriever::new(&BrokenEmbedder, &index)
        .retrieve(&config(), REPO, "Anything", HEAD, &callee_diff())
        .await;

    assert!(context.status.is_degraded());
    assert!(context.note().expect("says so").contains("down"));
    assert_eq!(spend.usage.cost_usd, 0.0, "a failed call bought nothing");
}

#[tokio::test]
async fn embedding_the_query_is_billed_through_the_pricing_table() {
    // Retrieval that could produce context without a price would be a hole in
    // the same budget the indexer already closed.
    let index = index_with(&[("src/caller.rs", 1, 6, "fn checkout() {}")]).await;
    let signature = EmbedSignature::new("voyage", "voyage-code-3", 32);
    let embedder = MockEmbedder::with_signature(signature.clone());

    let (_, spend) = Retriever::new(&embedder, &index)
        .retrieve(&config(), REPO, "Charge on settle", HEAD, &callee_diff())
        .await;

    assert!(spend.usage.embed_tokens > 0);
    assert!(spend.usage.cost_usd > 0.0, "a priced embedder must charge");
    assert_eq!(
        spend.usage.input_tokens, 0,
        "embeddings are not prompt tokens"
    );
    assert!(spend.models.contains(&signature.price_key()));
}

#[tokio::test]
async fn turning_retrieval_off_spends_nothing_and_admits_nothing() {
    let index = index_with(&[("src/caller.rs", 1, 6, "fn checkout() {}")]).await;
    let embedder = embedder();
    let mut config = config();
    config.retrieval.enabled = false;

    let (context, spend) = Retriever::new(&embedder, &index)
        .retrieve(&config, REPO, "Charge on settle", HEAD, &callee_diff())
        .await;

    assert_eq!(context.status, RetrievalStatus::Off);
    // An operator who turned retrieval off does not need telling on every
    // review that it is off.
    assert_eq!(context.note(), None);
    assert_eq!(spend.usage.embed_tokens, 0);
}

#[tokio::test]
async fn the_context_handed_to_a_lane_stays_inside_the_token_budget() {
    let chunks: Vec<(String, u32, u32, String)> = (0..200)
        .map(|index| {
            (
                format!("src/file{index:03}.rs"),
                1,
                60,
                format!(
                    "fn settle_{index}(order: &Order) {{ {} }}",
                    "work(); ".repeat(60)
                ),
            )
        })
        .collect();
    let borrowed: Vec<(&str, u32, u32, &str)> = chunks
        .iter()
        .map(|(path, start, end, text)| (path.as_str(), *start, *end, text.as_str()))
        .collect();
    let index = index_with(&borrowed).await;
    let embedder = embedder();
    let mut config = config();
    config.retrieval.context_tokens = 500;

    let (context, _) = Retriever::new(&embedder, &index)
        .retrieve(&config, REPO, "Charge on settle", HEAD, &callee_diff())
        .await;

    assert!(context.tokens <= 500, "{}", context.tokens);
    assert!(!context.chunks.is_empty());
    assert!(context.dropped > 0, "what was dropped must be reported");
}

#[tokio::test]
async fn a_diff_with_nothing_to_say_skips_the_embedding_call() {
    let index = index_with(&[("src/caller.rs", 1, 6, "fn checkout() {}")]).await;
    let embedder = embedder();

    let (context, spend) = Retriever::new(&embedder, &index)
        .retrieve(&config(), REPO, "", HEAD, &[])
        .await;

    assert!(context.is_empty());
    assert_eq!(spend.usage.embed_tokens, 0);
}

/// This crate's own `src/` tree, as source files.
///
/// The same reader `graph::build_test` uses. A retrieval pipeline is easy to
/// make look good on three hand-written fixtures; the question that matters is
/// whether it adds anything on a repository nobody wrote for it.
fn this_crate() -> Vec<crate::graph::SourceFile> {
    fn walk(
        dir: &std::path::Path,
        root: &std::path::Path,
        out: &mut Vec<crate::graph::SourceFile>,
    ) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, root, out);
            } else if let Ok(text) = std::fs::read_to_string(&path) {
                let relative = path
                    .strip_prefix(root)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .replace('\\', "/");
                out.push(crate::graph::SourceFile::new(relative, text));
            }
        }
    }

    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files = Vec::new();
    walk(&root.join("src"), root, &mut files);
    if let Ok(text) = std::fs::read_to_string(root.join("Cargo.toml")) {
        files.push(crate::graph::SourceFile::new("Cargo.toml", text));
    }
    files
}

#[tokio::test]
async fn graph_expansion_adds_context_on_this_repositorys_own_code() {
    // The honesty check on the whole workstream. Everything else here runs
    // against fixtures built to make the graph win. This indexes and graphs
    // *this crate*, takes a diff against a real file, and asks whether the
    // graph reaches source files the hybrid query did not — which is the only
    // question that decides whether the expansion step earns its keep.
    let files = this_crate();
    assert!(
        files.len() > 50,
        "expected a real tree, got {}",
        files.len()
    );

    let embedder = embedder();
    let index = MockChunkIndex::new();
    let chunker = crate::chunk::Chunker::new();
    let mut rows = Vec::new();
    for file in &files {
        for chunk in chunker.chunk(REPO, &file.path, &file.text) {
            let vector = embedder
                .embed(std::slice::from_ref(&chunk.text))
                .await
                .expect("embeds")
                .vectors
                .remove(0);
            rows.push(EmbeddedChunk { chunk, vector });
        }
    }
    assert!(
        rows.len() > 200,
        "expected a real index, got {}",
        rows.len()
    );
    index
        .upsert(&embedder.signature(), &rows)
        .await
        .expect("upserts");

    let graph_store = MockGraphStore::new();
    let built = crate::graph::build(REPO, &files).expect("builds");
    crate::graph::sync_all(&graph_store, REPO, &built)
        .await
        .expect("writes the graph");

    // A real, plausible change: adjust how an embedding call is priced.
    let diffs = vec![parse_file_patch(
        "src/harness/pricing.rs",
        "@@ -181,6 +181,7 @@ pub fn embedding_cost(signature: &str, tokens: u64) -> f64\n \
         let per_million = EMBED_PRICES\n+        .iter()\n         .iter()\n",
    )];

    let mut config = config();
    config.retrieval.max_chunks = 12;

    let without = Retriever::new(&embedder, &index)
        .retrieve(&config, REPO, "Reprice embedding calls", HEAD, &diffs)
        .await
        .0;
    let with = Retriever::new(&embedder, &index)
        .with_graph(&graph_store)
        .retrieve(&config, REPO, "Reprice embedding calls", HEAD, &diffs)
        .await
        .0;

    let (_, graph_hits) = with.counts();
    assert!(
        graph_hits > 0,
        "graph expansion contributed nothing on real code"
    );

    let searched: BTreeSet<&str> = without
        .chunks
        .iter()
        .map(|c| c.chunk.path.as_str())
        .collect();
    let added: BTreeSet<&str> = with
        .chunks
        .iter()
        .filter(|c| c.provenance == Provenance::Graph)
        .map(|c| c.chunk.path.as_str())
        .filter(|path| !searched.contains(path))
        .collect();
    assert!(
        !added.is_empty(),
        "the graph reached only files the search arm already had: {:?}",
        with.chunks
            .iter()
            .map(|c| (&c.chunk.path, c.provenance))
            .collect::<Vec<_>>()
    );

    assert!(with.tokens <= config.retrieval.context_tokens);
    assert!(with.graph_nodes > 0);
}

#[tokio::test]
async fn the_blast_radius_of_a_real_change_names_its_dependents_and_its_coverage() {
    // The acceptance test for the impact step, and deliberately on this
    // repository rather than a fixture: a fixture built to make the blast
    // radius non-empty proves only that the fixture was built that way.
    let files = this_crate();
    let graph_store = MockGraphStore::new();
    let built = crate::graph::build(REPO, &files).expect("builds");
    crate::graph::sync_all(&graph_store, REPO, &built)
        .await
        .expect("writes the graph");

    let embedder = embedder();
    let index = MockChunkIndex::new();

    // `graph::path::normalise` is called from other modules and exercised by
    // its own module's tests, so both halves of the answer should be
    // non-empty. It is also named only once in the crate — a symbol defined in
    // two files is deliberately left ambiguous by the builder, and picking one
    // would have tested the resolver's refusal to guess rather than this.
    let diffs = vec![parse_file_patch(
        "src/graph/path.rs",
        "@@ -14,4 +14,4 @@ pub fn normalise(path: &str) -> String\n-    let mut segments: Vec<&str> = Vec::new();\n+    let mut segments: Vec<&str> = Vec::with_capacity(8);\n",
    )];

    let context = Retriever::new(&embedder, &index)
        .with_graph(&graph_store)
        .retrieve(&config(), REPO, "Preallocate the segment buffer", HEAD, &diffs)
        .await
        .0;

    let callers: Vec<&str> = context
        .impact
        .reached
        .iter()
        .filter(|entry| entry.relation == crate::graph::Relation::Caller)
        .map(|entry| entry.id.as_str())
        .collect();
    assert!(
        !callers.is_empty(),
        "nothing calls a function the whole crate calls: {:?}",
        context.impact
    );
    assert!(
        context
            .impact
            .reached
            .iter()
            .any(|entry| entry.relation == crate::graph::Relation::Test),
        "no test covers a function its own module tests: {:?}",
        context.impact
    );

    // And it has to reach the prompt, not merely the struct: this whole step
    // is worthless if a lane never sees it.
    let rendered = context.render();
    assert!(rendered.contains("blast radius"), "{rendered}");
    assert!(rendered.contains(callers[0]), "{rendered}");
}

#[tokio::test]
async fn a_review_with_no_graph_claims_no_blast_radius() {
    // Retrieval without a graph must produce an *empty* impact rather than an
    // empty-looking one: "nothing depends on this change" and "we did not
    // look" are different sentences, and only one of them is safe to render.
    let index = index_with(&[("src/lib/math.ts", 1, 3, "export function total() {}")]).await;
    let diffs = vec![parse_file_patch(
        "src/lib/math.ts",
        "@@ -1,2 +1,2 @@ export function total()\n-  return 1;\n+  return 2;\n",
    )];

    let context = Retriever::new(&embedder(), &index)
        .retrieve(&config(), REPO, "Change the total", HEAD, &diffs)
        .await
        .0;

    assert!(context.impact.is_empty());
    assert!(!context.render().contains("blast radius"));
}
