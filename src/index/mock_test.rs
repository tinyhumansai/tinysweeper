//! Behaviour tests for the offline retrieval mocks.
//!
//! Split out of `mock.rs` because they cover four ports and had outgrown a
//! trailing block. These are the tests that keep the *destructive* half of the
//! index honest — incremental re-index is where a retrieval layer rots, and it
//! rots silently.

use super::*;

fn chunk(repo: &str, path: &str, line: u32, text: &str) -> Chunk {
    Chunk {
        repo_id: repo.into(),
        path: path.into(),
        start_line: line,
        end_line: line + 4,
        text: text.into(),
        lang: Some("rust".into()),
        symbol: None,
        content_hash: format!("{:x}", text.len()),
        chunked_by: crate::index::types::ChunkMethod::Parsed,
    }
}

async fn embedded(embedder: &MockEmbedder, chunks: Vec<Chunk>) -> Vec<EmbeddedChunk> {
    let texts: Vec<String> = chunks.iter().map(|c| c.text.clone()).collect();
    let vectors = embedder.embed(&texts).await.expect("embeds");
    chunks
        .into_iter()
        .zip(vectors)
        .map(|(chunk, vector)| EmbeddedChunk { chunk, vector })
        .collect()
}

#[tokio::test]
async fn the_embedder_is_deterministic_across_calls() {
    // Golden retrieval tests assert on an exact ordering; that only works if
    // the same text embeds identically every run, on every platform.
    let embedder = MockEmbedder::new(32);
    let once = embedder
        .embed_query("fn parse_config")
        .await
        .expect("embeds");
    let twice = embedder
        .embed_query("fn parse_config")
        .await
        .expect("embeds");
    assert_eq!(once, twice);
    assert_eq!(once.len(), 32);
}

#[tokio::test]
async fn embedding_a_batch_returns_one_vector_per_input_in_order() {
    let embedder = MockEmbedder::new(16);
    let texts = vec!["alpha".to_string(), "beta".to_string(), "alpha".to_string()];
    let vectors = embedder.embed(&texts).await.expect("embeds");
    assert_eq!(vectors.len(), 3);
    assert_eq!(vectors[0], vectors[2]);
    assert_ne!(vectors[0], vectors[1]);
}

#[tokio::test]
async fn a_signature_change_hides_previously_indexed_rows() {
    // The whole reason the signature is a partition key: swapping the embedding
    // model must invalidate the index rather than silently mix vector spaces.
    let index = MockChunkIndex::new();
    let old = MockEmbedder::with_signature(EmbedSignature::new("mock", "v1", 8));
    let new = MockEmbedder::with_signature(EmbedSignature::new("mock", "v2", 8));

    let rows = embedded(&old, vec![chunk("o/r", "src/a.rs", 1, "fn alpha() {}")]).await;
    index
        .upsert(&old.signature(), &rows)
        .await
        .expect("upserts");

    let found = index
        .query(&HybridQuery::new(
            new.signature(),
            "alpha",
            new.embed_query("alpha").await.expect("embeds"),
        ))
        .await
        .expect("queries");
    assert!(found.is_empty(), "v2 must not see v1's vectors");

    let found = index
        .query(&HybridQuery::new(
            old.signature(),
            "alpha",
            old.embed_query("alpha").await.expect("embeds"),
        ))
        .await
        .expect("queries");
    assert_eq!(found.len(), 1);
}

#[tokio::test]
async fn upserting_the_same_span_twice_is_one_row() {
    let index = MockChunkIndex::new();
    let embedder = MockEmbedder::new(8);
    let rows = embedded(
        &embedder,
        vec![chunk("o/r", "src/a.rs", 1, "fn alpha() {}")],
    )
    .await;
    index
        .upsert(&embedder.signature(), &rows)
        .await
        .expect("upserts");
    index
        .upsert(&embedder.signature(), &rows)
        .await
        .expect("upserts");
    assert_eq!(index.len(), 1);
}

#[tokio::test]
async fn upserting_a_wrongly_sized_vector_is_refused() {
    // Better here than at query time, where a dimension mismatch is either a
    // server rejection on a contributor's pull request or, worse, a score.
    let index = MockChunkIndex::new();
    let signature = EmbedSignature::new("mock", "v1", 8);
    let bad = vec![EmbeddedChunk {
        chunk: chunk("o/r", "src/a.rs", 1, "x"),
        vector: vec![0.0; 4],
    }];
    assert!(index.upsert(&signature, &bad).await.is_err());
}

