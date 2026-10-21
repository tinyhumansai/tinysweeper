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

fn cassette_request(prompt: &str) -> crate::ports::model::ModelRequest {
    use crate::ports::model::{Message, ModelRequest};

    ModelRequest {
        model: "test/review-only".into(),
        messages: vec![Message::user(prompt)],
        schema: serde_json::json!({"type": "object"}),
        schema_name: "test_response".into(),
        reasoning_effort: None,
        max_tokens: 128,
    }
}

#[tokio::test]
async fn a_short_recording_archives_the_previous_cassette_tail() {
    use crate::ports::model::Model;

    let dir = tempfile::tempdir().expect("temporary corpus");
    let cassette_dir = dir.path().join("cassettes/ts-0001");
    let audit_dir = dir.path().join("recording-attempts/ts-0001");
    let old_requests = ["old-1", "old-2", "old-3"];

    let old_recorder =
        crate::harness::cassette::Cassette::record(Arc::new(MockModel::silent()), &cassette_dir);
    for prompt in old_requests {
        old_recorder
            .complete(cassette_request(prompt))
            .await
            .expect("old response recorded");
    }
    assert_eq!(old_recorder.flush().expect("old takes flushed"), 3);

    let reservation = crate::eval::recording_budget::Reservation::begin(&audit_dir, 1, 1)
        .expect("new attempt admitted");
    let reservation_id = reservation.id().to_owned();
    archive_prior_takes(&cassette_dir, reservation.directory(), reservation.id())
        .expect("prior takes archived");
    let new_recorder =
        crate::harness::cassette::Cassette::record(Arc::new(MockModel::silent()), &cassette_dir);
    new_recorder
        .complete(cassette_request("new-only"))
        .await
        .expect("new response recorded");
    assert_eq!(new_recorder.flush().expect("new take flushed"), 1);
    reservation.finish(1).expect("attempt count finalized");

    let replay = crate::harness::cassette::Cassette::replay(
        &cassette_dir,
        crate::harness::cassette::Mode::Strict,
    )
    .expect("strict replay loads only the new take");
    replay
        .complete(cassette_request("new-only"))
        .await
        .expect("the current take replays");
    assert!(replay.complete(cassette_request("old-3")).await.is_err());
    assert_eq!(replay.strict_misses(), 1);

    let archived = std::fs::read_dir(audit_dir.join("prior-takes").join(reservation_id))
        .expect("attempt archive exists")
        .count();
    assert_eq!(archived, 3, "all old takes remain auditable");
}

#[test]
fn live_recording_candidate_pins_provider_price_and_output_bounds() {
    let loaded = crate::config::load_validated(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")),
        Some(std::path::Path::new("evals/recording.deepinfra.toml")),
    )
    .expect("recording candidate validates");
    let config = loaded.config;
    assert_eq!(config.models.scan, "deepseek/deepseek-v3.2");
    assert_eq!(config.models.deep, config.models.scan);
    assert_eq!(config.models.flash, config.models.scan);
    assert!(config.models.fallback.is_empty());
    assert_eq!(config.models.provider.order, ["deepinfra"]);
    assert!(!config.models.provider.allow_fallbacks);
    assert!(!config.models.provider.last_resort_unpinned);
    assert!(!config.models.agentic_reviewers);
    assert_eq!(config.models.max_tokens, 4096);
    assert_eq!(config.models.reasoning_effort, "off");
    assert_eq!(config.models.reviewer_reasoning_effort, "off");
    assert_eq!(config.models.falsify_reasoning_effort, "off");
    assert_eq!(config.models.summary_reasoning_effort, "off");
    let price = config
        .models
        .budget_prices
        .get("deepseek/deepseek-v3.2")
        .expect("verified price bound");
    assert_eq!(
        (price.input, price.cached, price.output),
        (0.26, 0.26, 0.38)
    );
}

