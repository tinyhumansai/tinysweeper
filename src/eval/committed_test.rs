//! The committed corpus, replayed offline.
//!
//! This is the test that catches "somebody changed a prompt and never
//! re-recorded". It runs the real engine over the real `evals/` corpus against
//! the committed cassettes — no key, no network, free — and asserts the two
//! regressions the corpus exists to guard are still fixed.
//!
//! When it fails on a cassette miss, the corpus is stale rather than the code
//! being wrong. Re-record with `tinysweeper eval run --record`.

use std::path::PathBuf;

use crate::eval::{RunOptions, load, run};

/// The repository's own corpus, found relative to the crate root.
fn corpus_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("evals")
}

fn config() -> crate::config::types::Config {
    crate::config::DEFAULTS
        .parse::<toml::Table>()
        .unwrap()
        .try_into()
        .unwrap()
}

#[tokio::test]
async fn the_committed_corpus_replays_and_holds_its_regressions() {
    let corpus = load(&corpus_root()).expect("the committed corpus loads");
    assert!(!corpus.cases.is_empty(), "the corpus is empty");

    let out = tempfile::tempdir().expect("tempdir");
    let outcome = run(
        &corpus,
        &config(),
        // No model at all: anything that reached for one would fail here rather
        // than quietly spend money in a unit test.
        None,
        &RunOptions {
            out: out.path().to_path_buf(),
            ..RunOptions::default()
        },
    )
    .await
    .expect("replays");

    for score in &outcome.scores {
        assert!(
            score.error.is_none(),
            "`{}` did not replay: {}\n\nThe corpus is stale against the prompts in this tree. \
             Re-record it with `tinysweeper eval run --record`.",
            score.id,
            score.error.as_deref().unwrap_or_default()
        );
        // The whole point of both cases: issue #47's hallucinated
        // hardware-access claim and PR #72's description finding on a code line
        // must both stay gone.
        assert!(
            score.forbidden_hits.is_empty(),
            "`{}` said something the corpus forbids: {:?}",
            score.id,
            score.forbidden_hits
        );
    }

    assert_eq!(
        outcome.loose_replays, 0,
        "answers were served by call order, so these numbers describe a prompt that is not \
         in this tree"
    );
}
