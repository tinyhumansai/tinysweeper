//! Tests for graph expansion.
//!
//! The acceptance criterion is
//! [`a_diff_touching_a_callee_surfaces_its_caller`]: without it the graph is a
//! picture nobody looks at, which is exactly what building one and never
//! traversing it at review time amounts to.

use super::*;
use crate::config::types::Retrieval;
use crate::evidence::diff::parse_file_patch;
use crate::index::types::{EmbeddedChunk, GraphEdge, GraphNode, NodeKind};
use crate::index::{MockChunkIndex, MockEmbedder, MockGraphStore};
use crate::ports::embed::Embedder;

const REPO: &str = "acme/app";

/// The bounds a test walks under. Named rather than inlined so a test that
/// changes one of them says which one it changed.
fn bounds(graph_hops: u8, max_graph_nodes: usize, max_chunks: usize) -> Retrieval {
    Retrieval {
        enabled: true,
        query_chars: 4000,
        context_tokens: 8000,
        max_chunks,
        graph_hops,
        max_graph_nodes,
    }
}

fn signature() -> EmbedSignature {
    MockEmbedder::new(16).signature()
}

async fn index_with(chunks: &[(&str, u32, u32, &str)]) -> MockChunkIndex {
    let index = MockChunkIndex::new();
    let embedder = MockEmbedder::new(16);
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
        .upsert(&signature(), &embedded)
        .await
        .expect("upserts");
    index
}

/// `src/caller.rs` calls `settle` in `src/billing.rs`, which the diff changes.
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
                "src/billing.rs",
                "src/billing.rs#settle",
                EdgeKind::Defines,
                "src/billing.rs",
            ),
            GraphEdge::new(
                REPO,
                "src/caller.rs",
                "src/caller.rs#checkout",
                EdgeKind::Defines,
                "src/caller.rs",
            ),
            GraphEdge::new(
                REPO,
                "src/caller.rs#checkout",
                "src/billing.rs#settle",
                EdgeKind::Calls,
                "src/caller.rs",
            ),
        ])
        .await
        .expect("edges");
    graph
}

fn callee_diff() -> Vec<FileDiff> {
    vec![parse_file_patch(
        "src/billing.rs",
        "@@ -10,3 +10,4 @@ pub fn settle(order: &Order) -> Result<()>\n ctx\n+    charge(order)?;\n more\n",
    )]
}

#[tokio::test]
async fn a_diff_touching_a_callee_surfaces_its_caller() {
    // The chunk of `src/caller.rs` shares no vocabulary with the diff, so no
    // similarity query would ever return it. Only the graph can.
    let graph = caller_graph().await;
    let index = index_with(&[(
        "src/caller.rs",
        1,
        12,
        "fn checkout(order: &Order) { settle(order); }",
    )])
    .await;

    let expansion = expand(
        &graph,
        &index,
        &signature(),
        REPO,
        &callee_diff(),
        &bounds(2, 60, 40),
    )
    .await
    .expect("expands");

    assert!(
        expansion
            .chunks
            .iter()
            .any(|chunk| chunk.path == "src/caller.rs"),
        "the caller must be reached: {:?}",
        expansion.chunks
    );
    assert!(expansion.nodes >= 2);
}

#[tokio::test]
async fn one_hop_from_a_symbol_seed_is_enough_to_reach_the_caller() {
    // The hunk heading names `settle`, so the seed is `src/billing.rs#settle`
    // and the caller's symbol is a single `calls` edge away. This is why the
    // headings are retained by the diff parser.
    let graph = caller_graph().await;
    let index = index_with(&[("src/caller.rs", 1, 12, "fn checkout() {}")]).await;

    let expansion = expand(
        &graph,
        &index,
        &signature(),
        REPO,
        &callee_diff(),
        &bounds(1, 60, 40),
    )
    .await
    .expect("expands");

    assert_eq!(expansion.chunks.len(), 1);
    assert_eq!(expansion.chunks[0].path, "src/caller.rs");
}