#[test]
fn continuation_candidate_pins_gpt_oss_novita_and_its_recording_limits() {
    let loaded = crate::config::load_validated(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")),
        Some(std::path::Path::new("evals/recording.gptoss-novita.toml")),
    )
    .expect("continuation candidate validates");
    let config = loaded.config;
    assert_eq!(config.models.scan, "openai/gpt-oss-120b");
    assert_eq!(config.models.deep, config.models.scan);
    assert_eq!(config.models.flash, config.models.scan);
    assert!(config.models.fallback.is_empty());
    assert!(config.models.routes.is_empty());
    assert_eq!(config.models.provider.order, ["novita"]);
    assert!(!config.models.provider.allow_fallbacks);
    assert!(!config.models.provider.last_resort_unpinned);
    assert!(config.models.provider.unpinned_vendors.is_empty());
    assert!(!config.models.agentic_reviewers);
    assert_eq!(config.models.max_tokens, 12000);
    assert_eq!(config.models.reasoning_effort, "low");
    assert_eq!(config.models.reviewer_reasoning_effort, "low");
    assert_eq!(config.models.falsify_reasoning_effort, "low");
    assert_eq!(config.models.summary_reasoning_effort, "low");
    assert_eq!(config.models.budget_usd_per_pr, 0.48);
    let price = config
        .models
        .budget_prices
        .get("openai/gpt-oss-120b")
        .expect("verified reservation rate");
    assert_eq!(
        (price.input, price.cached, price.output),
        (0.05, 0.05, 0.25)
    );
}

#[derive(Clone)]
struct BudgetCaptureModel {
    roots: Arc<std::sync::Mutex<Vec<f64>>>,
    calls: Arc<std::sync::atomic::AtomicUsize>,
    fail_without_usage: bool,
}

#[derive(Clone)]
struct HiddenRetryModel {
    control: Option<Arc<dyn crate::ports::model_execution::ModelExecutionControl>>,
    physical_calls: Arc<std::sync::atomic::AtomicUsize>,
}

#[async_trait::async_trait]
impl crate::ports::model::Model for HiddenRetryModel {
    async fn complete(
        &self,
        _request: crate::ports::model::ModelRequest,
    ) -> crate::error::Result<crate::ports::model::ModelResponse> {
        use crate::ports::model_execution::{
            ModelExecutionTerminal, ModelExecutionTerminalOutcome, ModelExecutionTerminalUsage,
        };

        let control = self.control.as_ref().expect("physical control bound");
        for _ in 0..100 {
            let permit = match control.acquire().await {
                Ok(permit) => permit,
                Err(refusal) => {
                    return Err(crate::error::Error::ModelExecutionRefused {
                        refusal,
                        usage: None,
                        accounting_complete: true,
                    });
                }
            };
            permit
                .mark_dispatched()
                .expect("reserved test dispatch is admitted");
            self.physical_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            control.on_terminal(ModelExecutionTerminal {
                requested_model: "deepseek/deepseek-v3.2".into(),
                answered_model: Some("deepseek/deepseek-v3.2".into()),
                outcome: ModelExecutionTerminalOutcome::Succeeded,
                refused: false,
                usage: Some(ModelExecutionTerminalUsage {
                    input_tokens: 10,
                    output_tokens: 10,
                    reasoning_tokens: 0,
                    cached_tokens: 0,
                    cost_usd: Some(0.0001),
                }),
            });
        }
        Ok(crate::ports::model::ModelResponse {
            value: json!({"summary": "No actionable findings.", "findings": []}),
            model: "deepseek/deepseek-v3.2".into(),
            usage: crate::ports::model::Usage::default(),
        })
    }

    fn complete_reports_physical_calls(&self) -> bool {
        true
    }

    fn reports_raw_completion_usage(&self) -> bool {
        true
    }

    fn with_review_budget(&self, _limit_usd: f64) -> Option<Arc<dyn crate::ports::model::Model>> {
        Some(Arc::new(self.clone()))
    }