#[tokio::test]
async fn deleting_by_path_leaves_the_rest_of_the_repository_alone() {
    // The incremental re-index primitive. Without it, an edited file keeps its
    // old chunks and reviews start quoting code that no longer exists.
    let index = MockChunkIndex::new();
    let embedder = MockEmbedder::new(8);
    let rows = embedded(
        &embedder,
        vec![
            chunk("o/r", "src/a.rs", 1, "fn alpha() {}"),
            chunk("o/r", "src/a.rs", 20, "fn also_alpha() {}"),
            chunk("o/r", "src/b.rs", 1, "fn beta() {}"),
            chunk("o/other", "src/a.rs", 1, "fn alpha() {}"),
        ],
    )
    .await;
    index
        .upsert(&embedder.signature(), &rows)
        .await
        .expect("upserts");

    let gone = index
        .delete_paths("o/r", &["src/a.rs".to_string()])
        .await
        .expect("deletes");
    assert_eq!(gone, 2);
    assert_eq!(index.len(), 2, "src/b.rs and the other repo survive");
}

#[tokio::test]
async fn deleting_no_paths_deletes_nothing_rather_than_everything() {
    let index = MockChunkIndex::new();
    let embedder = MockEmbedder::new(8);
    let rows = embedded(
        &embedder,
        vec![chunk("o/r", "src/a.rs", 1, "fn alpha() {}")],
    )
    .await;
    index
        .upsert(&embedder.signature(), &rows)
        .await
        .expect("upserts");
    assert_eq!(index.delete_paths("o/r", &[]).await.expect("deletes"), 0);
    assert_eq!(index.len(), 1);
}

#[tokio::test]
async fn deleting_a_repository_leaves_other_repositories_alone() {
    let index = MockChunkIndex::new();
    let embedder = MockEmbedder::new(8);
    let rows = embedded(
        &embedder,
        vec![
            chunk("o/r", "src/a.rs", 1, "fn alpha() {}"),
            chunk("o/other", "src/a.rs", 1, "fn alpha() {}"),
        ],
    )
    .await;
    index
        .upsert(&embedder.signature(), &rows)
        .await
        .expect("upserts");
    assert_eq!(index.delete_repo("o/r").await.expect("deletes"), 1);
    assert_eq!(index.len(), 1);
}

#[tokio::test]
async fn the_lexical_arm_lifts_an_exact_identifier_match() {
    // The reason hybrid is not optional: a rare identifier shares no vocabulary
    // with anything, so a dense-only index ranks it no better than its
    // neighbours.
    let index = MockChunkIndex::new();
    let embedder = MockEmbedder::new(64);
    let rows = embedded(
        &embedder,
        vec![
            chunk("o/r", "src/a.rs", 1, "fn resolve_git_range(spec: &str) {}"),
            chunk("o/r", "src/b.rs", 1, "fn something_else_entirely() {}"),
        ],
    )
    .await;
    index
        .upsert(&embedder.signature(), &rows)
        .await
        .expect("upserts");

    let query = HybridQuery::new(
        embedder.signature(),
        "resolve_git_range",
        embedder
            .embed_query("resolve_git_range")
            .await
            .expect("embeds"),
    );
    let found = index.query(&query).await.expect("queries");
    assert_eq!(found[0].chunk.path, "src/a.rs");
    assert!(found[0].score > found[1].score);
}

#[tokio::test]
async fn a_query_can_be_confined_to_one_repository() {
    let index = MockChunkIndex::new();
    let embedder = MockEmbedder::new(16);
    let rows = embedded(
        &embedder,
        vec![
            chunk("o/r", "src/a.rs", 1, "fn alpha() {}"),
            chunk("o/other", "src/a.rs", 1, "fn alpha() {}"),
        ],
    )
    .await;
    index
        .upsert(&embedder.signature(), &rows)
        .await
        .expect("upserts");

    let vector = embedder.embed_query("alpha").await.expect("embeds");
    let query = HybridQuery::new(embedder.signature(), "alpha", vector).in_repo("o/r");
    let found = index.query(&query).await.expect("queries");
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].chunk.repo_id, "o/r");
}

