//! Running a corpus, offline, end to end.
//!
//! These build a real corpus in a temp directory, record a cassette against
//! `MockModel`, then replay and score it — which is the same code path a live
//! run takes, minus the key. It is what catches "somebody changed a prompt and
//! never re-recorded".

use std::sync::Arc;

use serde_json::json;

use super::*;
use crate::config::types::Config;
use crate::eval::corpus::load;
use crate::eval::types::Fixture;
use crate::forge::types::{ChangedFile, FileStatus, PullRequest};
use crate::harness::mock::MockModel;

/// A one-file pull request with a real patch, so anchoring has work to do.
fn fixture() -> Fixture {
    Fixture {
        pull_request: PullRequest {
            number: 7,
            title: "feat: index into the slice".into(),
            body: "Adds a lookup.".into(),
            head_sha: "b".repeat(40),
            base_sha: "a".repeat(40),
            base_ref: "main".into(),
            head_ref: "feature".into(),
            ..Default::default()
        },
        files: vec![ChangedFile {
            path: "src/lib.rs".into(),
            previous_path: None,
            status: FileStatus::Modified,
            additions: 2,
            deletions: 0,
            patch: Some(
                "@@ -1,2 +1,4 @@\n fn head(items: &[u8]) -> u8 {\n+    // look it up\n+    items[0]\n }\n"
                    .into(),
            ),
            size_bytes: Some(120),
        }],
        commits: vec![],
        comments: vec![],
        blobs: Default::default(),
    }
}

fn case_toml(expectation: &str) -> String {
    format!(
        r#"
schema = 1
id = "ts-0001"
fixture = "../fixtures/ts-0001.json"
lanes = ["critique"]

[provenance]
repo = "tinyhumansai/tinysweeper"
pr = 7
evidence = "https://github.com/tinyhumansai/tinysweeper/pull/8"
labelled_by = "tester"
{expectation}
"#
    )
}

/// Write a one-case corpus and return its directory.
fn corpus_dir(expectation: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(dir.path().join("cases")).expect("mkdir");
    std::fs::create_dir_all(dir.path().join("fixtures")).expect("mkdir");
    std::fs::write(
        dir.path().join("cases/ts-0001.toml"),
        case_toml(expectation),
    )
    .expect("write");
    std::fs::write(
        dir.path().join("fixtures/ts-0001.json"),
        serde_json::to_string_pretty(&fixture()).expect("serializes"),
    )
    .expect("write");
    dir
}

fn config() -> Config {
    crate::config::DEFAULTS
        .parse::<toml::Table>()
        .unwrap()
        .try_into()
        .unwrap()
}

/// A model that reports the real defect on the added line.
fn finder() -> Arc<MockModel> {
    Arc::new(MockModel::always(json!({
        "summary": "One thing to fix.",
        "findings": [{
            "path": "src/lib.rs",
            "existing_code": "items[0]",
            "rule": "unchecked-index",
            "title": "Guard the index before dereferencing",
            "body": "`items[0]` panics when the slice is empty.",
            "severity": "high",
            "confidence": 0.9
        }],
        "rules": [],
        "rejected": []
    })))
}

const EXPECTATION: &str = r#"
[[expected]]
id = "E1"
path = "src/lib.rs"
lines = [3, 3]
summary = "Indexing a possibly-empty slice panics."
must_mention = ["panic|empty"]
"#;

async fn record_then_replay(
    expectation: &str,
    model: Arc<MockModel>,
) -> (tempfile::TempDir, RunOutcome) {
    let dir = corpus_dir(expectation);
    let out = dir.path().join("runs/test");
    let corpus = load(dir.path()).expect("loads");
    let config = config();

    let recording = RunOptions {
        out: out.clone(),
        record: true,
        ..RunOptions::default()
    };
    run(&corpus, &config, Some(model), &recording)
        .await
        .expect("records");

    let replaying = RunOptions {
        out,
        ..RunOptions::default()
    };
    // No model at all on the replay path: if anything reached for one, this
    // would panic rather than quietly spend money.
    let outcome = run(&corpus, &config, None, &replaying)
        .await
        .expect("replays");
    (dir, outcome)
}

#[tokio::test]
async fn a_recorded_case_replays_offline_and_scores_the_same() {
    let (_dir, outcome) = record_then_replay(EXPECTATION, finder()).await;

    assert_eq!(outcome.scores.len(), 1);
    let score = &outcome.scores[0];
    assert_eq!(score.true_positives, 1, "{:?}", score.judged);
    assert!(score.missed.is_empty());
    assert_eq!(score.false_positives, 0);
    // Strict replay: nothing fell back to call order, so these numbers describe
    // the prompts that are actually in the tree.
    assert_eq!(outcome.loose_replays, 0);
    assert!(score.error.is_none(), "{:?}", score.error);
}

