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

#[tokio::test]
#[ignore]
async fn rekey_oc2313_cassette_after_a_shared_rules_edit() {
    use std::sync::Arc;
    use crate::harness::cassette::{Cassette, Mode};

    let corpus = load(&corpus_root()).expect("loads");
    let case_id = "oc-2313-round-boundary-leaks".to_string();
    let corpus = corpus.select(&[case_id.clone()]).expect("known case");
    let dir = corpus.cases[0].cassette_dir(&corpus.root);

    // Load every old take into memory before the directory is cleared, and
    // serve them back in call order: the prompt text changed by one added
    // sentence in the cacheable prefix, not the review logic, so the same
    // calls happen in the same order and the same answers apply.
    let replay = Cassette::replay(&dir, Mode::Loose).expect("old cassette loads");
    std::fs::remove_dir_all(&dir).expect("clear stale cassette");

    let out = tempfile::tempdir().expect("tempdir");
    let outcome = run(
        &corpus,
        &config(),
        Some(Arc::new(replay) as Arc<dyn crate::ports::model::Model>),
        &crate::eval::RunOptions {
            out: out.path().to_path_buf(),
            record: true,
            loose: false,
            record_prompts: false,
            max_cost_usd: 1000.0,
            tree: None,
        },
    )
    .await
    .expect("re-records");

    for score in &outcome.scores {
        assert!(score.error.is_none(), "{:?}", score.error);
    }
}

#[tokio::test]
#[ignore]
async fn rekey_ts0045_cassette_after_a_shared_rules_edit() {
    use std::sync::Arc;
    use crate::harness::cassette::{Cassette, Mode};

    let corpus = load(&corpus_root()).expect("loads");
    let case_id = "ts-0045-kernel-bypass-hallucination".to_string();
    let corpus = corpus.select(&[case_id.clone()]).expect("known case");
    let dir = corpus.cases[0].cassette_dir(&corpus.root);

    let replay = Cassette::replay(&dir, Mode::Loose).expect("old cassette loads");
    std::fs::remove_dir_all(&dir).expect("clear stale cassette");

    let out = tempfile::tempdir().expect("tempdir");
    let outcome = run(
        &corpus,
        &config(),
        Some(Arc::new(replay) as Arc<dyn crate::ports::model::Model>),
        &crate::eval::RunOptions {
            out: out.path().to_path_buf(),
            record: true,
            loose: false,
            record_prompts: false,
            max_cost_usd: 1000.0,
            tree: None,
        },
    )
    .await
    .expect("re-records");

    for score in &outcome.scores {
        assert!(score.error.is_none(), "{:?}", score.error);
    }
}

#[tokio::test]
#[ignore]
async fn rekey_ts0068_cassette_after_a_shared_rules_edit() {
    use std::sync::Arc;
    use crate::harness::cassette::{Cassette, Mode};

    let corpus = load(&corpus_root()).expect("loads");
    let case_id = "ts-0068-description-anchored-to-code".to_string();
    let corpus = corpus.select(&[case_id.clone()]).expect("known case");
    let dir = corpus.cases[0].cassette_dir(&corpus.root);

    let replay = Cassette::replay(&dir, Mode::Loose).expect("old cassette loads");
    std::fs::remove_dir_all(&dir).expect("clear stale cassette");

    let out = tempfile::tempdir().expect("tempdir");
    let outcome = run(
        &corpus,
        &config(),
        Some(Arc::new(replay) as Arc<dyn crate::ports::model::Model>),
        &crate::eval::RunOptions {
            out: out.path().to_path_buf(),
            record: true,
            loose: false,
            record_prompts: false,
            max_cost_usd: 1000.0,
            tree: None,
        },
    )
    .await
    .expect("re-records");

    for score in &outcome.scores {
        assert!(score.error.is_none(), "{:?}", score.error);
    }
}