#[tokio::test]
async fn the_changed_files_own_chunks_are_never_returned_as_context() {
    // The lane is already reading their diff; restating it would spend the
    // context budget on the one thing the prompt certainly already has.
    let graph = caller_graph().await;
    let index = index_with(&[
        ("src/billing.rs", 1, 40, "pub fn settle() {}"),
        ("src/caller.rs", 1, 12, "fn checkout() {}"),
    ])
    .await;

    let expansion = expand(
        &graph,
        &index,
        &signature(),
        REPO,
        &callee_diff(),
        &bounds(2, 60, 40),
    )
    .await
    .expect("expands");

    assert!(
        expansion
            .chunks
            .iter()
            .all(|chunk| chunk.path != "src/billing.rs"),
        "{:?}",
        expansion.chunks
    );
}

#[tokio::test]
async fn a_zero_bound_expands_nothing_rather_than_everything() {
    let graph = caller_graph().await;
    let index = index_with(&[("src/caller.rs", 1, 12, "fn checkout() {}")]).await;

    for (hops, nodes, chunks) in [(0, 60, 40), (2, 0, 40), (2, 60, 0)] {
        let expansion = expand(
            &graph,
            &index,
            &signature(),
            REPO,
            &callee_diff(),
            &bounds(hops, nodes, chunks),
        )
        .await
        .expect("expands");
        assert_eq!(expansion, Expansion::default(), "{hops}/{nodes}/{chunks}");
    }
}

#[tokio::test]
async fn a_reached_file_with_no_indexed_chunks_still_counts_as_a_reached_node() {
    // "The graph found nothing" and "the graph found plenty, none of it
    // indexed" need different fixes, so they are reported differently.
    let graph = caller_graph().await;
    let index = MockChunkIndex::new();

    let expansion = expand(
        &graph,
        &index,
        &signature(),
        REPO,
        &callee_diff(),
        &bounds(2, 60, 40),
    )
    .await
    .expect("expands");

    assert!(expansion.chunks.is_empty());
    assert!(expansion.nodes > 0);
}

#[test]
fn seeds_name_both_the_changed_files_and_the_symbols_their_hunks_are_inside() {
    let seeded = seeds(&callee_diff());
    assert!(seeded.contains(&"src/billing.rs".to_string()), "{seeded:?}");
    assert!(
        seeded.contains(&"src/billing.rs#settle".to_string()),
        "{seeded:?}"
    );
}

#[test]
fn a_symbol_is_read_out_of_a_heading_in_several_languages() {
    for (heading, expected) in [
        (
            "pub fn settle_invoice(order: &Order) -> Result<()>",
            "settle_invoice",
        ),
        (
            "func (s *Server) HandleRequest(w http.ResponseWriter)",
            "HandleRequest",
        ),
        ("class LedgerEntry:", "LedgerEntry"),
        ("def compute_total(self, rows):", "compute_total"),
        (
            "export async function parseRequest(input: string) {",
            "parseRequest",
        ),
        ("impl Display for RepoId {", "RepoId"),
    ] {
        assert_eq!(
            symbol_of(heading).as_deref(),
            Some(expected),
            "heading: {heading}"
        );
    }
}

#[test]
fn a_heading_that_names_nothing_seeds_nothing() {
    for heading in ["", "   ", "{", "42", "(", "}"] {
        assert_eq!(symbol_of(heading), None, "heading: {heading:?}");
    }
}

#[test]
fn seeds_are_deduplicated_so_one_file_is_not_walked_repeatedly() {
    let diff = parse_file_patch(
        "src/a.rs",
        "@@ -1,2 +1,3 @@ fn same()\n a\n+b\n c\n@@ -20,2 +21,3 @@ fn same()\n x\n+y\n z\n",
    );
    let seeded = seeds(&[diff]);
    assert_eq!(seeded, vec!["src/a.rs".to_string(), "src/a.rs#same".into()]);
}

#[test]
fn a_node_kind_is_irrelevant_to_seeding_but_the_path_is_what_gets_fetched() {
    // Documents the contract expansion relies on: a symbol node carries the
    // file it lives in, which is what `chunks_in_paths` is keyed on.
    let node = GraphNode::symbol(REPO, "src/a.rs", "thing");
    assert_eq!(node.kind, NodeKind::Symbol);
    assert_eq!(node.path, "src/a.rs");
}