    fn with_execution_control(
        &self,
        control: Arc<dyn crate::ports::model_execution::ModelExecutionControl>,
    ) -> Option<Arc<dyn crate::ports::model::Model>> {
        Some(Arc::new(Self {
            control: Some(control),
            physical_calls: self.physical_calls.clone(),
        }))
    }
}

#[async_trait::async_trait]
impl crate::ports::model::Model for BudgetCaptureModel {
    async fn complete(
        &self,
        _request: crate::ports::model::ModelRequest,
    ) -> crate::error::Result<crate::ports::model::ModelResponse> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if self.fail_without_usage {
            return Err(crate::error::Error::Model("unreported test charge".into()));
        }
        Ok(crate::ports::model::ModelResponse {
            value: json!({"summary": "No actionable findings.", "findings": []}),
            model: "test/budget-capture".into(),
            usage: crate::ports::model::Usage::default(),
        })
    }

    fn with_review_budget(&self, limit_usd: f64) -> Option<Arc<dyn crate::ports::model::Model>> {
        self.roots.lock().expect("budget roots").push(limit_usd);
        Some(Arc::new(self.clone()))
    }
}

fn two_case_corpus(directory: &tempfile::TempDir) -> crate::eval::corpus::Corpus {
    let mut corpus = load(directory.path()).expect("loads one-case corpus");
    let mut second = corpus.cases[0].clone();
    second.case.id = "ts-0002".into();
    corpus.cases.push(second);
    corpus
}

#[tokio::test]
async fn hidden_physical_retries_share_the_recording_cap_and_failed_attempt_audit() {
    let dir = corpus_dir(EXPECTATION);
    let corpus = load(dir.path()).expect("loads one-case corpus");
    let physical_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let model = Arc::new(HiddenRetryModel {
        control: None,
        physical_calls: physical_calls.clone(),
    });
    let mut config = config();
    config.models.budget_usd_per_pr = 0.5;

    run(
        &corpus,
        &config,
        Some(model),
        &RunOptions {
            out: dir.path().join("runs/hidden-retries"),
            record: true,
            max_cost_usd: 0.5,
            max_physical_calls_per_case: Some(8),
            ..RunOptions::default()
        },
    )
    .await
    .expect("persists bounded failed review");

    assert_eq!(physical_calls.load(std::sync::atomic::Ordering::SeqCst), 8);
    let proposal: crate::app::review::Proposal = serde_json::from_slice(
        &std::fs::read(dir.path().join("runs/hidden-retries/ts-0001/proposal.json"))
            .expect("bounded review proposal exists"),
    )
    .expect("proposal decodes");
    assert!(
        proposal.skipped.is_some() || !proposal.unreviewed.is_empty(),
        "call-limit refusal must remain visibly incomplete, never look like a clean review"
    );
    let audit: serde_json::Value = serde_json::from_slice(
        &std::fs::read(
            corpus
                .root
                .join("recording-attempts/ts-0001/recording-attempts.json"),
        )
        .expect("attempt audit exists"),
    )
    .expect("valid attempt audit");
    assert_eq!(audit["reservations"][0]["reserved_calls"], 8);
    assert_eq!(audit["reservations"][0]["actual_calls"], 8);
}

