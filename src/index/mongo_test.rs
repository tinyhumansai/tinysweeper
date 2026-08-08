//! Tests for the MongoDB retrieval adapter.
//!
//! Split into two halves, deliberately.
//!
//! The **pure** tests always run: they assert on the shape of the pipeline and
//! the documents without a server, which is what keeps `cargo test` offline.
//! The `$rankFusion` pipeline is the piece most likely to be broken by a typo
//! and least likely to be noticed, so it is asserted structurally.
//!
//! The **live** tests need a real MongoDB 8.2+ with `mongot`, named by
//! `TINYSWEEPER_TEST_MONGODB_URI`, and skip without it — the same convention
//! `server::store` uses. Bring one up with:
//!
//! ```sh
//! docker compose -f docker-compose.dev.yml up -d mongod mongot
//! TINYSWEEPER_TEST_MONGODB_URI='mongodb://localhost:27017/?directConnection=true' \
//!     cargo test --features serve -- --ignored index::mongo
//! ```

use super::*;
use crate::index::mock::MockEmbedder;
use crate::ports::embed::Embedder;

/// Environment variable naming the test database, shared with `server::store`.
const TEST_URI_ENV: &str = "TINYSWEEPER_TEST_MONGODB_URI";

fn signature() -> EmbedSignature {
    EmbedSignature::new("mock", "hash-bag", 64)
}

// --- pure: always run, no server -----------------------------------------

#[test]
fn a_vector_is_stored_as_float32_bindata_not_an_array_of_doubles() {
    // Four bytes per dimension against eight plus per-element overhead. On a
    // 1024-dimension index this field is most of the document.
    let binary = encode_vector(&[1.0_f32, -0.5, 0.25]);
    assert_eq!(binary.subtype, bson::spec::BinarySubtype::Vector);
    // Two bytes of header (dtype, padding) plus four per element.
    assert_eq!(binary.bytes.len(), 2 + 3 * 4);
}

#[test]
fn each_signature_gets_its_own_vector_index() {
    // Sharing one index across signatures cannot work: `numDimensions` is part
    // of the definition.
    let a = vector_index_name(&EmbedSignature::new("voyage", "voyage-code-3", 1024));
    let b = vector_index_name(&EmbedSignature::new("voyage", "voyage-3-large", 1024));
    assert_ne!(a, b);
    assert!(a.starts_with("tinysweeper_vec_"));
    assert!(
        a.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'),
        "index names have a restricted character set"
    );
}

#[test]
fn the_vector_definition_declares_both_filter_fields() {
    // `$vectorSearch` refuses to filter on a field the index does not declare,
    // and the signature filter is what makes a model swap invalidate the index.
    let definition = MongoChunkIndex::vector_definition(&signature());
    let fields = definition.get_array("fields").expect("fields");
    let paths: Vec<&str> = fields
        .iter()
        .filter_map(|f| f.as_document()?.get_str("path").ok())
        .collect();
    assert!(paths.contains(&"vector"));
    assert!(paths.contains(&"sig"));
    assert!(paths.contains(&"repo_id"));
    let vector = fields[0].as_document().expect("document");
    assert_eq!(vector.get_i64("numDimensions").expect("dims"), 64);
}

#[test]
fn the_text_definition_does_not_index_the_vector_binary() {
    // `dynamic: true` would index the stored vector: large, and meaningless to
    // BM25.
    let definition = MongoChunkIndex::text_definition();
    let mappings = definition.get_document("mappings").expect("mappings");
    assert!(!mappings.get_bool("dynamic").expect("dynamic"));
    assert!(
        mappings
            .get_document("fields")
            .expect("fields")
            .get("vector")
            .is_none()
    );
}

fn pipeline_for(query: &HybridQuery) -> Vec<Document> {
    // The adapter needs a Collection handle it never dereferences to build a
    // pipeline, so an unconnected client is enough.
    let client = Client::with_options(
        mongodb::options::ClientOptions::builder()
            .hosts(vec![mongodb::options::ServerAddress::Tcp {
                host: "localhost".into(),
                port: Some(27017),
            }])
            .build(),
    )
    .expect("builds");
    MongoChunkIndex::new(&client.database("t"), "code_chunks").pipeline(query)
}

