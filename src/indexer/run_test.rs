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
        .with_selector(crate::chunk::Selector::new(&[]).expect("globs").max_bytes(500));
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
    assert_eq!(embedder.calls(), 0, "the ceiling is checked before spending");

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
    let selection = crate::chunk::Selector::new(&[]).expect("globs").select(sized);
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
