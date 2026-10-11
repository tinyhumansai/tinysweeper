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
        lookups: Default::default(),
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
            merge: false,
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

    // The per-PR ceiling decides whether a case fails with `Error::Budget`, so
    // two runs that spend the same model but allow different money are not the
    // same run either.
    let mut other_budget = base.clone();
    other_budget.models.budget_usd_per_pr = 0.5;
    assert_ne!(digest_of(&base), digest_of(&other_budget));

    // A route changes the ceiling and the endpoint one tier is served at,
    // without changing the tier's name.
    let mut routed = base.clone();
    routed.models.routes.push(crate::config::types::ModelRoute {
        model: base.models.deep.clone(),
        order: vec![],
        allow_fallbacks: true,
        max_tokens: Some(0),
    });
    assert_ne!(
        digest_of(&base),
        digest_of(&routed),
        "a route's ceiling decides whether a case completes or truncates"
    );
    let mut repinned = routed.clone();
    repinned.models.routes[0].order = vec!["openai/flex".into()];
    assert_ne!(digest_of(&routed), digest_of(&repinned));

    let mut fewer_rounds = base.clone();
    fewer_rounds.lookup.rounds = 0;
    assert_ne!(
        digest_of(&base),
        digest_of(&fewer_rounds),
        "the lookup policy decides what the reviewer sees"
    );

    // A path instruction's selectors decide which prompt is built even when
    // the instruction text is identical: `lanes` gates which lanes get the
    // injected instructions at all, and `rules` names the document inside them.
    let mut with_instruction = base.clone();
    with_instruction
        .path_instructions
        .push(crate::config::types::PathInstruction {
            glob: "**/*.rs".into(),
            instructions: "Flag unchecked index operations.".into(),
            rules: None,
            lanes: vec![],
            merge: false,
        });
    assert_eq!(digest_of(&with_instruction), digest_of(&with_instruction));
    let mut lanes_gated = with_instruction.clone();
    lanes_gated.path_instructions[0].lanes = vec![crate::config::types::LaneId::Security];
    assert_ne!(digest_of(&with_instruction), digest_of(&lanes_gated));
    let mut rules_named = with_instruction.clone();
    rules_named.path_instructions[0].rules = Some("rust".into());
    assert_ne!(digest_of(&with_instruction), digest_of(&rules_named));
    let mut merged = with_instruction.clone();
    merged.path_instructions[0].merge = true;
    assert_ne!(
        digest_of(&with_instruction),
        digest_of(&merged),
        "merge decides whether the next matching entry renders too"
    );

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
async fn a_tree_option_with_more_than_one_case_is_a_config_error() {
    // `--tree` names one checkout on disk; handing it to every case in a
    // multi-case run would feed the same tree's lookups into unrelated
    // fixtures. This must be refused before any case is touched, not
    // discovered later as corrupted cassettes.
    let dir = corpus_dir(EXPECTATION);
    std::fs::write(dir.path().join("cases/ts-0002.toml"), {
        let mut text = case_toml(EXPECTATION);
        text = text.replace("ts-0001", "ts-0002");
        text
    })
    .expect("write");
    std::fs::write(
        dir.path().join("fixtures/ts-0002.json"),
        serde_json::to_string_pretty(&fixture()).expect("serializes"),
    )
    .expect("write");
    let corpus = load(dir.path()).expect("loads");
    assert_eq!(corpus.cases.len(), 2);

    let err = run(
        &corpus,
        &config(),
        None,
        &RunOptions {
            out: dir.path().join("runs/test"),
            tree: Some(dir.path().to_path_buf()),
            ..RunOptions::default()
        },
    )
    .await
    .expect_err("--tree with more than one case must be refused");

    assert!(err.to_string().contains("--tree"), "{err}");
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

#[test]
fn agentic_review_and_price_bounds_change_the_evaluation_digest() {
    let base = config();
    let mut agentic = base.clone();
    agentic.models.agentic_reviewers = true;
    assert_ne!(digest_of(&base), digest_of(&agentic));
    let mut bounded = base.clone();
    bounded.models.budget_prices.insert(
        "flash".into(),
        crate::config::types::BudgetPriceBound {
            input: 1.0,
            cached: 0.1,
            output: 2.0,
        },
    );
    assert_ne!(digest_of(&base), digest_of(&bounded));
}

#[tokio::test]
async fn a_case_failing_after_paid_calls_reports_the_known_model_charge() {
    let dir = corpus_dir(EXPECTATION);
    let mut corpus = load(dir.path()).unwrap();
    corpus.cases[0].case.budget.max_cost_usd = 0.005;
    let mut config = config();
    config.models.budget_usd_per_pr = 0.000001;
    let model = Arc::new(MockModel::silent().with_usage(crate::ports::model::Usage {
        input_tokens: 100,
        output_tokens: 10,
        cached_tokens: 20,
        cost_usd: 0.01,
        ..Default::default()
    }));
    let result = run(
        &corpus,
        &config,
        Some(model.clone()),
        &RunOptions {
            out: dir.path().join("runs/test"),
            record: true,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let score = &result.scores[0];
    assert!(
        score.error.is_some(),
        "The budget failure remains a failure: {score:?}"
    );
    assert!(
        score.cost_usd >= 0.01,
        "The paid response cannot be reported as free: {score:?}"
    );
    assert!(score.over_budget);
    assert!(score.input_tokens >= 100);
    assert!(score.output_tokens >= 10);
    assert!(score.cached_tokens >= 20);
    assert!(!score.models.is_empty());
    let rescored = rescore(&corpus, &dir.path().join("runs/test")).unwrap();
    assert_eq!(rescored[0].cost_usd, score.cost_usd);
    assert_eq!(rescored[0].over_budget, score.over_budget);
    assert_eq!(rescored[0].input_tokens, score.input_tokens);
    assert_eq!(rescored[0].output_tokens, score.output_tokens);
    assert_eq!(rescored[0].cached_tokens, score.cached_tokens);
    assert_eq!(rescored[0].models, score.models);
    assert_eq!(rescored[0].error, score.error);
    assert_eq!(rescored[0].wall_secs, score.wall_secs);
}

#[tokio::test]
async fn malformed_description_preserves_other_lanes_and_holds_approval() {
    use crate::ports::model::{ModelRequest, ModelResponse, Usage};
    struct PartialReviewer;
    #[async_trait::async_trait]
    impl Model for PartialReviewer {
        async fn complete(&self, request: ModelRequest) -> Result<ModelResponse> {
            let value = match request.schema_name.as_str() {
                "tinysweeper_description" => {
                    json!({"summary": "Malformed", "findings": [{"path": "src/lib.rs"}]})
                }
                "tinysweeper_falsify" => json!({"incorrect": []}),
                _ => json!({"summary": "An advisory finding.", "findings": [{
                    "path": "src/lib.rs", "existing_code": "items[0]", "rule": "unchecked-index",
                    "title": "Guard the index", "body": "This panics on an empty slice.",
                    "severity": "medium", "confidence": 0.7
                }]}),
            };
            Ok(ModelResponse {
                value,
                model: "observed-model".into(),
                usage: Usage {
                    input_tokens: 100,
                    output_tokens: 10,
                    cost_usd: 0.001,
                    ..Default::default()
                },
            })
        }
    }
    let dir = corpus_dir(EXPECTATION);
    let mut corpus = load(dir.path()).unwrap();
    corpus.cases[0].case.lanes = vec!["critique".into(), "description".into()];
    corpus.cases[0].fixture.pull_request.body =
        "Adds a complete deployment and lookup change with supporting context.".into();
    let out = dir.path().join("runs/partial");
    let config = config();
    let result = run(
        &corpus,
        &config,
        Some(Arc::new(PartialReviewer)),
        &RunOptions {
            out: out.clone(),
            record: true,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert!(
        result.scores[0].error.is_none(),
        "A malformed lane must not discard the review"
    );
    let proposal: Proposal =
        serde_json::from_slice(&std::fs::read(out.join("ts-0001/proposal.json")).unwrap()).unwrap();
    assert!(!proposal.complete());
    assert!(proposal.usage().cost_usd >= 0.002);
    assert!(
        serde_json::to_string(&proposal)
            .unwrap()
            .contains("This panics on an empty slice")
    );
    let fixture = &corpus.cases[0].fixture;
    let forge = crate::forge::mock::MockForge::new().with_pull_request(
        fixture.pull_request.clone(),
        fixture.files.clone(),
        fixture.commits.clone(),
    );
    crate::app::apply::apply(&forge, &forge, &config, &proposal, None)
        .await
        .unwrap();
    assert!(
        !forge.writes().iter().any(|write| matches!(
            write,
            crate::forge::mock::Write::Review {
                event: crate::forge::types::ReviewEvent::Approve,
                ..
            }
        )),
        "An unanswered lane cannot approve, even with only advisory findings"
    );
}

#[tokio::test]
async fn failed_case_marks_unreported_model_charges_unknown() {
    let dir = corpus_dir(EXPECTATION);
    let corpus = load(dir.path()).unwrap();
    let cassette = Cassette::record(
        Arc::new(MockModel::new().then_error("provider refused without usage")),
        dir.path().join("cassettes"),
    );
    let request = crate::ports::model::ModelRequest {
        model: "requested".into(),
        messages: vec![],
        schema: json!({}),
        schema_name: "test".into(),
        max_tokens: 10,
    };
    assert!(cassette.complete(request).await.is_err());
    let out = dir.path().join("run");
    let score = persist_failed_case(
        &corpus.cases[0],
        &out,
        "review failed".into(),
        std::time::Duration::from_secs(1),
        &cassette,
    )
    .unwrap();
    assert_eq!(score.cost_usd, 0.0, "Do not invent an unknown charge");
    assert!(
        score
            .error
            .as_deref()
            .unwrap()
            .contains("charges for model calls without reported usage are unknown")
    );
    assert_eq!(rescore(&corpus, &out).unwrap()[0].error, score.error);
}

#[test]
fn legacy_failure_markers_stay_failed_when_rescored() {
    let dir = corpus_dir(EXPECTATION);
    let corpus = load(dir.path()).unwrap();
    let out = dir.path().join("runs/legacy");
    std::fs::create_dir_all(out.join("ts-0001")).unwrap();
    std::fs::write(
        out.join("ts-0001/failure.json"),
        serde_json::to_string("old failure").unwrap(),
    )
    .unwrap();
    let score = rescore(&corpus, &out).unwrap().remove(0);
    assert_eq!(score.error.as_deref(), Some("old failure"));
    assert_eq!(score.missed, vec!["E1"]);
    assert_eq!(score.cost_usd, 0.0);
}

#[tokio::test]
async fn missing_cassette_supersedes_a_previous_success_when_rescored() {
    let (dir, _) = record_then_replay(EXPECTATION, finder()).await;
    let corpus = load(dir.path()).unwrap();
    let out = dir.path().join("runs/test");
    assert!(out.join("ts-0001/proposal.json").exists());
    std::fs::remove_dir_all(dir.path().join("cassettes")).unwrap();
    let outcome = run(
        &corpus,
        &config(),
        None,
        &RunOptions {
            out: out.clone(),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert!(outcome.scores[0].error.is_some());
    let rescored = rescore(&corpus, &out).unwrap();
    assert_eq!(
        rescored[0].error, outcome.scores[0].error,
        "A current failure cannot rescore the stale successful proposal"
    );
    assert!(!out.join("ts-0001/proposal.json").exists());
    assert_eq!(rescored[0].cost_usd, 0.0, "No call was dispatched");
}