#[tokio::test]
async fn the_pipeline_fuses_two_arms_with_rank_fusion_rather_than_hand_rolled_rrf() {
    let query = HybridQuery::new(signature(), "resolve range", vec![0.0; 64]);
    let pipeline = pipeline_for(&query);

    let fusion = pipeline[0]
        .get_document("$rankFusion")
        .expect("$rankFusion");
    let pipelines = fusion
        .get_document("input")
        .and_then(|i| i.get_document("pipelines"))
        .expect("pipelines");
    assert!(
        pipelines.get_array("dense").expect("dense")[0]
            .as_document()
            .expect("document")
            .contains_key("$vectorSearch")
    );
    assert!(
        pipelines.get_array("lexical").expect("lexical")[0]
            .as_document()
            .expect("document")
            .contains_key("$search"),
        "the lexical arm must be real BM25, not a hand-rolled TF vector"
    );

    let weights = fusion
        .get_document("combination")
        .and_then(|c| c.get_document("weights"))
        .expect("weights");
    assert_eq!(weights.get_f64("dense").expect("dense"), query.dense_weight);
    assert_eq!(
        weights.get_f64("lexical").expect("lexical"),
        query.text_weight
    );
}

#[tokio::test]
async fn every_arm_of_the_pipeline_is_partitioned_by_signature() {
    // If either arm forgot the signature filter, a model swap would leak old
    // vectors into new results — and score them, rather than fail.
    let query = HybridQuery::new(signature(), "anything", vec![0.0; 64]);
    let rendered = format!("{:?}", pipeline_for(&query));
    assert_eq!(
        rendered.matches(&signature().key()).count(),
        2,
        "both the dense filter and the lexical filter must carry the signature"
    );
}

#[tokio::test]
async fn confining_a_query_to_a_repository_filters_both_arms() {
    let query = HybridQuery::new(signature(), "anything", vec![0.0; 64]).in_repo("o/r");
    let rendered = format!("{:?}", pipeline_for(&query));
    assert_eq!(rendered.matches("o/r").count(), 2);
}

#[tokio::test]
async fn the_query_vector_travels_as_bindata_too() {
    let query = HybridQuery::new(signature(), "anything", vec![0.25; 64]);
    let dense = pipeline_for(&query)[0]
        .get_document("$rankFusion")
        .and_then(|f| f.get_document("input"))
        .and_then(|i| i.get_document("pipelines"))
        .and_then(|p| p.get_array("dense"))
        .expect("dense")[0]
        .as_document()
        .expect("document")
        .get_document("$vectorSearch")
        .expect("$vectorSearch")
        .clone();
    assert!(matches!(dense.get("queryVector"), Some(Bson::Binary(_))));
}

#[test]
fn a_chunk_round_trips_through_its_document() {
    let chunk = Chunk {
        repo_id: "o/r".into(),
        path: "src/a.rs".into(),
        start_line: 10,
        end_line: 20,
        text: "fn alpha() {}".into(),
        lang: Some("rust".into()),
        symbol: Some("alpha".into()),
        content_hash: "abc".into(),
    };
    let document = chunk_document(
        &signature(),
        &EmbeddedChunk {
            chunk: chunk.clone(),
            vector: vec![0.0; 64],
        },
    );
    assert_eq!(document.get_str("sig").expect("sig"), signature().key());
    assert_eq!(chunk_from_document(&document), chunk);
}

#[test]
fn a_graph_node_round_trips_through_its_document() {
    let node = GraphNode::symbol("o/r", "src/a.rs", "alpha");
    assert_eq!(node_from_document(&node_document(&node)), node);
    let file = GraphNode::file("o/r", "src/a.rs");
    assert_eq!(
        node_from_document(&node_document(&file)).kind,
        NodeKind::File
    );
}

#[test]
fn a_graph_edge_round_trips_through_its_document() {
    let edge = GraphEdge::new("o/r", "a", "b", EdgeKind::Calls, "src/a.rs");
    assert_eq!(edge_from_document(&edge_document(&edge)), Some(edge));
}

