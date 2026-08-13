//! Behaviour tests for an indexing run.
//!
//! These are the tests that keep incremental indexing honest, and most of them
//! assert a *negative*: no embedding call, no chunk moved, no deletion before a
//! write. A retrieval layer rots quietly, and every one of these guards a way
//! it has rotted in a real product.

use super::*;

use std::path::PathBuf;

use crate::index::{MockChunkIndex, MockEmbedder};
use crate::indexer::mock::{CountingEmbedder, MockManifest};
use crate::indexer::types::IndexState;
use crate::ports::index::ChunkIndex;

const REPO: &str = "tinyhumansai/tinysweeper";

/// A checkout with a couple of source files.
struct Checkout {
    dir: tempfile::TempDir,
}

impl Checkout {
    fn new() -> Self {
        let checkout = Self {
            dir: tempfile::tempdir().expect("tempdir"),
        };
        checkout.write("src/alpha.rs", "fn alpha() -> usize {\n    1\n}\n");
        checkout.write("src/beta.rs", "fn beta() -> usize {\n    2\n}\n");
        checkout
    }

    fn root(&self) -> PathBuf {
        self.dir.path().to_path_buf()
    }

    fn write(&self, path: &str, contents: &str) {
        let full = self.dir.path().join(path);
        std::fs::create_dir_all(full.parent().expect("parent")).expect("dirs");
        std::fs::write(full, contents).expect("write");
    }

    fn remove(&self, path: &str) {
        std::fs::remove_file(self.dir.path().join(path)).expect("remove");
    }
}

struct Rig {
    embedder: CountingEmbedder<MockEmbedder>,
    index: MockChunkIndex,
    manifest: MockManifest,
}

impl Rig {
    fn new() -> Self {
        Self {
            embedder: CountingEmbedder::new(MockEmbedder::new(16)),
            index: MockChunkIndex::new(),
            manifest: MockManifest::new(),
        }
    }

    fn indexer(&self) -> Indexer<'_> {
        Indexer::new(&self.embedder, &self.index, &self.manifest)
            .expect("builds")
            .as_holder("worker-1")
    }

    fn signature(&self) -> crate::index::EmbedSignature {
        crate::ports::embed::Embedder::signature(&self.embedder)
    }

    /// Every chunk in the index, as `(path, id)`.
    async fn rows(&self) -> Vec<(String, String)> {
        let query = crate::index::HybridQuery::new(self.signature(), "fn", vec![0.0; 16])
            .in_repo(REPO)
            .limit(1_000);
        // The lexical arm alone would miss chunks sharing no token with the
        // query, so this asks for everything and sorts locally.
        let mut rows: Vec<(String, String)> = self
            .index
            .query(&query)
            .await
            .expect("queries")
            .into_iter()
            .map(|hit| (hit.chunk.path.clone(), hit.chunk.id()))
            .collect();
        rows.sort();
        rows
    }
}

fn report(outcome: IndexOutcome) -> IndexReport {
    match outcome {
        IndexOutcome::Indexed(report) => report,
        other => panic!("expected a run, got {other:?}"),
    }
}

#[test]
fn confirmation_retains_stale_ids_until_the_delete_is_finalized() {
    let old = format!("{REPO}\u{1f}src/a.rs\u{1f}1\u{1f}old-hash");
    let work = FileWork::new(
        "src/a.rs".into(),
        vec![crate::index::Chunk {
            repo_id: REPO.into(),
            path: "src/a.rs".into(),
            start_line: 1,
            end_line: 1,
            text: "fn current() {}".into(),
            content_hash: "new-hash".into(),
            ..Default::default()
        }],
        Some(&crate::indexer::IndexedFile::confirmed(
            "src/a.rs",
            vec![old.clone()],
        )),
    );

    assert!(work.confirmation().pending.contains(&old));
    assert!(work.finalized().pending.is_empty());
}