#[tokio::test]
async fn the_run_writes_a_proposal_a_later_score_can_read() {
    let (dir, _) = record_then_replay(EXPECTATION, finder()).await;
    let path = dir.path().join("runs/test/ts-0001/proposal.json");

    let raw = std::fs::read_to_string(&path).expect("written");
    let proposal: Proposal = serde_json::from_str(&raw).expect("round-trips");
    assert_eq!(proposal.number, 7);
    assert_eq!(proposal.head_sha, "b".repeat(40));
}

#[tokio::test]
async fn a_case_the_reviewer_says_nothing_about_is_scored_as_a_miss() {
    let silent = Arc::new(MockModel::always(
        json!({"summary": "Nothing to report.", "findings": [], "rules": [], "rejected": []}),
    ));
    let (_dir, outcome) = record_then_replay(EXPECTATION, silent).await;

    let score = &outcome.scores[0];
    assert_eq!(score.true_positives, 0);
    assert_eq!(score.missed, ["E1"]);
}

#[tokio::test]
async fn a_stale_cassette_fails_the_case_rather_than_scoring_an_old_prompt() {
    let dir = corpus_dir(EXPECTATION);
    let corpus = load(dir.path()).expect("loads");
    let out = dir.path().join("runs/test");

    run(
        &corpus,
        &config(),
        Some(finder()),
        &RunOptions {
            out: out.clone(),
            record: true,
            ..RunOptions::default()
        },
    )
    .await
    .expect("records");

    // Simulate the realistic prompt edit: somebody changes a rule document.
    // `path_instructions` is inlined into the prompt, so the bytes the model
    // sees move — which is exactly what must invalidate the recording.
    //
    // Note `strictness` deliberately would *not* do this: it moves
    // `severity_gate` and `confidence_min`, which filter findings after the
    // call, and the prompt is byte-identical either way.
    let mut edited = config();
    edited
        .path_instructions
        .push(crate::config::types::PathInstruction {
            glob: "**/*.rs".into(),
            instructions: "Flag any index into a slice without a bounds check.".into(),
            rules: None,
            lanes: vec![],
        });

    let outcome = run(
        &corpus,
        &edited,
        None,
        &RunOptions {
            out: out.clone(),
            ..RunOptions::default()
        },
    )
    .await
    .expect("runs");

    // Scored as a failure, loudly, rather than silently replaying answers to a
    // question nobody asked. The lane worked around the miss and reported
    // "could not be reviewed", so the runner must convert that into a failed
    // case — which is what `strict_misses` on the cassette makes possible.
    let score = &outcome.scores[0];
    assert!(score.error.is_some(), "expected a cassette miss");
    assert!(
        score
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("re-record"),
        "{:?}",
        score.error
    );
    assert_eq!(score.missed, ["E1"], "a failed review found nothing");

    // The failure must be durable, or a later `eval score` would silently
    // re-score the stale replay as a normal result the run never produced.
    assert!(
        !out.join("ts-0001/proposal.json").exists(),
        "a stale replay must not leave a rescoreable proposal behind"
    );
    let rescored = rescore(&corpus, &out).expect("rescoring is free");
    let error = rescored[0].error.as_deref().expect("must stay failed");
    assert!(error.contains("re-record"), "{error}");
}

#[tokio::test]
async fn the_config_digest_moves_when_the_prompt_inputs_move() {
    let base = config();
    let mut stricter = base.clone();
    stricter.review.strictness = 3;

    // Comparing a strictness-3 run against a strictness-2 baseline and reading
    // the difference as a prompt improvement is the mistake the digest exists
    // to make impossible.
    assert_ne!(digest_of(&base), digest_of(&stricter));

    let mut other_model = base.clone();
    other_model.models.deep = "deepseek/deepseek-v4-pro".into();
    assert_ne!(digest_of(&base), digest_of(&other_model));

    // A path instruction's selectors decide which prompt is built even when
    // the instruction text is identical: `lanes` gates which lanes get the
    // injected instructions at all, and `rules` names the document inside them.
    let mut with_instruction = base.clone();
    with_instruction.path_instructions.push(
        crate::config::types::PathInstruction {
            glob: "**/*.rs".into(),
            instructions: "Flag unchecked index operations.".into(),
            rules: None,
            lanes: vec![],
        },
    );
    assert_eq!(
        digest_of(&with_instruction),
        digest_of(&with_instruction)
    );
    let mut lanes_gated = with_instruction.clone();
    lanes_gated.path_instructions[0].lanes = vec![crate::config::types::LaneId::Security];
    assert_ne!(digest_of(&with_instruction), digest_of(&lanes_gated));
    let mut rules_named = with_instruction.clone();
    rules_named.path_instructions[0].rules = Some("rust".into());
    assert_ne!(digest_of(&with_instruction), digest_of(&rules_named));

    assert_eq!(digest_of(&base), digest_of(&config()));
}