#[test]
fn a_repository_lookup_widens_to_its_organisation_but_not_the_reverse() {
    let repo = MongoKnowledgeStore::visible(&KnowledgeScope::repo("tinyhumansai/x"));
    let rendered = format!("{repo:?}");
    assert!(rendered.contains("tinyhumansai/x"));
    assert!(rendered.contains("tinyhumansai\""), "inherits the org");

    let org = MongoKnowledgeStore::visible(&KnowledgeScope::org("tinyhumansai"));
    assert_eq!(org.get_str("scope.kind").expect("kind"), "org");
    assert!(!format!("{org:?}").contains("repo_id"));
}

#[test]
fn the_boot_probe_never_searches_with_a_zero_vector() {
    // Observed against a real MongoDB 8.2 + mongot: the server rejects the
    // whole aggregation with "Cosine similarity cannot be calculated against a
    // zero vector", so a zero probe fails on a working deployment.
    let vector = probe_vector(64);
    assert_eq!(vector.len(), 64);
    let norm = vector.iter().map(|v| v * v).sum::<f32>().sqrt();
    assert!(
        (norm - 1.0).abs() < 1e-5,
        "the probe vector must be normalised"
    );
}

#[test]
fn a_missing_search_stage_is_reported_as_a_deployment_problem() {
    // The stock `mongo:` image fails exactly this way, and the raw message
    // reads like a bug in tinysweeper rather than in the deployment.
    assert!(is_unsupported_stage(
        "Unrecognized pipeline stage name: '$vectorSearch'"
    ));
    assert!(is_unsupported_stage(
        "$search is only supported on MongoDB Atlas"
    ));
    assert!(!is_unsupported_stage("E11000 duplicate key error"));
    assert!(VECTOR_SEARCH_UNAVAILABLE.contains("8.2"));
    assert!(VECTOR_SEARCH_UNAVAILABLE.contains("mongot"));
}

// --- live: need a real MongoDB 8.2+ with mongot ---------------------------

/// Connect to the test deployment, or skip.
async fn index() -> Option<MongoIndex> {
    let uri = std::env::var(TEST_URI_ENV).ok()?;
    let name = format!("tinysweeper_index_test_{}", std::process::id());
    Some(MongoIndex::connect(&uri, &name).await.expect("connects"))
}

macro_rules! live_test {
    ($name:ident, $body:expr) => {
        // Ignored by default: these need a container, and the offline
        // invariant says `cargo test` must not.
        #[tokio::test]
        #[ignore = "needs a MongoDB 8.2+ deployment with mongot"]
        async fn $name() {
            let Some(index) = index().await else {
                eprintln!("skipping: {} is not set", TEST_URI_ENV);
                return;
            };
            let f: fn(MongoIndex) -> _ = $body;
            f(index).await;
        }
    };
}

live_test!(
    preparing_the_index_proves_vector_search_and_rank_fusion_work,
    |index| async move {
        // This *is* the boot assertion. If it passes against a container, the
        // container is one tinysweeper can run against.
        index.prepare(&signature()).await.expect("prepares");
    }
);

live_test!(
    a_hybrid_query_returns_the_exact_identifier_match,
    |index| async move {
        index.prepare(&signature()).await.expect("prepares");
        let embedder = MockEmbedder::new(64);

        let chunks = [
            ("src/a.rs", "fn resolve_git_range(spec: &str) -> Range {}"),
            ("src/b.rs", "fn something_else_entirely() -> () {}"),
        ];
        let mut rows = Vec::new();
        for (path, text) in chunks {
            rows.push(EmbeddedChunk {
                vector: embedder
                    .embed_query(text)
                    .await
                    .expect("embeds")
                    .into_query_vector()
                    .expect("one vector"),
                chunk: Chunk {
                    repo_id: "o/r".into(),
                    path: path.into(),
                    start_line: 1,
                    end_line: 3,
                    text: text.into(),
                    lang: Some("rust".into()),
                    symbol: None,
                    content_hash: format!("{:x}", text.len()),
                },
            });
        }
        index.code.delete_repo("o/r").await.expect("clears");
        index
            .code
            .upsert(&signature(), &rows)
            .await
            .expect("upserts");

        // mongot indexes asynchronously; poll rather than sleep a fixed amount.
        let mut found = Vec::new();
        for _ in 0..30 {
            let vector = embedder
                .embed_query("resolve_git_range")
                .await
                .expect("embeds")
                .into_query_vector()
                .expect("one vector");
            let query = HybridQuery::new(signature(), "resolve_git_range", vector).in_repo("o/r");
            found = index.code.query(&query).await.expect("queries");
            if !found.is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }
        assert_eq!(found[0].chunk.path, "src/a.rs");
    }
);