#[tokio::test]
async fn re_indexing_unchanged_content_issues_zero_embedding_calls() {
    // The one number that decides whether this module was worth writing. A
    // chunker without content hashing re-embeds the whole repository on every
    // push; here the second run must not call the embedder at all.
    let checkout = Checkout::new();
    let rig = Rig::new();

    let first = report(
        rig.indexer()
            .index_repo(REPO, "sha-1", &checkout.root())
            .await
            .expect("indexes"),
    );
    assert!(first.upserted > 0, "the first run must write something");
    assert!(rig.embedder.calls() > 0);

    rig.embedder.reset();
    let second = report(
        rig.indexer()
            .index_repo(REPO, "sha-2", &checkout.root())
            .await
            .expect("indexes"),
    );

    assert_eq!(
        rig.embedder.calls(),
        0,
        "unchanged content must not be re-embedded"
    );
    assert_eq!(rig.embedder.texts(), 0);
    assert_eq!(second.upserted, 0);
    assert_eq!(second.deleted, 0);
    assert_eq!(second.reused, first.upserted);
    assert_eq!(second.usage.cost_usd, 0.0);

    // The same finding, reported for the code graph to act on. A `changed` list
    // that named every file would make the graph re-parse the whole tree on a
    // push that embedded nothing — the exact waste this run just avoided.
    assert_eq!(second.changed, Vec::<String>::new());
    assert_eq!(first.changed.len(), first.files as usize);
}

#[tokio::test]
async fn only_the_edited_file_is_reported_as_changed() {
    let checkout = Checkout::new();
    let rig = Rig::new();
    rig.indexer()
        .index_repo(REPO, "sha-1", &checkout.root())
        .await
        .expect("indexes");

    checkout.write("src/beta.rs", "fn beta() -> usize {\n    22\n}\n");
    let second = report(
        rig.indexer()
            .index_repo(REPO, "sha-2", &checkout.root())
            .await
            .expect("indexes"),
    );

    assert_eq!(second.changed, ["src/beta.rs"]);
    assert!(second.removed.is_empty());
}

#[tokio::test]
async fn a_deleted_file_is_reported_as_removed_rather_than_changed() {
    let checkout = Checkout::new();
    let rig = Rig::new();
    rig.indexer()
        .index_repo(REPO, "sha-1", &checkout.root())
        .await
        .expect("indexes");

    checkout.remove("src/beta.rs");
    let second = report(
        rig.indexer()
            .index_repo(REPO, "sha-2", &checkout.root())
            .await
            .expect("indexes"),
    );

    assert_eq!(second.removed, ["src/beta.rs"]);
    assert!(second.changed.is_empty(), "{:?}", second.changed);
}

#[tokio::test]
async fn an_unchanged_revision_is_not_re_indexed_at_all() {
    let checkout = Checkout::new();
    let rig = Rig::new();
    rig.indexer()
        .index_repo(REPO, "sha-1", &checkout.root())
        .await
        .expect("indexes");

    let outcome = rig
        .indexer()
        .index_repo(REPO, "sha-1", &checkout.root())
        .await
        .expect("checks");
    assert_eq!(outcome, IndexOutcome::AlreadyFresh);
}

#[tokio::test]
async fn changing_one_file_moves_only_that_files_chunks() {
    let checkout = Checkout::new();
    let rig = Rig::new();
    rig.indexer()
        .index_repo(REPO, "sha-1", &checkout.root())
        .await
        .expect("indexes");
    let before = rig.rows().await;
    let untouched: Vec<_> = before
        .iter()
        .filter(|(path, _)| path == "src/beta.rs")
        .cloned()
        .collect();

    checkout.write("src/alpha.rs", "fn alpha() -> usize {\n    99\n}\n");
    rig.embedder.reset();
    let second = report(
        rig.indexer()
            .index_paths(REPO, "sha-2", &checkout.root(), &["src/alpha.rs".into()])
            .await
            .expect("indexes"),
    );

    assert!(second.upserted > 0 && second.deleted > 0);
    let after = rig.rows().await;
    assert_eq!(
        after
            .iter()
            .filter(|(path, _)| path == "src/beta.rs")
            .cloned()
            .collect::<Vec<_>>(),
        untouched,
        "an untouched file's chunks must keep their exact ids"
    );
    assert_ne!(
        before
            .iter()
            .filter(|(path, _)| path == "src/alpha.rs")
            .collect::<Vec<_>>(),
        after
            .iter()
            .filter(|(path, _)| path == "src/alpha.rs")
            .collect::<Vec<_>>(),
        "the edited file's chunks must have been replaced"
    );
}

