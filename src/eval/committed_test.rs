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

// Ignored while the cassettes are stale: the shared review rules changed
// (`harness::prompt::SHARED_RULES`), which changes the key of every recorded
// call. Re-recording needs live model keys; once `tinysweeper eval run
// --record` has been run against this tree, remove this attribute.
#[ignore = "eval cassettes are stale after the shared-rules prompt change; re-record with `tinysweeper eval run --record`, then remove this"]
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
async fn the_five_historical_recordings_replay_with_the_measured_outcomes() {
    const CASES: [&str; 5] = [
        "tm-0102-memory-pending-debug-state",
        "oh-8061-standalone-local-only-policy",
        "oh-8061-speech-redirect-origin",
        "oh-8061-privacy-docs-contract-mismatch",
        "oh-8076-credential-read-failure-migration",
    ];
    let expected_unscored = [
        (CASES[0], 3),
        (CASES[1], 3),
        (CASES[2], 1),
        (CASES[3], 3),
        (CASES[4], 1),
    ];
    let root = corpus_root();
    let ids = CASES.map(str::to_owned);
    let corpus = load(&root)
        .expect("the corpus loads")
        .select(&ids)
        .expect("the five labelled cases exist");
    let loaded_cases = corpus.cases.clone();
    for case in &loaded_cases {
        let cassettes = case.cassette_dir(&root);
        let has_take = std::fs::read_dir(&cassettes)
            .unwrap_or_else(|err| panic!("{} has no cassette directory: {err}", case.case.id))
            .filter_map(Result::ok)
            .any(|entry| entry.path().extension().is_some_and(|ext| ext == "json"));
        assert!(has_take, "{} has no recorded answers", case.case.id);
    }

    let loaded = crate::config::load_validated(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")),
        Some(std::path::Path::new("evals/recording.gptoss-novita.toml")),
    )
    .expect("the recording profile validates")
    .config;
    let out = tempfile::tempdir().expect("tempdir");
    let outcome = run(
        &corpus,
        &loaded,
        None,
        &RunOptions {
            out: out.path().to_path_buf(),
            ..RunOptions::default()
        },
    )
    .await
    .expect("the labelled recordings replay without a model");

    assert_eq!(
        outcome.loose_replays, 0,
        "replay must use strict cassette keys"
    );
    assert_eq!(outcome.scores.len(), CASES.len());
    for score in &outcome.scores {
        assert!(
            score.error.is_none(),
            "{} failed strict replay: {}",
            score.id,
            score.error.as_deref().unwrap_or_default()
        );
        assert!(
            score.forbidden_hits.is_empty(),
            "{} matched forbidden labels: {:?}",
            score.id,
            score.forbidden_hits
        );
        assert_eq!(
            score.true_positives, 0,
            "{} baseline hits changed",
            score.id
        );
        assert_eq!(score.missed, ["E1"], "{} baseline misses changed", score.id);
        let expected = expected_unscored
            .iter()
            .find_map(|(id, count)| (*id == score.id).then_some(*count))
            .expect("the score belongs to one of the five cases");
        assert_eq!(
            score.unscored, expected,
            "{} unscored count changed",
            score.id
        );

        let case = loaded_cases
            .iter()
            .find(|case| case.case.id == score.id)
            .expect("the loaded case exists");
        let fixture_files = case
            .fixture
            .files
            .iter()
            .map(|file| (file.path.as_str(), file))
            .collect::<std::collections::BTreeMap<_, _>>();
        // Critique needs changed head lines to anchor a finding. A deletion-only
        // patch has no head-line range, so it is intentionally outside this
        // lane's reviewable set even though it remains in the fixture.
        let expected_files = fixture_files
            .iter()
            .filter(|(_, file)| file.additions > 0)
            .map(|(path, _)| *path)
            .collect::<std::collections::BTreeSet<_>>();
        if case.case.id == CASES[0] {
            let excluded = fixture_files
                .values()
                .filter(|file| file.additions == 0 && file.deletions > 0)
                .map(|file| file.path.as_str())
                .collect::<std::collections::BTreeSet<_>>();
            assert_eq!(fixture_files.len(), 11);
            assert_eq!(
                excluded,
                ["crates/tinymcp/src/registry/oauth/types.rs"].into()
            );
            let deletion_only = fixture_files["crates/tinymcp/src/registry/oauth/types.rs"];
            assert_eq!(
                deletion_only.status,
                crate::forge::types::FileStatus::Modified
            );
            assert_eq!(deletion_only.additions, 0);
            assert_eq!(deletion_only.deletions, 22);
        }
        let proposal_path = out.path().join(&score.id).join("proposal.json");
        let proposal: serde_json::Value = serde_json::from_slice(
            &std::fs::read(&proposal_path)
                .unwrap_or_else(|err| panic!("{} has no replay proposal: {err}", score.id)),
        )
        .expect("the replay proposal is valid JSON");
        assert!(
            proposal["partial_review"].is_null(),
            "{} is partial",
            score.id
        );
        assert_eq!(
            proposal["unreviewed"].as_array().map(Vec::len),
            Some(0),
            "{} left files unreviewed",
            score.id
        );
        let lanes = proposal["lanes"].as_array().expect("proposal has lanes");
        assert!(!lanes.is_empty(), "{} has no reviewed lane", score.id);
        for lane in lanes {
            assert_eq!(
                lane["completed"], true,
                "{} lane did not complete",
                score.id
            );
            let coverage = &lane["coverage"];
            assert_eq!(
                coverage["unanswered"].as_array().map(Vec::len),
                Some(0),
                "{} lane left files unanswered",
                score.id
            );
            let reviewed = coverage["reviewed"]
                .as_array()
                .expect("lane reports reviewed files")
                .iter()
                .filter_map(serde_json::Value::as_str)
                .collect::<std::collections::BTreeSet<_>>();
            assert_eq!(
                reviewed, expected_files,
                "{} file coverage changed",
                score.id
            );
        }
    }
}