#[tokio::test]
async fn a_query_honours_its_limit() {
    let index = MockChunkIndex::new();
    let embedder = MockEmbedder::new(16);
    let rows = embedded(
        &embedder,
        (0..10)
            .map(|i| {
                chunk(
                    "o/r",
                    "src/a.rs",
                    i * 10 + 1,
                    &format!("fn alpha{i}() {{}}"),
                )
            })
            .collect(),
    )
    .await;
    index
        .upsert(&embedder.signature(), &rows)
        .await
        .expect("upserts");

    let vector = embedder.embed_query("alpha").await.expect("embeds");
    let query = HybridQuery::new(embedder.signature(), "alpha", vector).limit(3);
    assert_eq!(index.query(&query).await.expect("queries").len(), 3);
}

#[tokio::test]
async fn preparing_the_index_records_the_signature() {
    let index = MockChunkIndex::new();
    let signature = EmbedSignature::new("mock", "v1", 8);
    index.prepare(&signature).await.expect("prepares");
    assert_eq!(index.prepared(), vec![signature]);
}

#[tokio::test]
async fn a_two_hop_walk_reaches_the_caller_of_a_caller() {
    // The case similarity search cannot serve: `c.rs` shares no vocabulary with
    // the changed file but breaks when it changes.
    let graph = MockGraphStore::new();
    graph
        .upsert_nodes(&[
            GraphNode::file("o/r", "src/a.rs"),
            GraphNode::file("o/r", "src/b.rs"),
            GraphNode::file("o/r", "src/c.rs"),
            GraphNode::file("o/r", "src/far.rs"),
        ])
        .await
        .expect("writes");
    graph
        .upsert_edges(&[
            GraphEdge::new("o/r", "src/b.rs", "src/a.rs", EdgeKind::Imports, "src/b.rs"),
            GraphEdge::new("o/r", "src/c.rs", "src/b.rs", EdgeKind::Imports, "src/c.rs"),
            GraphEdge::new(
                "o/r",
                "src/far.rs",
                "src/c.rs",
                EdgeKind::Imports,
                "src/far.rs",
            ),
        ])
        .await
        .expect("writes");

    let seeds = vec!["src/a.rs".to_string()];
    let one = graph
        .neighbours("o/r", &seeds, 1, &EdgeKind::ALL)
        .await
        .expect("walks");
    let paths: Vec<&str> = one.nodes.iter().map(|n| n.path.as_str()).collect();
    assert_eq!(paths, vec!["src/a.rs", "src/b.rs"]);

    let two = graph
        .neighbours("o/r", &seeds, 2, &EdgeKind::ALL)
        .await
        .expect("walks");
    let paths: Vec<&str> = two.nodes.iter().map(|n| n.path.as_str()).collect();
    assert_eq!(paths, vec!["src/a.rs", "src/b.rs", "src/c.rs"]);
    assert!(!paths.contains(&"src/far.rs"), "three hops is out of range");
}

#[tokio::test]
async fn a_walk_can_be_confined_to_one_edge_kind() {
    let graph = MockGraphStore::new();
    graph
        .upsert_nodes(&[
            GraphNode::file("o/r", "src/a.rs"),
            GraphNode::file("o/r", "src/b.rs"),
            GraphNode::symbol("o/r", "src/a.rs", "alpha"),
        ])
        .await
        .expect("writes");
    graph
        .upsert_edges(&[
            GraphEdge::new("o/r", "src/b.rs", "src/a.rs", EdgeKind::Imports, "src/b.rs"),
            GraphEdge::new(
                "o/r",
                "src/a.rs",
                "src/a.rs#alpha",
                EdgeKind::Defines,
                "src/a.rs",
            ),
        ])
        .await
        .expect("writes");

    let found = graph
        .neighbours("o/r", &["src/a.rs".to_string()], 1, &[EdgeKind::Defines])
        .await
        .expect("walks");
    let ids: Vec<&str> = found.nodes.iter().map(|n| n.id.as_str()).collect();
    assert_eq!(ids, vec!["src/a.rs", "src/a.rs#alpha"]);
}

#[tokio::test]
async fn an_unknown_seed_is_skipped_rather_than_an_error() {
    // A pull request that adds a file names a path the graph has never seen.
    let graph = MockGraphStore::new();
    let found = graph
        .neighbours("o/r", &["src/brand_new.rs".to_string()], 2, &EdgeKind::ALL)
        .await
        .expect("walks");
    assert!(found.nodes.is_empty());
    assert!(found.edges.is_empty());
}