#[tokio::test]
async fn recording_rerun_uses_only_the_remaining_cumulative_attempt_allowance() {
    let dir = corpus_dir(EXPECTATION);
    let corpus = load(dir.path()).expect("loads one-case corpus");
    let audit_dir = corpus.root.join("recording-attempts/ts-0001");
    let first = crate::eval::recording_budget::Reservation::begin(&audit_dir, 8, 56)
        .expect("first invocation reserves its initial allowance");
    first
        .finish(8)
        .expect("first invocation audit is completed");

    let cassette_dir = corpus.cases[0].cassette_dir(&corpus.root);
    std::fs::create_dir_all(&cassette_dir).expect("cassette directory");
    let stale_tail = cassette_dir.join("9999-stale-tail.json");
    std::fs::write(&stale_tail, "stale take from an earlier attempt")
        .expect("seed stale cassette tail");

    let physical_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let model = Arc::new(HiddenRetryModel {
        control: None,
        physical_calls: physical_calls.clone(),
    });
    let mut config = config();
    config.models.budget_usd_per_pr = 0.5;

    let outcome = run(
        &corpus,
        &config,
        Some(model),
        &RunOptions {
            out: dir.path().join("runs/cumulative-retry"),
            record: true,
            max_cost_usd: 0.5,
            max_physical_calls_per_case: Some(40),
            ..RunOptions::default()
        },
    )
    .await
    .expect("rerun is admitted against the persisted remainder");

    assert_eq!(physical_calls.load(std::sync::atomic::Ordering::SeqCst), 32);
    let audit: serde_json::Value = serde_json::from_slice(
        &std::fs::read(audit_dir.join("recording-attempts.json")).expect("attempt audit exists"),
    )
    .expect("valid attempt audit");
    assert_eq!(audit["reservations"][0]["actual_calls"], 8);
    assert_eq!(audit["reservations"][1]["reserved_calls"], 32);
    assert_eq!(audit["reservations"][1]["actual_calls"], 32);
    assert!(
        !stale_tail.exists(),
        "old takes must leave the replay directory"
    );
    assert_eq!(
        outcome.execution_limits.max_physical_calls_per_case,
        Some(40),
        "the operator's requested cumulative ceiling remains visible"
    );
    assert_eq!(
        outcome.execution_limits.effective_case_budgets[0].max_physical_calls_per_case,
        Some(32),
        "identity records the allowance actually admitted after prior attempts"
    );
    let limits = serde_json::to_value(&outcome.execution_limits).expect("limits serialize");
    assert_eq!(limits["max_physical_calls_per_case"], 40);
    assert_eq!(
        limits["effective_case_budgets"][0]["max_physical_calls_per_case"],
        32
    );
    let archived = audit_dir.join("prior-takes");
    assert!(
        archived.exists(),
        "replaced takes retain attempt provenance"
    );
    assert!(
        std::fs::read_dir(archived)
            .expect("archive listing")
            .next()
            .is_some()
    );
}

#[tokio::test]
async fn a_recording_ceiling_cannot_be_replenished_by_rerunning() {
    for ceiling in [8, 32] {
        let dir = corpus_dir(EXPECTATION);
        let corpus = load(dir.path()).expect("loads one-case corpus");
        let physical_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let model = Arc::new(HiddenRetryModel {
            control: None,
            physical_calls: physical_calls.clone(),
        });
        let mut config = config();
        config.models.budget_usd_per_pr = 0.5;
        let requested_ceiling = Some(ceiling);

        run(
            &corpus,
            &config,
            Some(model.clone()),
            &RunOptions {
                out: dir.path().join("runs/cumulative-first"),
                record: true,
                max_cost_usd: 0.5,
                max_physical_calls_per_case: requested_ceiling,
                ..RunOptions::default()
            },
        )
        .await
        .expect("initial selected allowance is admitted");
        assert_eq!(
            physical_calls.load(std::sync::atomic::Ordering::SeqCst),
            ceiling
        );
        let denied_tail = corpus.cases[0]
            .cassette_dir(&corpus.root)
            .join("denied-attempt-must-not-archive.json");
        std::fs::create_dir_all(denied_tail.parent().expect("cassette parent"))
            .expect("create active cassette directory");
        std::fs::write(&denied_tail, "still active until reservation succeeds")
            .expect("seed active take before denied rerun");

        let second = run(
            &corpus,
            &config,
            Some(model),
            &RunOptions {
                out: dir.path().join("runs/cumulative-second"),
                record: true,
                max_cost_usd: 0.5,
                max_physical_calls_per_case: requested_ceiling,
                ..RunOptions::default()
            },
        )
        .await;
        assert!(
            second.is_err(),
            "a rerun cannot restore the selected {ceiling}-call allowance"
        );
        assert_eq!(
            physical_calls.load(std::sync::atomic::Ordering::SeqCst),
            ceiling
        );
        assert!(
            denied_tail.exists(),
            "an exhausted reservation must not alter active takes"
        );
    }
}