#[tokio::test]
async fn a_first_changed_path_run_builds_a_complete_baseline() {
    let checkout = Checkout::new();
    let rig = Rig::new();

    rig.indexer()
        .index_paths(REPO, "sha-1", &checkout.root(), &["src/alpha.rs".into()])
        .await
        .expect("indexes");

    assert!(
        rig.rows()
            .await
            .iter()
            .any(|(path, _)| path == "src/beta.rs"),
        "a changed-path list is not a complete repository baseline"
    );
    assert!(
        rig.manifest
            .snapshot(REPO, &rig.signature())
            .is_fresh("sha-1")
    );
}

#[cfg(unix)]
#[tokio::test]
async fn an_incremental_symlink_is_never_read_as_repository_content() {
    use std::os::unix::fs::symlink;

    let checkout = Checkout::new();
    let rig = Rig::new();
    rig.indexer()
        .index_repo(REPO, "sha-1", &checkout.root())
        .await
        .expect("indexes");
    let outside = tempfile::NamedTempFile::new().expect("outside file");
    std::fs::write(outside.path(), "fn host_secret() {}\n").expect("writes");
    symlink(outside.path(), checkout.root().join("src/link.rs")).expect("links");

    rig.indexer()
        .index_paths(REPO, "sha-2", &checkout.root(), &["src/link.rs".into()])
        .await
        .expect("indexes");

    assert!(
        !rig.rows()
            .await
            .iter()
            .any(|(path, _)| path == "src/link.rs"),
        "a symlink must not cause host data to be embedded"
    );
}

#[test]
fn moving_an_unchanged_definition_reuses_its_embedding() {
    let old = crate::index::Chunk {
        repo_id: REPO.into(),
        path: "src/alpha.rs".into(),
        start_line: 1,
        end_line: 3,
        text: "fn alpha() {}".into(),
        content_hash: "same-content".into(),
        ..Default::default()
    };
    let moved = crate::index::Chunk {
        start_line: 2,
        end_line: 4,
        ..old.clone()
    };
    let work = FileWork::new(
        "src/alpha.rs".into(),
        vec![moved.clone()],
        Some(&crate::indexer::IndexedFile::confirmed(
            "src/alpha.rs",
            vec![old.id()],
        )),
    );

    assert!(work.to_embed.is_empty());
    assert_eq!(work.to_relocate, vec![(old.id(), moved)]);
}

#[tokio::test]
async fn nothing_is_deleted_before_the_replacement_is_written() {
    // The failure this guards: delete-then-embed leaves a repository with zero
    // chunks whenever the embedding step fails. Asserting the *order* directly
    // is awkward, so this asserts its consequence — the file is never
    // unrepresented, and the count never dips.
    let checkout = Checkout::new();
    let rig = Rig::new();
    rig.indexer()
        .index_repo(REPO, "sha-1", &checkout.root())
        .await
        .expect("indexes");
    let before = rig.index.len();

    checkout.write(
        "src/alpha.rs",
        "fn alpha() -> usize {\n    let x = 5;\n    x\n}\n",
    );
    rig.indexer()
        .index_paths(REPO, "sha-2", &checkout.root(), &["src/alpha.rs".into()])
        .await
        .expect("indexes");

    assert_eq!(rig.index.len(), before, "one chunk in, one chunk out");
    assert!(
        rig.rows()
            .await
            .iter()
            .any(|(path, _)| path == "src/alpha.rs"),
        "the file must never be unrepresented"
    );
}

#[tokio::test]
async fn a_deleted_file_loses_its_chunks() {
    let checkout = Checkout::new();
    let rig = Rig::new();
    rig.indexer()
        .index_repo(REPO, "sha-1", &checkout.root())
        .await
        .expect("indexes");

    checkout.remove("src/beta.rs");
    let second = report(
        rig.indexer()
            .index_paths(REPO, "sha-2", &checkout.root(), &["src/beta.rs".into()])
            .await
            .expect("indexes"),
    );

    assert!(second.deleted > 0);
    assert!(
        !rig.rows()
            .await
            .iter()
            .any(|(path, _)| path == "src/beta.rs")
    );
    assert!(
        !rig.manifest
            .paths(REPO, &rig.signature())
            .await
            .expect("lists")
            .contains(&"src/beta.rs".to_string())
    );
}