#[tokio::test]
async fn deleting_a_path_takes_its_symbols_and_edges_with_it() {
    let graph = MockGraphStore::new();
    graph
        .upsert_nodes(&[
            GraphNode::file("o/r", "src/a.rs"),
            GraphNode::symbol("o/r", "src/a.rs", "alpha"),
            GraphNode::file("o/r", "src/b.rs"),
        ])
        .await
        .expect("writes");
    graph
        .upsert_edges(&[GraphEdge::new(
            "o/r",
            "src/a.rs",
            "src/a.rs#alpha",
            EdgeKind::Defines,
            "src/a.rs",
        )])
        .await
        .expect("writes");

    let gone = graph
        .delete_paths("o/r", &["src/a.rs".to_string()])
        .await
        .expect("deletes");
    assert_eq!(gone, 3, "two nodes and one edge");
    assert_eq!(graph.node_count(), 1);
    assert_eq!(graph.edge_count(), 0);
}

#[tokio::test]
async fn deleting_a_repository_graph_leaves_other_repositories_alone() {
    let graph = MockGraphStore::new();
    graph
        .upsert_nodes(&[
            GraphNode::file("o/r", "src/a.rs"),
            GraphNode::file("o/other", "src/a.rs"),
        ])
        .await
        .expect("writes");
    assert_eq!(graph.delete_repo("o/r").await.expect("deletes"), 1);
    assert_eq!(graph.node_count(), 1);
}

#[tokio::test]
async fn upserting_the_same_edge_twice_is_one_edge() {
    let graph = MockGraphStore::new();
    let edge = GraphEdge::new("o/r", "a", "b", EdgeKind::Calls, "src/a.rs");
    graph
        .upsert_edges(std::slice::from_ref(&edge))
        .await
        .expect("writes");
    graph.upsert_edges(&[edge]).await.expect("writes");
    assert_eq!(graph.edge_count(), 1);
}

#[tokio::test]
async fn a_repository_lookup_inherits_its_organisation_documents() {
    let store = MockKnowledgeStore::new();
    store
        .put(
            &KnowledgeDoc::new("org-style", KnowledgeScope::org("o"), "Style", "no unwrap")
                .pinned(),
        )
        .await
        .expect("writes");
    store
        .put(&KnowledgeDoc::new("repo-note", KnowledgeScope::repo("o/r"), "Note", "local").pinned())
        .await
        .expect("writes");
    store
        .put(
            &KnowledgeDoc::new(
                "other-note",
                KnowledgeScope::repo("o/other"),
                "Note",
                "elsewhere",
            )
            .pinned(),
        )
        .await
        .expect("writes");

    let mut ids: Vec<String> = store
        .pinned(&KnowledgeScope::repo("o/r"))
        .await
        .expect("reads")
        .into_iter()
        .map(|d| d.id)
        .collect();
    ids.sort();
    assert_eq!(ids, vec!["org-style", "repo-note"]);
}

#[tokio::test]
async fn an_organisation_lookup_does_not_see_one_repositorys_documents() {
    // A convention written for one repository is not evidence about another.
    let store = MockKnowledgeStore::new();
    store
        .put(&KnowledgeDoc::new("repo-note", KnowledgeScope::repo("o/r"), "Note", "local").pinned())
        .await
        .expect("writes");
    assert!(
        store
            .pinned(&KnowledgeScope::org("o"))
            .await
            .expect("reads")
            .is_empty()
    );
}

#[tokio::test]
async fn pinned_and_retrievable_documents_are_disjoint_sets() {
    let store = MockKnowledgeStore::new();
    store
        .put(&KnowledgeDoc::new("pin", KnowledgeScope::org("o"), "Pinned", "always").pinned())
        .await
        .expect("writes");
    store
        .put(&KnowledgeDoc::new(
            "ret",
            KnowledgeScope::org("o"),
            "Retrievable",
            "sometimes",
        ))
        .await
        .expect("writes");

    let scope = KnowledgeScope::org("o");
    let pinned = store.pinned(&scope).await.expect("reads");
    let retrievable = store.retrievable(&scope).await.expect("reads");
    assert_eq!(pinned.len(), 1);
    assert_eq!(retrievable.len(), 1);
    assert_ne!(pinned[0].id, retrievable[0].id);
}

#[tokio::test]
async fn a_document_round_trips_and_deletes_once() {
    let store = MockKnowledgeStore::new();
    let doc = KnowledgeDoc::new("d", KnowledgeScope::org("o"), "T", "B");
    store.put(&doc).await.expect("writes");
    assert_eq!(store.get("d").await.expect("reads"), Some(doc));
    assert!(store.delete("d").await.expect("deletes"));
    assert!(!store.delete("d").await.expect("deletes"));
    assert!(store.is_empty());
}
