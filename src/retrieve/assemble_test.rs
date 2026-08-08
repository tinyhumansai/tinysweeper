//! Tests for ranking, dedupe and the token budget.

use super::*;

fn chunk(path: &str, start: u32, end: u32, text: &str) -> Chunk {
    Chunk {
        repo_id: "acme/app".into(),
        path: path.into(),
        start_line: start,
        end_line: end,
        text: text.into(),
        ..Chunk::default()
    }
}

fn hit(path: &str, start: u32, end: u32, score: f64) -> ScoredChunk {
    ScoredChunk {
        chunk: chunk(path, start, end, "fn similar() { work(); }"),
        score,
    }
}

#[test]
fn the_budget_is_a_ceiling_and_what_it_dropped_is_counted() {
    // 200 chunks of a few hundred bytes each is far more than 200 tokens.
    let hits: Vec<ScoredChunk> = (0..200)
        .map(|index| {
            hit(
                &format!("src/f{index}.rs"),
                1,
                40,
                1.0 - index as f64 / 200.0,
            )
        })
        .collect();

    let assembled = assemble(hits, Vec::new(), &[], 500, 200);

    assert!(assembled.tokens <= 200, "{}", assembled.tokens);
    assert!(!assembled.chunks.is_empty(), "the budget is not zero");
    assert!(assembled.dropped > 0, "dropped chunks must be reported");
    assert_eq!(assembled.dropped, 200 - assembled.chunks.len());
}

#[test]
fn the_chunk_cap_bites_before_the_token_budget_does() {
    let hits: Vec<ScoredChunk> = (0..50)
        .map(|index| hit(&format!("src/f{index}.rs"), 1, 2, 1.0))
        .collect();
    let assembled = assemble(hits, Vec::new(), &[], 5, 100_000);

    assert_eq!(assembled.chunks.len(), 5);
    assert_eq!(assembled.dropped, 45);
}

#[test]
fn overlapping_spans_of_one_file_are_deduplicated() {
    // Both arms routinely return the same function under different boundaries,
    // because chunk boundaries move when a file is re-chunked.
    let assembled = assemble(
        vec![hit("src/a.rs", 10, 40, 0.9)],
        vec![chunk("src/a.rs", 20, 60, "fn same() {}")],
        &[],
        20,
        100_000,
    );

    assert_eq!(assembled.chunks.len(), 1);
    assert_eq!(assembled.dropped, 1);
}

#[test]
fn adjacent_but_disjoint_spans_of_one_file_both_survive() {
    let assembled = assemble(
        vec![hit("src/a.rs", 1, 10, 0.9)],
        vec![chunk("src/a.rs", 11, 20, "fn other() {}")],
        &[],
        20,
        100_000,
    );

    assert_eq!(assembled.chunks.len(), 2);
    assert_eq!(assembled.dropped, 0);
}

#[test]
fn the_pull_requests_own_files_are_never_returned_as_context() {
    let changed = vec!["src/changed.rs".to_string()];
    let assembled = assemble(
        vec![
            hit("src/changed.rs", 1, 10, 1.0),
            hit("src/other.rs", 1, 10, 0.5),
        ],
        vec![chunk("src/changed.rs", 40, 50, "fn also_changed() {}")],
        &changed,
        20,
        100_000,
    );

    assert_eq!(assembled.chunks.len(), 1);
    assert_eq!(assembled.chunks[0].chunk.path, "src/other.rs");
}

#[test]
fn a_rich_search_arm_cannot_shut_the_graph_out_entirely() {
    // The regression the interleave exists for: concatenating search-first
    // means a diff with plenty of lexical neighbours never spends a token on
    // the caller it breaks.
    let hits: Vec<ScoredChunk> = (0..100)
        .map(|index| hit(&format!("src/s{index}.rs"), 1, 4, 1.0))
        .collect();
    let reached: Vec<Chunk> = (0..10)
        .map(|index| chunk(&format!("src/g{index}.rs"), 1, 4, "fn reached() {}"))
        .collect();

    let assembled = assemble(hits, reached, &[], 9, 100_000);
    let graph = assembled
        .chunks
        .iter()
        .filter(|c| c.provenance == Provenance::Graph)
        .count();

    assert_eq!(assembled.chunks.len(), 9);
    assert_eq!(graph, 3, "one graph hit per two search hits");
}

#[test]
fn one_oversized_chunk_does_not_shut_out_the_smaller_ones_behind_it() {
    let assembled = assemble(
        vec![
            ScoredChunk {
                chunk: chunk("src/huge.rs", 1, 9000, &"x".repeat(100_000)),
                score: 1.0,
            },
            hit("src/small.rs", 1, 4, 0.5),
        ],
        Vec::new(),
        &[],
        20,
        400,
    );

    assert_eq!(assembled.chunks.len(), 1);
    assert_eq!(assembled.chunks[0].chunk.path, "src/small.rs");
    assert_eq!(assembled.dropped, 1);
}

#[test]
fn a_zero_budget_keeps_nothing_and_says_so() {
    let assembled = assemble(vec![hit("src/a.rs", 1, 4, 1.0)], Vec::new(), &[], 20, 0);
    assert!(assembled.chunks.is_empty());
    assert_eq!(assembled.dropped, 1);
    assert_eq!(assembled.tokens, 0);
}

#[test]
fn assembling_nothing_produces_nothing_rather_than_a_panic() {
    assert_eq!(
        assemble(Vec::new(), Vec::new(), &[], 20, 1000),
        Assembled::default()
    );
}