#[tokio::test]
async fn a_file_deleted_between_full_indexes_is_swept_up() {
    let checkout = Checkout::new();
    let rig = Rig::new();
    rig.indexer()
        .index_repo(REPO, "sha-1", &checkout.root())
        .await
        .expect("indexes");

    checkout.remove("src/beta.rs");
    rig.indexer()
        .index_repo(REPO, "sha-2", &checkout.root())
        .await
        .expect("indexes");

    assert!(
        !rig.rows()
            .await
            .iter()
            .any(|(path, _)| path == "src/beta.rs"),
        "a full re-index must notice a file that is simply gone"
    );
}

#[tokio::test]
async fn an_oversized_file_is_reported_rather_than_dropped() {
    let checkout = Checkout::new();
    checkout.write("src/huge.rs", &"fn f() {}\n".repeat(500));
    let rig = Rig::new();

    let indexer = Indexer::new(&rig.embedder, &rig.index, &rig.manifest)
        .expect("builds")
        .with_selector(
            crate::chunk::Selector::new(&[])
                .expect("globs")
                .max_bytes(500),
        );
    let report = report(
        indexer
            .index_repo(REPO, "sha-1", &checkout.root())
            .await
            .expect("indexes"),
    );

    let skipped = report
        .skipped
        .iter()
        .find(|s| s.path == "src/huge.rs")
        .expect("the oversized file is reported, not silently dropped");
    assert!(matches!(
        skipped.reason,
        crate::chunk::SkipReason::TooLarge { .. }
    ));
    let summary = report.summary();
    assert!(summary.contains("src/huge.rs"), "{summary}");
}

#[tokio::test]
async fn a_contended_claim_requeues_instead_of_blocking() {
    let checkout = Checkout::new();
    let rig = Rig::new();
    // Somebody else is already indexing.
    rig.manifest
        .claim(REPO, &rig.signature(), "worker-0")
        .await
        .expect("claims");

    let outcome = rig
        .indexer()
        .index_repo(REPO, "sha-1", &checkout.root())
        .await
        .expect("answers");
    assert_eq!(
        outcome,
        IndexOutcome::Requeue {
            holder: Some("worker-0".into())
        }
    );
    assert!(
        rig.index.is_empty(),
        "a requeued run must not have written anything"
    );
}

#[tokio::test]
async fn a_completed_run_leaves_the_repository_ready_and_claimable() {
    let checkout = Checkout::new();
    let rig = Rig::new();
    rig.indexer()
        .index_repo(REPO, "sha-1", &checkout.root())
        .await
        .expect("indexes");

    let record = rig.manifest.snapshot(REPO, &rig.signature());
    assert_eq!(record.state, IndexState::Ready);
    assert!(record.is_fresh("sha-1"));
    assert!(record.chunks > 0);
}

#[tokio::test]
async fn a_run_that_hits_its_budget_stops_with_a_partial_index_rather_than_failing() {
    let checkout = Checkout::new();
    let rig = Rig::new();
    // The mock embedder is priced at zero, so a budget of zero can never be
    // exceeded; a signature with a real price is what makes the ceiling bite.
    let embedder = CountingEmbedder::new(MockEmbedder::with_signature(
        crate::index::EmbedSignature::new("voyage", "voyage-code-3", 16),
    ));
    let indexer = Indexer::new(&embedder, &rig.index, &rig.manifest)
        .expect("builds")
        .with_batch(1)
        .with_budget(0.000_000_001);

    let report = report(
        indexer
            .index_repo(REPO, "sha-1", &checkout.root())
            .await
            .expect("indexes"),
    );
    assert!(report.budget_exhausted);
    assert_eq!(
        embedder.calls(),
        0,
        "the ceiling is checked before spending"
    );

    // And the revision is not claimed, so the next run finishes the job.
    let record = rig
        .manifest
        .snapshot(REPO, &crate::ports::embed::Embedder::signature(&embedder));
    assert_eq!(record.state, IndexState::Ready);
    assert!(!record.is_fresh("sha-1"));
}