#[tokio::test]
async fn recording_binds_each_case_to_its_share_of_the_global_cost_ceiling() {
    let dir = corpus_dir(EXPECTATION);
    let mut corpus = two_case_corpus(&dir);
    for case in &mut corpus.cases {
        case.case.budget.max_cost_usd = 0.5;
    }
    let roots = Arc::new(std::sync::Mutex::new(Vec::new()));
    let model = Arc::new(BudgetCaptureModel {
        roots: roots.clone(),
        calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        fail_without_usage: false,
    });
    let mut config = config();
    config.models.budget_usd_per_pr = 1.0;

    run(
        &corpus,
        &config,
        Some(model),
        &RunOptions {
            out: dir.path().join("runs/budget-bound"),
            record: true,
            max_cost_usd: 0.5,
            ..RunOptions::default()
        },
    )
    .await
    .expect("records both bounded cases");

    let roots = roots.lock().expect("budget roots");
    assert_eq!(roots.len(), 2, "each case binds one monetary root");
    assert!(roots.iter().all(|limit| (*limit - 0.25).abs() < 1e-12));
}

#[tokio::test]
async fn an_unreported_case_charge_stops_later_recording_cases() {
    let dir = corpus_dir(EXPECTATION);
    let mut corpus = two_case_corpus(&dir);
    for case in &mut corpus.cases {
        case.case.budget.max_cost_usd = 0.5;
    }
    let roots = Arc::new(std::sync::Mutex::new(Vec::new()));
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let model = Arc::new(BudgetCaptureModel {
        roots: roots.clone(),
        calls: calls.clone(),
        fail_without_usage: true,
    });
    let mut config = config();
    config.models.budget_usd_per_pr = 1.0;

    let result = run(
        &corpus,
        &config,
        Some(model),
        &RunOptions {
            out: dir.path().join("runs/unknown-charge"),
            record: true,
            max_cost_usd: 0.5,
            ..RunOptions::default()
        },
    )
    .await
    .expect("persists the failed first case and stops");

    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(roots.lock().expect("budget roots").len(), 1);
    assert_eq!(result.scores.len(), 1);
    assert_eq!(result.skipped, vec!["ts-0002"]);
}