live_test!(
    deleting_by_path_is_scoped_to_the_repository,
    |index| async move {
        // Repository ids are unique per test: the live tests share one process and
        // therefore one database, and `--test-threads` is not something a reader of
        // this file controls.
        index.prepare(&signature()).await.expect("prepares");
        let rows: Vec<EmbeddedChunk> = ["o/del", "o/del-other"]
            .iter()
            .map(|repo| EmbeddedChunk {
                vector: vec![0.1; 64],
                chunk: Chunk {
                    repo_id: (*repo).into(),
                    path: "src/a.rs".into(),
                    start_line: 1,
                    end_line: 2,
                    text: "fn alpha() {}".into(),
                    lang: None,
                    symbol: None,
                    content_hash: "h".into(),
                },
            })
            .collect();
        index
            .code
            .upsert(&signature(), &rows)
            .await
            .expect("upserts");
        assert_eq!(
            index
                .code
                .delete_paths("o/del", &["src/a.rs".to_string()])
                .await
                .expect("deletes"),
            1
        );
        assert_eq!(
            index
                .code
                .delete_repo("o/del-other")
                .await
                .expect("deletes"),
            1
        );
    }
);

live_test!(
    a_two_hop_walk_reaches_the_caller_of_a_caller,
    |index| async move {
        index.graph.prepare().await.expect("prepares");
        index.graph.delete_repo("o/g").await.expect("clears");
        index
            .graph
            .upsert_nodes(&[
                GraphNode::file("o/g", "src/a.rs"),
                GraphNode::file("o/g", "src/b.rs"),
                GraphNode::file("o/g", "src/c.rs"),
            ])
            .await
            .expect("writes");
        index
            .graph
            .upsert_edges(&[
                GraphEdge::new("o/g", "src/b.rs", "src/a.rs", EdgeKind::Imports, "src/b.rs"),
                GraphEdge::new("o/g", "src/c.rs", "src/b.rs", EdgeKind::Imports, "src/c.rs"),
            ])
            .await
            .expect("writes");

        let found = index
            .graph
            .neighbours("o/g", &["src/a.rs".to_string()], 2, &EdgeKind::ALL)
            .await
            .expect("walks");
        let paths: Vec<&str> = found.nodes.iter().map(|n| n.path.as_str()).collect();
        assert_eq!(paths, vec!["src/a.rs", "src/b.rs", "src/c.rs"]);
    }
);

live_test!(
    knowledge_documents_widen_from_repo_to_org,
    |index| async move {
        index.knowledge.prepare().await.expect("prepares");
        for id in ["k-org", "k-repo", "k-other"] {
            index.knowledge.delete(id).await.expect("clears");
        }
        index
            .knowledge
            .put(&KnowledgeDoc::new("k-org", KnowledgeScope::org("o"), "T", "B").pinned())
            .await
            .expect("writes");
        index
            .knowledge
            .put(&KnowledgeDoc::new("k-repo", KnowledgeScope::repo("o/r"), "T", "B").pinned())
            .await
            .expect("writes");
        index
            .knowledge
            .put(&KnowledgeDoc::new("k-other", KnowledgeScope::repo("o/x"), "T", "B").pinned())
            .await
            .expect("writes");

        let ids: Vec<String> = index
            .knowledge
            .pinned(&KnowledgeScope::repo("o/r"))
            .await
            .expect("reads")
            .into_iter()
            .map(|d| d.id)
            .collect();
        assert_eq!(ids, vec!["k-org".to_string(), "k-repo".to_string()]);

        assert!(
            index
                .knowledge
                .retrievable(&KnowledgeScope::repo("o/r"))
                .await
                .expect("reads")
                .is_empty()
        );
    }
);