#[tokio::test]
async fn embedding_spend_is_counted_rather_than_left_untracked() {
    let checkout = Checkout::new();
    let rig = Rig::new();
    let embedder = CountingEmbedder::new(MockEmbedder::with_signature(
        crate::index::EmbedSignature::new("voyage", "voyage-code-3", 16),
    ));
    let indexer = Indexer::new(&embedder, &rig.index, &rig.manifest).expect("builds");

    let report = report(
        indexer
            .index_repo(REPO, "sha-1", &checkout.root())
            .await
            .expect("indexes"),
    );
    assert!(report.usage.tokens > 0);
    assert!(
        report.usage.cost_usd > 0.0,
        "a priced model must show a cost; unpriced indexing is how the bill surprises somebody"
    );
    assert_eq!(report.usage.calls, embedder.calls());

    // And it accumulates on the repository's record, not just this run's.
    let record = rig
        .manifest
        .snapshot(REPO, &crate::ports::embed::Embedder::signature(&embedder));
    assert_eq!(record.usage.cost_usd, report.usage.cost_usd);
}

#[tokio::test]
async fn an_unreadable_file_is_reported_and_does_not_abort_the_run() {
    let checkout = Checkout::new();
    let rig = Rig::new();
    // A path the selector accepts (size and extension come from the caller)
    // but the filesystem will not produce.
    let outcome = rig
        .indexer()
        .index_paths(REPO, "sha-1", &checkout.root(), &["src/alpha.rs".into()])
        .await
        .expect("indexes");
    assert!(matches!(outcome, IndexOutcome::Indexed(_)));

    checkout.write("src/gone.rs", "fn gone() {}\n");
    let sized = vec![("src/gone.rs".to_string(), 13_u64)];
    let selection = crate::chunk::Selector::new(&[])
        .expect("globs")
        .select(sized);
    assert_eq!(selection.selected, vec!["src/gone.rs".to_string()]);
    checkout.remove("src/gone.rs");

    let report = report(
        rig.indexer()
            .index_paths(REPO, "sha-2", &checkout.root(), &["src/gone.rs".into()])
            .await
            .expect("indexes"),
    );
    assert_eq!(report.files, 0);
}

#[tokio::test]
async fn a_binary_file_on_the_allowlist_is_reported_as_not_text() {
    let checkout = Checkout::new();
    let rig = Rig::new();
    std::fs::write(checkout.root().join("src/blob.rs"), [0xff_u8, 0xfe, 0x00]).expect("write");

    let report = report(
        rig.indexer()
            .index_paths(REPO, "sha-1", &checkout.root(), &["src/blob.rs".into()])
            .await
            .expect("indexes"),
    );
    assert!(
        report
            .skipped
            .iter()
            .any(|s| s.path == "src/blob.rs" && s.reason == crate::chunk::SkipReason::NotText)
    );
}

// --- batching -------------------------------------------------------------
//
// A batch is bounded by two ceilings, and only one of them used to exist.
// These assert the token ceiling and, more importantly, that adding it did not
// cost the partition invariant: every queued chunk is embedded exactly once.

/// A chunk whose text is `chars` bytes long, so its estimated token cost is
/// `chars / 4` and a batch's expected split is arithmetic rather than a guess.
fn sized_chunk(chars: usize) -> Chunk {
    Chunk {
        repo_id: REPO.into(),
        path: "src/lib.rs".into(),
        start_line: 1,
        end_line: 2,
        text: "x".repeat(chars),
        lang: None,
        symbol: None,
        chunked_by: Default::default(),
        content_hash: String::new(),
    }
}

/// `batch_bounds` over `sizes`, with the queue kept alive by the caller.
fn bounds_over(chunks: &[Chunk], max_items: usize, max_tokens: u64) -> Vec<(usize, usize)> {
    let queue: Vec<(usize, &Chunk)> = chunks.iter().map(|chunk| (0_usize, chunk)).collect();
    batch_bounds(&queue, max_items, max_tokens)
}