#[tokio::test]
async fn incremental_review_is_forced_off_however_the_config_arrived() {
    // Suppression and cross-push dedupe make findings depend on what the last
    // run saw. A corpus run with this left on measures run order and reports it
    // as review quality.
    let mut incremental = config();
    incremental.review.incremental = true;

    let dir = corpus_dir(EXPECTATION);
    let corpus = load(dir.path()).expect("loads");
    let out = dir.path().join("runs/test");

    run(
        &corpus,
        &incremental,
        Some(finder()),
        &RunOptions {
            out: out.clone(),
            record: true,
            ..RunOptions::default()
        },
    )
    .await
    .expect("records");

    // Running twice must give the same answer. With `incremental` honoured, the
    // second run would suppress what the first posted and score zero.
    let replay = RunOptions {
        out,
        ..RunOptions::default()
    };
    let first = run(&corpus, &incremental, None, &replay)
        .await
        .expect("runs");
    let second = run(&corpus, &incremental, None, &replay)
        .await
        .expect("runs");

    assert_eq!(first.scores[0].true_positives, 1);
    assert_eq!(
        first.scores[0].true_positives,
        second.scores[0].true_positives
    );
}

#[tokio::test]
async fn rescore_covers_the_three_things_a_proposal_can_be() {
    // Three cases sharing one fixture: one with a valid proposal, one whose
    // proposal is garbage, one that never ran. `rescore` is the loop people
    // iterate in, so each of the three must fail loudly — a corpus that
    // silently scored fewer cases than it holds reports the wrong recall in
    // the flattering direction.
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(dir.path().join("cases")).expect("mkdir");
    std::fs::create_dir_all(dir.path().join("fixtures")).expect("mkdir");
    std::fs::write(
        dir.path().join("fixtures/ts-0001.json"),
        serde_json::to_string_pretty(&fixture()).expect("serializes"),
    )
    .expect("write");
    for id in ["ts-ok", "ts-garbage", "ts-missing"] {
        std::fs::write(
            dir.path().join(format!("cases/{id}.toml")),
            format!(
                r#"
schema = 1
id = "{id}"
fixture = "../fixtures/ts-0001.json"
lanes = ["critique"]

[provenance]
repo = "tinyhumansai/tinysweeper"
pr = 7
evidence = "https://github.com/tinyhumansai/tinysweeper/pull/8"
labelled_by = "tester"
{EXPECTATION}
"#
            ),
        )
        .expect("write");
    }

    let corpus = load(dir.path()).expect("loads");
    let out = dir.path().join("runs/test");

    // Record once so every case has a real proposal on disk…
    run(
        &corpus,
        &config(),
        Some(finder()),
        &RunOptions {
            out: out.clone(),
            record: true,
            ..RunOptions::default()
        },
    )
    .await
    .expect("records");

    // …then make two of them lie about the run.
    std::fs::write(
        out.join("ts-garbage/proposal.json"),
        "not a proposal at all",
    )
    .expect("write");
    std::fs::remove_file(out.join("ts-missing/proposal.json")).expect("remove");

    let scores = rescore(&corpus, &out).expect("rescoring is free");
    assert_eq!(scores.len(), 3);
    let by_id: std::collections::HashMap<_, _> = scores
        .iter()
        .map(|score| (score.id.clone(), score))
        .collect();

    // The valid one scores exactly as it did live.
    let ok = by_id["ts-ok"];
    assert_eq!(ok.true_positives, 1, "{:?}", ok.judged);
    assert!(ok.error.is_none(), "{:?}", ok.error);

    // The garbage one is a loud failure naming the file, not a silent skip.
    let garbage = by_id["ts-garbage"];
    assert!(
        garbage
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("not a proposal"),
        "{:?}",
        garbage.error
    );

    // The one that never ran says how to make it run.
    let missing = by_id["ts-missing"];
    assert!(
        missing
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("eval run"),
        "{:?}",
        missing.error
    );
}

#[tokio::test]
async fn the_corpus_ceiling_stops_the_run_rather_than_the_bill() {
    let dir = corpus_dir(EXPECTATION);
    let corpus = load(dir.path()).expect("loads");
    let out = dir.path().join("runs/test");

    // Zero dollars available: the first case is skipped before it is reviewed.
    let outcome = run(
        &corpus,
        &config(),
        Some(finder()),
        &RunOptions {
            out,
            record: true,
            max_cost_usd: 0.0,
            ..RunOptions::default()
        },
    )
    .await
    .expect("runs");

    assert!(outcome.scores.is_empty());
    assert_eq!(outcome.skipped, ["ts-0001"]);
}

#[tokio::test]
async fn recording_without_a_model_says_which_feature_is_missing() {
    let dir = corpus_dir(EXPECTATION);
    let corpus = load(dir.path()).expect("loads");

    let err = run(
        &corpus,
        &config(),
        None,
        &RunOptions {
            out: dir.path().join("runs/test"),
            record: true,
            ..RunOptions::default()
        },
    )
    .await
    .expect_err("cannot record with no model");

    assert!(err.to_string().contains("--features harness"), "{err}");
}