#[tokio::test]
async fn recording_from_an_arbitrary_tree_does_not_claim_fixture_history() {
    use crate::ports::model::{ModelRequest, ModelResponse, Usage};
    use crate::ports::tree::{Found, Lookup, TreeQuery, TreeReader};

    struct HistoryProbe {
        history: Arc<std::sync::Mutex<Vec<Found>>>,
        falsification_keys: Arc<std::sync::Mutex<Vec<String>>>,
    }

    #[async_trait::async_trait]
    impl crate::ports::model::Model for HistoryProbe {
        async fn complete(&self, request: ModelRequest) -> crate::error::Result<ModelResponse> {
            if request.schema_name == "tinysweeper_falsify" {
                self.falsification_keys
                    .lock()
                    .expect("falsification key capture")
                    .push(crate::harness::cassette::key(&request));
            }
            Ok(ModelResponse {
                value: json!({"incorrect": []}),
                model: "test/history-probe".into(),
                usage: Usage::default(),
            })
        }

        async fn review(
            &self,
            _request: ModelRequest,
            tree: &dyn TreeReader,
            _policy: &crate::config::types::LookupPolicy,
        ) -> crate::error::Result<ModelResponse> {
            let _ = tree
                .lookup(&Lookup::Read {
                    path: "src/lib.rs".into(),
                    start: Some(1),
                    end: Some(2),
                })
                .await?;
            let _ = tree
                .explore(&TreeQuery::Symbol {
                    symbol: "from_another_revision".into(),
                    limit: 3,
                })
                .await?;
            let found = tree
                .explore(&TreeQuery::History {
                    commit: "b".repeat(40),
                    path: "src/lib.rs".into(),
                    start: 1,
                    end: 1,
                })
                .await?;
            self.history.lock().expect("history capture").push(found);
            Ok(ModelResponse {
                value: json!({"summary": "Indexing can panic on an empty slice.", "findings": [{
                    "path": "src/lib.rs", "existing_code": "items[0]", "rule": "unchecked-index",
                    "title": "Guard the index", "body": "Indexing panics for an empty slice.",
                    "severity": "high", "confidence": 0.9
                }], "rules": [], "rejected": []}),
                model: "test/history-probe".into(),
                usage: Usage::default(),
            })
        }
    }

    let corpus_dir = corpus_dir("");
    let corpus = load(corpus_dir.path()).expect("loads corpus");
    let source = tempfile::tempdir().expect("source tree");
    std::fs::create_dir_all(source.path().join("src")).expect("source directory");
    std::fs::write(
        source.path().join("src/lib.rs"),
        "fn from_another_revision() {}\n",
    )
    .expect("source file");
    let history = Arc::new(std::sync::Mutex::new(Vec::new()));
    let falsification_keys = Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut config = config();
    config.models.agentic_reviewers = true;
    // This test pins the fixture-revision history lookup. A second adaptive
    // pass would legitimately issue the same history query again after the
    // high-severity response, so keep the probe to one review pass.
    config.review.passes = 1;

    run(
        &corpus,
        &config,
        Some(Arc::new(HistoryProbe {
            history: history.clone(),
            falsification_keys: falsification_keys.clone(),
        })),
        &RunOptions {
            out: corpus_dir.path().join("runs/record"),
            record: true,
            tree: Some(source.path().to_path_buf()),
            ..RunOptions::default()
        },
    )
    .await
    .expect("records against the supplied tree");

    {
        let history = history.lock().expect("history capture");
        assert_eq!(history.len(), 1);
        assert!(
            !matches!(history[0], Found::Text { .. }),
            "an arbitrary directory cannot answer history at the fixture's commit: {:?}",
            history[0]
        );
    }
    let falsification_key = falsification_keys
        .lock()
        .expect("falsification key capture")
        .first()
        .cloned()
        .expect("the finding reaches the falsifier");

    let recorded_corpus = load(corpus_dir.path()).expect("loads persisted source evidence");
    let lookups = &recorded_corpus.cases[0].fixture.lookups;
    assert!(
        lookups.contains_key(
            &Lookup::Read {
                path: "src/lib.rs".into(),
                start: Some(1),
                end: Some(2),
            }
            .key()
        )
    );
    assert!(
        lookups.contains_key(
            &TreeQuery::Symbol {
                symbol: "from_another_revision".into(),
                limit: 3,
            }
            .key()
        )
    );
    assert!(
        lookups.contains_key(
            &TreeQuery::History {
                commit: "b".repeat(40),
                path: "src/lib.rs".into(),
                start: 1,
                end: 1,
            }
            .key()
        )
    );
    let cassette_dir = recorded_corpus.cases[0].cassette_dir(corpus_dir.path());
    let mut found_falsification_take = false;
    for entry in std::fs::read_dir(cassette_dir).expect("cassette directory") {
        let raw =
            std::fs::read_to_string(entry.expect("cassette entry").path()).expect("cassette take");
        let take: crate::harness::cassette::Take =
            serde_json::from_str(&raw).expect("valid cassette take");
        if take.schema_name == "tinysweeper_falsify" {
            assert_eq!(take.key, falsification_key);
            found_falsification_take = true;
        }
    }
    assert!(
        found_falsification_take,
        "the actual falsification response is recorded"
    );

    let replay_out = corpus_dir.path().join("runs/replay");
    let replay = run(
        &recorded_corpus,
        &config,
        None,
        &RunOptions {
            out: replay_out,
            ..RunOptions::default()
        },
    )
    .await
    .expect("strict replay is offline");
    assert!(
        replay.scores[0].error.is_none(),
        "all review and falsification keys replay: {:?}",
        replay.scores[0]
    );
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

    // Each stage has its own effort override. A role-only configuration
    // change must invalidate an eval baseline even when the global effort is
    // unchanged.
    let mut reviewer_effort = base.clone();
    reviewer_effort.models.reviewer_reasoning_effort = "low".into();
    assert_ne!(digest_of(&base), digest_of(&reviewer_effort));
    let mut falsify_effort = base.clone();
    falsify_effort.models.falsify_reasoning_effort = "high".into();
    assert_ne!(digest_of(&base), digest_of(&falsify_effort));
    let mut summary_effort = base.clone();
    summary_effort.models.summary_reasoning_effort = "high".into();
    assert_ne!(digest_of(&base), digest_of(&summary_effort));

    let baseline = crate::eval::types::Scorecard {
        corpus_digest: "same-corpus".into(),
        config_digest: digest_of(&base),
        execution_limits: None,
        loose_replays: 0,
        cases: vec![],
    };
    for changed in [&reviewer_effort, &falsify_effort, &summary_effort] {
        let current = crate::eval::types::Scorecard {
            corpus_digest: baseline.corpus_digest.clone(),
            config_digest: digest_of(changed),
            execution_limits: None,
            loose_replays: 0,
            cases: vec![],
        };
        assert!(matches!(
            crate::eval::report::compare(&current, &baseline, false),
            crate::eval::report::Comparison::Incomparable(reason)
                if reason.contains("configuration changed")
        ));
    }

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
async fn execution_limits_capture_selected_cases_and_effective_run_ceilings() {
    let dir = corpus_dir(EXPECTATION);
    std::fs::write(
        dir.path().join("cases/ts-0002.toml"),
        case_toml(EXPECTATION).replace("id = \"ts-0001\"", "id = \"ts-0002\""),
    )
    .expect("write second case");
    let full = load(dir.path()).expect("loads both cases");
    let selected = full
        .clone()
        .select(&["ts-0001".to_owned()])
        .expect("select known case");
    let base_options = RunOptions {
        out: dir.path().join("runs/identity-base"),
        max_cost_usd: 0.25,
        max_physical_calls_per_case: Some(8),
        ..RunOptions::default()
    };
    let base = run(&full, &config(), None, &base_options)
        .await
        .expect("empty spend ceiling skips cases");
    let selected_options = RunOptions {
        out: dir.path().join("runs/identity-selected"),
        ..base_options.clone()
    };
    let selected_run = run(&selected, &config(), None, &selected_options)
        .await
        .expect("selected empty run");
    let different_bound = RunOptions {
        out: dir.path().join("runs/identity-bound"),
        max_cost_usd: 0.2,
        max_physical_calls_per_case: Some(7),
        ..base_options
    };
    let bounded_run = run(&full, &config(), None, &different_bound)
        .await
        .expect("bounded empty run");

    assert_eq!(
        base.execution_limits.selected_case_ids,
        ["ts-0001", "ts-0002"]
    );
    assert_eq!(selected_run.execution_limits.selected_case_ids, ["ts-0001"]);
    assert_eq!(base.execution_limits.max_cost_usd, 0.25);
    assert_eq!(base.execution_limits.max_physical_calls_per_case, Some(8));
    assert_ne!(base.execution_limits, selected_run.execution_limits);
    assert_ne!(base.execution_limits, bounded_run.execution_limits);
    assert!(
        base.execution_limits
            .effective_case_budgets
            .iter()
            .all(|budget| { budget.budget_usd.is_none() })
    );
    let recording_limits = super::effective_execution_limits(
        &full,
        &config(),
        &RunOptions {
            record: true,
            max_cost_usd: 0.025,
            ..RunOptions::default()
        },
    );
    assert_eq!(
        recording_limits
            .effective_case_budgets
            .iter()
            .map(|budget| budget.budget_usd)
            .collect::<Vec<_>>(),
        [Some(0.0125), Some(0.0125)]
    );
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
        reasoning_effort: None,
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