#[test]
fn the_token_ceiling_splits_a_batch_the_count_ceiling_would_keep_whole() {
    // The production regression, in miniature. Ten chunks is under a count
    // ceiling of 64, so the old `queue.chunks(self.batch)` sent all ten in one
    // call; at 4,000 estimated tokens each that is 40,000 against a ceiling of
    // 12,000, which is exactly the shape the provider rejected with
    // `max_tokens_per_request`.
    let chunks: Vec<Chunk> = (0..10).map(|_| sized_chunk(16_000)).collect();
    let bounds = bounds_over(&chunks, 64, 12_000);
    assert!(
        bounds.len() > 1,
        "a batch over the token ceiling must be split, got {bounds:?}"
    );
    for (start, end) in &bounds {
        let tokens: u64 = chunks[*start..*end]
            .iter()
            .map(|chunk| estimate_tokens(&chunk.text))
            .sum();
        assert!(tokens <= 12_000, "batch {start}..{end} carries {tokens}");
    }
}

#[test]
fn the_real_rejected_batch_now_fits_under_the_default_ceiling() {
    // The measured failure: 64 chunks at the chunker's 14,400-char cap. The
    // provider counted 467,846 real tokens against a 300,000 limit. Under the
    // default ceiling this must split, and each batch must stay under half of
    // 300,000 — the headroom that covers `estimate_tokens` under-counting code
    // by roughly 2x.
    let chunks: Vec<Chunk> = (0..64).map(|_| sized_chunk(14_400)).collect();
    let bounds = bounds_over(&chunks, DEFAULT_BATCH, DEFAULT_MAX_BATCH_TOKENS);
    assert!(bounds.len() > 1, "64 full chunks must not be one call");
    for (start, end) in &bounds {
        let estimated: u64 = chunks[*start..*end]
            .iter()
            .map(|chunk| estimate_tokens(&chunk.text))
            .sum();
        assert!(estimated <= DEFAULT_MAX_BATCH_TOKENS);
        assert!(
            estimated * 2 < 300_000,
            "batch {start}..{end} would be ~{} real tokens",
            estimated * 2
        );
    }
}

#[test]
fn every_chunk_lands_in_exactly_one_batch() {
    // The invariant the split must not cost. A chunk dropped here is a file
    // missing from the index that nothing else would ever report.
    let chunks: Vec<Chunk> = (1..=37).map(|n| sized_chunk(n * 400)).collect();
    let bounds = bounds_over(&chunks, 7, 9_000);

    let mut covered = Vec::new();
    let mut cursor = 0;
    for (start, end) in &bounds {
        assert_eq!(*start, cursor, "batches must be contiguous: {bounds:?}");
        assert!(start < end, "an empty batch would spin: {bounds:?}");
        covered.extend(*start..*end);
        cursor = *end;
    }
    assert_eq!(cursor, chunks.len(), "the tail must be emitted");
    assert_eq!(covered, (0..chunks.len()).collect::<Vec<_>>());
}

#[test]
fn the_count_ceiling_still_binds_when_the_texts_are_small() {
    // Adding the token ceiling must not retire the count one: a thousand tiny
    // chunks in a single call is a different failure, not a fixed one.
    let chunks: Vec<Chunk> = (0..20).map(|_| sized_chunk(8)).collect();
    let bounds = bounds_over(&chunks, 5, 1_000_000);
    assert_eq!(bounds, vec![(0, 5), (5, 10), (10, 15), (15, 20)]);
}

#[test]
fn a_chunk_over_the_ceiling_on_its_own_still_makes_progress() {
    // Forward progress beats correctness of the call here. The provider will
    // reject this one, but a batch that can never close would hang the index
    // and a skipped chunk would vanish silently.
    let chunks = vec![sized_chunk(80), sized_chunk(400_000), sized_chunk(80)];
    let bounds = bounds_over(&chunks, 64, 1_000);
    assert_eq!(bounds, vec![(0, 1), (1, 2), (2, 3)]);
}

#[test]
fn an_empty_queue_makes_no_calls() {
    assert!(bounds_over(&[], 64, 120_000).is_empty());
}

#[test]
fn a_zero_token_ceiling_is_read_as_the_default_not_as_unbounded() {
    // A config typo must not switch the ceiling off — that is the bug.
    let indexer = Rig::new();
    let built = indexer.indexer().with_max_batch_tokens(0);
    assert_eq!(built.max_batch_tokens, DEFAULT_MAX_BATCH_TOKENS);
}
