//! Cassette recording, replay, and the two ways it is allowed to miss.

use std::sync::Arc;

use serde_json::json;

use super::*;
use crate::harness::mock::MockModel;
use crate::ports::model::{Message, ModelResponse};
use crate::ports::tree::{Found, Lookup, TreeQuery, TreeReader};

struct ReviewOnlyModel {
    reviews: std::sync::atomic::AtomicUsize,
}

#[async_trait::async_trait]
impl Model for ReviewOnlyModel {
    async fn complete(&self, _request: ModelRequest) -> Result<ModelResponse> {
        Err(Error::Model("the test requires Model::review".into()))
    }

    async fn review(
        &self,
        _request: ModelRequest,
        tree: &dyn TreeReader,
        _policy: &crate::config::types::LookupPolicy,
    ) -> Result<ModelResponse> {
        self.reviews
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        tree.lookup(&Lookup::Read {
            path: "src/lib.rs".into(),
            start: Some(1),
            end: Some(2),
        })
        .await?;
        tree.explore(&TreeQuery::Symbol {
            symbol: "load_state".into(),
            limit: 3,
        })
        .await?;
        Ok(ModelResponse {
            value: answer(),
            model: "test/review-only".into(),
            usage: Usage {
                input_tokens: 17,
                output_tokens: 5,
                cost_usd: 0.002,
                ..Usage::default()
            },
        })
    }
}

struct ReviewTree;

#[async_trait::async_trait]
impl TreeReader for ReviewTree {
    async fn lookup(&self, lookup: &Lookup) -> Result<Found> {
        Ok(match lookup {
            Lookup::Read { path, .. } if path == "src/lib.rs" => Found::Text {
                text: "1: pub fn load_state() {}\n2: // source".into(),
                start: 1,
                end: 2,
                total: 2,
            },
            _ => Found::NotFound,
        })
    }

    async fn explore(&self, query: &TreeQuery) -> Result<Found> {
        Ok(match query {
            TreeQuery::Symbol { symbol, .. } if symbol == "load_state" => Found::Hits {
                hits: vec![crate::ports::tree::Hit {
                    path: "src/lib.rs".into(),
                    line: 1,
                    text: "pub fn load_state() {}".into(),
                }],
                truncated: false,
                skipped: vec![],
            },
            _ => Found::NotFound,
        })
    }

    fn describe(&self) -> String {
        "test tree".into()
    }

    fn revision(&self) -> Option<String> {
        Some("a".repeat(40))
    }
}

#[derive(Clone, Default)]
struct ReviewBudgetProbe {
    remaining: Option<Arc<std::sync::atomic::AtomicUsize>>,
    completed: Arc<std::sync::atomic::AtomicUsize>,
}

struct PhysicalEventCounter(std::sync::atomic::AtomicUsize);

impl crate::ports::model::ModelCallObserver for PhysicalEventCounter {
    fn completed(
        &self,
        _usage: Usage,
        succeeded: bool,
        refused: bool,
        _latency: std::time::Duration,
    ) {
        assert!(succeeded);
        assert!(!refused);
        self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
}

#[cfg(feature = "harness")]
#[derive(Default)]
struct ModelEventCounter(std::sync::Mutex<Vec<Usage>>);

#[cfg(feature = "harness")]
impl crate::ports::model::ModelCallObserver for ModelEventCounter {
    fn completed(
        &self,
        usage: Usage,
        _succeeded: bool,
        _refused: bool,
        _latency: std::time::Duration,
    ) {
        self.0.lock().expect("model event lock").push(usage);
    }
}

#[async_trait::async_trait]
impl Model for ReviewBudgetProbe {
    async fn complete(&self, _request: ModelRequest) -> Result<ModelResponse> {
        use std::sync::atomic::Ordering;

        if let Some(remaining) = &self.remaining
            && remaining
                .try_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                    count.checked_sub(1)
                })
                .is_err()
        {
            return Err(Error::Budget {
                spent: 1.0,
                limit: 1.0,
                usage: None,
            });
        }
        self.completed.fetch_add(1, Ordering::Relaxed);
        let usage = Usage {
            input_tokens: 2,
            output_tokens: 1,
            calls_attempted: 1,
            calls_succeeded: 1,
            cost_usd: 1.0,
            ..Usage::default()
        };
        if let Some(observer) = crate::ports::model::current_model_call_observer() {
            observer.completed(usage, true, false, std::time::Duration::ZERO);
        }
        Ok(ModelResponse {
            value: answer(),
            model: "review-budget-probe".into(),
            usage,
        })
    }

    async fn review(
        &self,
        _request: ModelRequest,
        _tree: &dyn crate::ports::tree::TreeReader,
        _policy: &crate::config::types::LookupPolicy,
    ) -> Result<ModelResponse> {
        Ok(ModelResponse {
            value: answer(),
            model: "review-budget-probe-logical-review".into(),
            usage: Usage::default(),
        })
    }

    fn complete_reports_physical_calls(&self) -> bool {
        true
    }

    fn review_reports_physical_calls(&self) -> bool {
        false
    }

    fn scoped_budget(&self, _budget_usd: f64) -> Option<Arc<dyn Model>> {
        Some(Arc::new(self.clone()))
    }

    fn with_review_budget(&self, limit_usd: f64) -> Option<Arc<dyn Model>> {
        let calls = limit_usd.floor().max(0.0) as usize;
        Some(Arc::new(Self {
            remaining: Some(Arc::new(std::sync::atomic::AtomicUsize::new(calls))),
            completed: self.completed.clone(),
        }))
    }
}

#[tokio::test]
async fn recording_cassette_preserves_review_root_and_recording_state_across_scopes() {
    use std::sync::atomic::Ordering;

    for bind_after_scope in [false, true] {
        let dir = tempfile::tempdir().expect("temporary cassette directory");
        let inner = Arc::new(ReviewBudgetProbe::default());
        let completed = inner.completed.clone();
        let cassette = Arc::new(Cassette::record(inner, dir.path()));
        let model: Arc<dyn Model> = cassette.clone();
        let root = if bind_after_scope {
            model
                .scoped_budget(8.0)
                .expect("recording cassette forwards lane scopes")
                .with_review_budget(1.0)
                .expect("recording cassette forwards review binding")
        } else {
            model
                .with_review_budget(1.0)
                .expect("recording cassette forwards review binding")
        };
        let lane = root
            .scoped_budget(8.0)
            .expect("recording cassette retains the review root on lane rescope");
        assert!(root.complete_reports_physical_calls());
        assert!(!root.review_reports_physical_calls());
        let physical_events =
            Arc::new(PhysicalEventCounter(std::sync::atomic::AtomicUsize::new(0)));

        let first = crate::ports::model::with_model_call_observer(
            physical_events.clone(),
            root.complete(request("probe", "first paid response")),
        )
        .await
        .expect("the first response fits the review root");
        assert_eq!(first.value, answer());
        let second = crate::ports::model::with_model_call_observer(
            physical_events.clone(),
            lane.complete(request("probe", "second paid response")),
        )
        .await;
        assert!(matches!(second, Err(Error::Budget { .. })));
        assert_eq!(completed.load(Ordering::Relaxed), 1);
        assert_eq!(physical_events.0.load(Ordering::Relaxed), 1);
        assert_eq!(cassette.served(), 1);
        assert_eq!(cassette.spend().usage.input_tokens, 2);
        assert_eq!(cassette.spend().usage.output_tokens, 1);
        assert_eq!(cassette.cost_usd(), 1.0);
        assert_eq!(
            cassette
                .flush()
                .expect("the original recorder sees the take"),
            1
        );
        assert_eq!(
            load(dir.path())
                .expect("the recorded take remains readable")
                .len(),
            1
        );

        let replay = Cassette::replay(dir.path(), Mode::Strict).expect("strict replay loads");
        assert!(!replay.complete_reports_physical_calls());
        assert!(!replay.review_reports_physical_calls());
        crate::ports::model::with_model_call_observer(
            physical_events.clone(),
            replay.complete(request("probe", "first paid response")),
        )
        .await
        .expect("replay serves the recorded response without another live call");
        assert_eq!(completed.load(Ordering::Relaxed), 1);
        assert_eq!(physical_events.0.load(Ordering::Relaxed), 1);
    }
}

#[tokio::test]
async fn recording_cassette_preserves_dispatch_control_across_budget_binding_orders() {
    use crate::harness::mock::execution_test_support::{DispatchProbe, execution_control};
    use crate::ports::model_execution::ModelExecutionRefusal;

    for scope_first in [false, true] {
        for control_first in [false, true] {
            let dir = tempfile::tempdir().expect("temporary cassette directory");
            let mock = MockModel::new()
                .then(json!({"summary":"first"}))
                .then(json!({"summary":"must remain queued"}))
                .with_usage(Usage {
                    input_tokens: 2,
                    output_tokens: 1,
                    calls_attempted: 1,
                    calls_succeeded: 1,
                    cost_usd: 1.0,
                    ..Usage::default()
                });
            let cassette = Arc::new(Cassette::record(
                Arc::new(DispatchProbe::new(mock.clone())),
                dir.path(),
            ));
            let model: Arc<dyn Model> = cassette.clone();
            let (execution, control) = execution_control(1);
            let scoped = if scope_first {
                model
                    .scoped_budget(8.0)
                    .expect("recording cassette creates a lane scope")
            } else {
                model
            };
            let root = if control_first {
                let controlled = scoped
                    .with_execution_control(control)
                    .unwrap_or_else(|| scoped.clone());
                controlled
                    .with_review_budget(10.0)
                    .expect("recording cassette binds the shared review budget")
            } else {
                let budgeted = scoped
                    .with_review_budget(10.0)
                    .expect("recording cassette binds the shared review budget");
                budgeted.with_execution_control(control).unwrap_or(budgeted)
            };
            let lane = root
                .scoped_budget(8.0)
                .expect("rescoping retains the recording state and both controls");

            assert!(
                root.complete(request("probe", "first paid response"))
                    .await
                    .is_ok()
            );
            assert!(matches!(
                lane.complete(request("probe", "refused second response"))
                    .await,
                Err(Error::ModelExecutionRefused {
                    refusal: ModelExecutionRefusal::CallLimit,
                    ..
                })
            ));
            assert_eq!(
                mock.calls(),
                1,
                "the refused call does not reach the inner model"
            );
            assert_eq!(execution.snapshot().committed_calls, 1);
            assert_eq!(cassette.served(), 1);
            assert_eq!(cassette.spend().usage.input_tokens, 2);
            assert_eq!(cassette.cost_usd(), 1.0);
            assert_eq!(
                cassette
                    .flush()
                    .expect("the shared recorder retains its first take"),
                1
            );
            assert_eq!(load(dir.path()).expect("recorded take loads").len(), 1);
            assert!(root.complete_reports_physical_calls());
            assert!(root.review_reports_physical_calls());
        }
    }
}

#[tokio::test]
async fn recording_cassette_review_reports_the_physical_path_it_uses() {
    let dir = tempfile::tempdir().expect("temporary cassette directory");
    let inner = Arc::new(ReviewBudgetProbe::default());
    let completed = inner.completed.clone();
    let cassette = Arc::new(Cassette::record(inner.clone(), dir.path()));
    let physical_events = Arc::new(PhysicalEventCounter(std::sync::atomic::AtomicUsize::new(0)));
    let tree = crate::ports::tree::MockTree::from_files([("src/lib.rs", "pub fn answer() {}")]);
    let policy = crate::config::types::LookupPolicy::default();

    let completed_call = crate::ports::model::with_model_call_observer(
        physical_events.clone(),
        cassette.complete(request("probe", "physical completion")),
    )
    .await
    .expect("recording completion uses the inner completion");
    assert_eq!(completed_call.model, "review-budget-probe");
    assert_eq!(completed.load(std::sync::atomic::Ordering::Relaxed), 1);
    assert_eq!(
        physical_events.0.load(std::sync::atomic::Ordering::Relaxed),
        1
    );

    let reviewed = crate::ports::model::with_model_call_observer(
        physical_events.clone(),
        cassette.review(request("probe", "logical review"), &tree, &policy),
    )
    .await
    .expect("recording review uses the inner logical review");
    assert_eq!(reviewed.model, "review-budget-probe-logical-review");
    assert_eq!(completed.load(std::sync::atomic::Ordering::Relaxed), 1);
    assert_eq!(
        physical_events.0.load(std::sync::atomic::Ordering::Relaxed),
        1,
        "review follows its own logical-only inner path"
    );
    assert_eq!(cassette.served(), 2);
    assert_eq!(
        cassette
            .flush()
            .expect("both methods were recorded in the cassette"),
        2
    );
    assert!(cassette.complete_reports_physical_calls());
    assert!(!cassette.review_reports_physical_calls());
}

#[tokio::test]
async fn recording_review_forwards_actual_review_and_tree_queries() {
    let dir = tempfile::tempdir().expect("tempdir");
    let live = Arc::new(ReviewOnlyModel {
        reviews: std::sync::atomic::AtomicUsize::new(0),
    });
    let recorder = Cassette::record(live.clone(), dir.path());
    let policy = crate::config::types::LookupPolicy::default();

    let response = recorder
        .review(
            request("test/review-only", "inspect this change"),
            &ReviewTree,
            &policy,
        )
        .await
        .expect("records through Model::review");

    assert_eq!(response.model, "test/review-only");
    assert_eq!(live.reviews.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(recorder.flush().expect("flushes"), 1);
    let path = std::fs::read_dir(dir.path())
        .expect("cassette directory")
        .next()
        .expect("one take")
        .expect("take entry")
        .path();
    let take: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(path).expect("take file"))
            .expect("take json");
    let context = &take["review_context"];
    assert_eq!(context["revision"], "a".repeat(40));
    let queries = context["queries"].as_array().expect("recorded queries");
    assert_eq!(queries.len(), 2);
    assert_eq!(queries[0]["query"]["kind"], "lookup");
    assert_eq!(queries[0]["query"]["query"]["kind"], "read");
    assert_eq!(queries[0]["query"]["query"]["path"], "src/lib.rs");
    assert_eq!(queries[1]["query"]["kind"], "symbol");
    assert_eq!(queries[1]["query"]["query"]["symbol"], "load_state");
    assert_eq!(take["usage"]["cost_usd"], 0.002);
}

#[tokio::test]
async fn review_replay_reissues_queries_and_reconstructs_identical_evidence_offline() {
    let dir = tempfile::tempdir().expect("tempdir");
    let live = Arc::new(ReviewOnlyModel {
        reviews: std::sync::atomic::AtomicUsize::new(0),
    });
    let source = ReviewTree;
    let recording_tree = crate::ports::tree::RecordingTree::new(&source);
    let recorder = Arc::new(Cassette::record(live.clone(), dir.path()));
    let models = crate::config::types::Models {
        agentic_reviewers: true,
        budget_usd_per_pr: 1.0,
        ..Default::default()
    };
    let live_cap = crate::flows::caps::ModelCapability::new(recorder.clone(), models.clone());
    let call = crate::flows::panel::Call {
        id: "reviewer".into(),
        model: "test/review-only".into(),
        system: "inspect".into(),
        prompt: "change evidence".into(),
        schema_name: "tinysweeper_critique".into(),
    };
    let schema = json!({"type": "object"});
    let policy = crate::config::types::LookupPolicy::default();
    let (live_response, live_evidence) = live_cap
        .review(&call, &schema, &recording_tree, &policy)
        .await
        .expect("live review");
    recorder.flush().expect("flushes actual review response");
    assert_eq!(live.reviews.load(std::sync::atomic::Ordering::SeqCst), 1);

    let recorded_lookups = recording_tree.recorded();
    assert_eq!(recorded_lookups.len(), 3);
    let replay_tree = crate::ports::tree::MockTree::from_recorded(recorded_lookups);
    let replay = Arc::new(Cassette::replay(dir.path(), Mode::Strict).expect("loads"));
    let replay_cap = crate::flows::caps::ModelCapability::new(replay.clone(), models);
    let (replayed_response, replayed_evidence) = replay_cap
        .review(&call, &schema, &replay_tree, &policy)
        .await
        .expect("offline replay reissues source reads");

    assert_eq!(replayed_response.value, live_response.value);
    assert_eq!(replayed_response.usage, live_response.usage);
    assert_eq!(replayed_evidence, live_evidence);
    assert_eq!(replay.served(), 1);
    assert_eq!(replay.strict_misses(), 0);
    let falsification_request = |evidence: &str| {
        let mut request = request("test/review-only", "falsify the finding");
        request.messages.push(Message::user(evidence));
        request
    };
    assert_eq!(
        key(&falsification_request(&live_evidence)),
        key(&falsification_request(&replayed_evidence)),
        "source evidence keeps the downstream falsification prompt stable"
    );
}

#[tokio::test]
async fn loose_review_replay_reissues_queries_for_a_changed_prompt() {
    let dir = tempfile::tempdir().expect("tempdir");
    let recorder = Cassette::record(
        Arc::new(ReviewOnlyModel {
            reviews: std::sync::atomic::AtomicUsize::new(0),
        }),
        dir.path(),
    );
    let policy = crate::config::types::LookupPolicy::default();
    recorder
        .review(
            request("test/review-only", "original prompt"),
            &ReviewTree,
            &policy,
        )
        .await
        .expect("records review");
    recorder.flush().expect("flushes review");

    let replay = Cassette::replay(dir.path(), Mode::Loose).expect("loads loose replay");
    let response = replay
        .review(
            request("test/review-only", "changed prompt"),
            &ReviewTree,
            &policy,
        )
        .await
        .expect("loose replay uses the review take by call order");

    assert_eq!(response.value, answer());
    assert_eq!(replay.loose_hits(), 1);
    assert_eq!(replay.served(), 1);
    assert_eq!(replay.strict_misses(), 0);
}

#[tokio::test]
async fn review_policy_revision_method_and_source_are_part_of_strict_identity() {
    let dir = tempfile::tempdir().expect("tempdir");
    let recorder = Cassette::record(
        Arc::new(ReviewOnlyModel {
            reviews: std::sync::atomic::AtomicUsize::new(0),
        }),
        dir.path(),
    );
    let review_request = request("test/review-only", "inspect this change");
    let policy = crate::config::types::LookupPolicy::default();
    recorder
        .review(review_request.clone(), &ReviewTree, &policy)
        .await
        .expect("records");
    recorder.flush().expect("flushes");

    let player = Cassette::replay(dir.path(), Mode::Strict).expect("loads");
    let mut changed_policy = policy.clone();
    changed_policy.rounds += 1;
    assert!(
        player
            .review(review_request.clone(), &ReviewTree, &changed_policy)
            .await
            .is_err()
    );
    assert_eq!(player.strict_misses(), 1);

    struct DifferentRevision;
    #[async_trait::async_trait]
    impl TreeReader for DifferentRevision {
        async fn lookup(&self, _: &Lookup) -> Result<Found> {
            Ok(Found::NotFound)
        }

        fn describe(&self) -> String {
            "same reader".into()
        }

        fn revision(&self) -> Option<String> {
            Some("b".repeat(40))
        }
    }
    let revision_player = Cassette::replay(dir.path(), Mode::Strict).expect("loads");
    assert!(
        revision_player
            .review(review_request.clone(), &DifferentRevision, &policy)
            .await
            .is_err()
    );
    assert_eq!(revision_player.strict_misses(), 1);

    let method_player = Cassette::replay(dir.path(), Mode::Strict).expect("loads");
    assert!(
        method_player
            .complete(review_request.clone())
            .await
            .is_err()
    );
    assert_eq!(method_player.strict_misses(), 1);

    struct SameRevisionDifferentSource;
    #[async_trait::async_trait]
    impl TreeReader for SameRevisionDifferentSource {
        async fn lookup(&self, _: &Lookup) -> Result<Found> {
            Ok(Found::NotFound)
        }

        async fn explore(&self, _: &TreeQuery) -> Result<Found> {
            Ok(Found::NotFound)
        }

        fn describe(&self) -> String {
            "/different/absolute/archive".into()
        }

        fn revision(&self) -> Option<String> {
            Some("a".repeat(40))
        }
    }
    let source_player = Cassette::replay(dir.path(), Mode::Strict).expect("loads");
    assert!(
        source_player
            .review(review_request, &SameRevisionDifferentSource, &policy,)
            .await
            .is_err()
    );
    assert_eq!(source_player.strict_misses(), 1);
    assert_eq!(source_player.cost_usd(), 0.0);
}

#[cfg(feature = "harness")]
#[tokio::test]
async fn recording_agentic_review_keeps_the_physical_cap_across_repository_reads() {
    use crate::harness::fake_gateway::{FakeGateway, Reply};
    use crate::harness::mock::execution_test_support::execution_control;
    use crate::ports::model::{Model, ModelRequest};
    use crate::ports::model_execution::ModelExecutionRefusal;
    use crate::ports::tree::MockTree;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let _guard = crate::harness::agentic::TEST_LOCK.lock().await;
    let gateway = FakeGateway::start(vec![
        Reply {
            status: 200,
            body: json!({
                "id":"cassette-agentic-admission",
                "object":"chat.completion",
                "model":"b",
                "choices":[{"index":0,"message":{"role":"assistant","content":null,"tool_calls":[{
                    "id":"read-1","type":"function","function":{
                        "name":"repo_read","arguments":json!({"path":"src/lib.rs","start_line":1,"end_line":1}).to_string()
                    }
                }]} ,"finish_reason":"tool_calls"}],
                "usage":{"prompt_tokens":12,"completion_tokens":3,"total_tokens":15,"cost":0.001}
            }),
        },
        Reply::completion(
            "b",
            r#"{"summary":"checked"}"#,
            "stop",
            json!({"prompt_tokens":16,"completion_tokens":4,"cost":0.002}),
        ),
    ])
    .await;
    let mut config = crate::config::types::Models::default();
    config.agentic_reviewers = true;
    config.base_url = gateway.base_url.clone();
    config.fallback = vec!["fallback".into()];
    config.provider = crate::config::types::ProviderRouting {
        order: vec!["pinned-provider".into()],
        allow_fallbacks: false,
        last_resort_unpinned: true,
        unpinned_vendors: vec![],
    };
    for model in ["b", "fallback"] {
        config.budget_prices.insert(
            model.into(),
            crate::config::types::BudgetPriceBound {
                input: 0.0,
                cached: 0.0,
                output: 100.0,
            },
        );
    }
    let gateway_model = crate::harness::embed::GatewayModel::with_key(&config, "fixture".into());
    let (execution, control) = execution_control(1);
    let dir = tempfile::tempdir().expect("temporary cassette directory");
    let cassette = Arc::new(Cassette::record(Arc::new(gateway_model), dir.path()));
    let model: Arc<dyn Model> = cassette.clone();
    let model = model
        .with_execution_control(control)
        .expect("recording forwards the shared physical execution control");
    let tree = CountingReviewTree {
        inner: MockTree::from_files([("src/lib.rs", "pub fn answer() {}")]),
        lookups: AtomicUsize::new(0),
    };
    let physical_events = Arc::new(ModelEventCounter::default());
    let request = ModelRequest {
        model: "b".into(),
        messages: vec![
            Message::system("Review this evidence."),
            Message::user("Check the implementation."),
        ],
        schema: json!({
            "type":"object", "additionalProperties":false,
            "properties":{"summary":{"type":"string"}}, "required":["summary"]
        }),
        schema_name: "review".into(),
        reasoning_effort: None,
        max_tokens: 128,
    };
    let result = crate::ports::model::with_model_call_observer(
        physical_events.clone(),
        model.review(
            request,
            &tree,
            &crate::config::types::LookupPolicy::default(),
        ),
    )
    .await;

    assert_eq!(tree.lookups.load(Ordering::SeqCst), 1);
    assert_eq!(gateway.requests().len(), 1);
    assert_eq!(execution.snapshot().committed_calls, 1);
    let refusal = result.expect_err("the next agentic turn exceeds the physical call cap");
    assert!(matches!(
        refusal,
        Error::ModelExecutionRefused {
            refusal: ModelExecutionRefusal::CallLimit,
            ..
        }
    ));
    let usage = refusal.usage().expect("the paid read remains reportable");
    assert_eq!(usage.input_tokens, 12);
    assert_eq!(usage.output_tokens, 3);
    assert_eq!(usage.calls_attempted, 1);
    assert_eq!(usage.calls_succeeded, 1);
    assert_eq!(usage.cost_usd, 0.001);
    assert_eq!(refusal.usage_accounting_complete(), Some(true));
    let physical_events = physical_events.0.lock().expect("model event lock");
    assert_eq!(physical_events.len(), 1);
    assert_eq!(physical_events[0].input_tokens, 12);
    assert_eq!(physical_events[0].output_tokens, 3);
    assert_eq!(physical_events[0].calls_attempted, 1);
    assert_eq!(physical_events[0].calls_succeeded, 1);
    assert_eq!(physical_events[0].cost_usd, 0.001);
    assert_eq!(cassette.spend().usage.input_tokens, 12);
    assert_eq!(cassette.spend().usage.output_tokens, 3);
    assert_eq!(cassette.cost_usd(), 0.001);
}

#[cfg(feature = "harness")]
#[tokio::test]
async fn cancelled_cassette_dispatch_with_no_paid_call_is_not_unknown_usage() {
    use crate::harness::fake_gateway::FakeGateway;
    use crate::harness::mock::execution_test_support::execution_control;
    use crate::ports::model::Model;
    use crate::ports::model_execution::ModelExecutionRefusal;

    let gateway = FakeGateway::start(vec![]).await;
    let config = crate::config::types::Models {
        base_url: gateway.base_url.clone(),
        ..Default::default()
    };
    let gateway_model = crate::harness::embed::GatewayModel::with_key(&config, "fixture".into());
    let (execution, control) = execution_control(1);
    execution.cancel();
    let dir = tempfile::tempdir().expect("temporary cassette directory");
    let cassette = Arc::new(Cassette::record(Arc::new(gateway_model), dir.path()));
    let model: Arc<dyn Model> = cassette.clone();
    let model = model
        .with_execution_control(control)
        .expect("recording forwards the physical execution control");
    let events = Arc::new(ModelEventCounter::default());
    let result = crate::ports::model::with_model_call_observer(
        events.clone(),
        model.complete(request("b", "cancel before dispatch")),
    )
    .await;

    assert!(gateway.requests().is_empty());
    assert_eq!(execution.snapshot().committed_calls, 0);
    assert_eq!(execution.snapshot().reserved_calls, 0);
    assert!(events.0.lock().expect("model event lock").is_empty());
    let refusal = result.expect_err("cancelled execution refuses before dispatch");
    assert!(matches!(
        &refusal,
        Error::ModelExecutionRefused {
            refusal: ModelExecutionRefusal::Cancelled,
            usage: None,
            accounting_complete: true,
        }
    ));
    assert_eq!(refusal.usage(), None);
    assert_eq!(refusal.usage_accounting_complete(), None);
    assert!(!cassette.has_unreported_usage());
    assert_eq!(cassette.served(), 0);
}

struct ConcurrentScopedModel {
    calls: Arc<std::sync::atomic::AtomicUsize>,
    start: Arc<tokio::sync::Barrier>,
}

#[async_trait::async_trait]
impl Model for ConcurrentScopedModel {
    async fn complete(&self, request: ModelRequest) -> Result<ModelResponse> {
        self.start.wait().await;
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if request
            .messages
            .last()
            .is_some_and(|message| message.content == "fail")
        {
            return Err(Error::Model("scoped refusal".into()).with_usage(Usage {
                input_tokens: 50,
                output_tokens: 7,
                cost_usd: 0.005,
                ..Default::default()
            }));
        }
        Ok(ModelResponse {
            value: answer(),
            model: "test/concurrent".into(),
            usage: Usage {
                input_tokens: 10,
                output_tokens: 2,
                cost_usd: 0.001,
                ..Default::default()
            },
        })
    }

    fn scoped_budget(&self, budget_usd: f64) -> Option<Arc<dyn Model>> {
        assert!(budget_usd > 0.0);
        Some(Arc::new(Self {
            calls: self.calls.clone(),
            start: self.start.clone(),
        }))
    }
}

#[tokio::test]
async fn concurrent_scoped_clones_share_recorded_takes_and_usage_errors() {
    let dir = tempfile::tempdir().expect("tempdir");
    let model = Arc::new(ConcurrentScopedModel {
        calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        start: Arc::new(tokio::sync::Barrier::new(3)),
    });
    let recorder = Arc::new(Cassette::record(model.clone(), dir.path()));
    let first_scope = recorder.scoped_budget(1.0).expect("first scoped model");
    let second_scope = recorder.scoped_budget(1.0).expect("second scoped model");

    let (root_result, first_result, second_result) = tokio::join!(
        recorder.complete(request("test/concurrent", "root")),
        first_scope.complete(request("test/concurrent", "scope one")),
        second_scope.complete(request("test/concurrent", "fail")),
    );

    assert!(root_result.is_ok());
    assert!(first_result.is_ok());
    assert!(second_result.is_err());
    assert_eq!(model.calls.load(std::sync::atomic::Ordering::SeqCst), 3);
    assert_eq!(recorder.served(), 2);
    assert_eq!(recorder.spend().usage.input_tokens, 70);
    assert_eq!(recorder.cost_usd(), 0.007);
    assert_eq!(recorder.flush().expect("root sees both scoped takes"), 2);
    assert_eq!(
        std::fs::read_dir(dir.path()).expect("cassette dir").count(),
        2
    );
}

struct ScopedPhysicalModel {
    flags: (bool, bool),
    scoped_flags: (bool, bool),
    events: Arc<std::sync::Mutex<Vec<&'static str>>>,
}

#[async_trait::async_trait]
impl Model for ScopedPhysicalModel {
    async fn complete(&self, _request: ModelRequest) -> Result<ModelResponse> {
        self.events.lock().expect("event log").push("complete");
        if self.flags.0
            && let Some(observer) = crate::ports::model::current_model_call_observer()
        {
            observer.completed(Usage::default(), true, false, std::time::Duration::ZERO);
        }
        Ok(ModelResponse {
            value: answer(),
            model: "test/scoped-physical".into(),
            usage: Usage {
                calls_attempted: 1,
                calls_succeeded: 1,
                cost_usd: 0.001,
                ..Default::default()
            },
        })
    }

    async fn review(
        &self,
        _request: ModelRequest,
        _tree: &dyn TreeReader,
        _policy: &crate::config::types::LookupPolicy,
    ) -> Result<ModelResponse> {
        self.events.lock().expect("event log").push("review");
        if self.flags.1
            && let Some(observer) = crate::ports::model::current_model_call_observer()
        {
            observer.completed(Usage::default(), true, false, std::time::Duration::ZERO);
        }
        Ok(ModelResponse {
            value: answer(),
            model: "test/scoped-physical".into(),
            usage: Usage {
                calls_attempted: 1,
                calls_succeeded: 1,
                cost_usd: 0.002,
                ..Default::default()
            },
        })
    }

    fn complete_reports_physical_calls(&self) -> bool {
        self.flags.0
    }

    fn review_reports_physical_calls(&self) -> bool {
        self.flags.1
    }

    fn scoped_budget(&self, budget_usd: f64) -> Option<Arc<dyn Model>> {
        assert!(budget_usd > 0.0);
        Some(Arc::new(Self {
            flags: self.scoped_flags,
            scoped_flags: self.scoped_flags,
            events: self.events.clone(),
        }))
    }
}

#[tokio::test]
async fn physical_call_capabilities_follow_each_live_scope_and_replay_emits_none() {
    use crate::ports::model::with_model_call_observer;

    let dir = tempfile::tempdir().expect("tempdir");
    let events = Arc::new(std::sync::Mutex::new(Vec::new()));
    let recorder = Arc::new(Cassette::record(
        Arc::new(ScopedPhysicalModel {
            flags: (true, false),
            scoped_flags: (false, true),
            events: events.clone(),
        }),
        dir.path(),
    ));
    assert!(recorder.complete_reports_physical_calls());
    assert!(!recorder.review_reports_physical_calls());

    let observer = Arc::new(PhysicalEventCounter(std::sync::atomic::AtomicUsize::new(0)));
    with_model_call_observer(
        observer.clone(),
        recorder.complete(request("test/scoped-physical", "root complete")),
    )
    .await
    .expect("records root completion");
    with_model_call_observer(
        observer.clone(),
        recorder.review(
            request("test/scoped-physical", "root review"),
            &ReviewTree,
            &crate::config::types::LookupPolicy::default(),
        ),
    )
    .await
    .expect("records root review");

    let scoped = recorder.scoped_budget(1.0).expect("scoped recorder");
    assert!(!scoped.complete_reports_physical_calls());
    assert!(scoped.review_reports_physical_calls());
    with_model_call_observer(
        observer.clone(),
        scoped.complete(request("test/scoped-physical", "scoped complete")),
    )
    .await
    .expect("records scoped completion");
    with_model_call_observer(
        observer.clone(),
        scoped.review(
            request("test/scoped-physical", "scoped review"),
            &ReviewTree,
            &crate::config::types::LookupPolicy::default(),
        ),
    )
    .await
    .expect("records scoped review");

    assert_eq!(observer.0.load(std::sync::atomic::Ordering::Relaxed), 2);
    assert!((recorder.cost_usd() - 0.006).abs() < 1e-12);
    assert_eq!(recorder.flush().expect("flushes scoped takes"), 4);
    assert_eq!(
        events.lock().expect("model events").as_slice(),
        ["complete", "review", "complete", "review"]
    );

    let replay = Cassette::replay(dir.path(), Mode::Strict).expect("loads physical tape");
    assert!(!replay.complete_reports_physical_calls());
    assert!(!replay.review_reports_physical_calls());
    with_model_call_observer(
        observer.clone(),
        replay.complete(request("test/scoped-physical", "root complete")),
    )
    .await
    .expect("replays completion");
    assert_eq!(observer.0.load(std::sync::atomic::Ordering::Relaxed), 2);
}

struct ScopedQuotaModel {
    calls: Arc<std::sync::atomic::AtomicUsize>,
    scopes: Arc<std::sync::atomic::AtomicUsize>,
    reviews: Arc<std::sync::atomic::AtomicUsize>,
}

#[async_trait::async_trait]
impl Model for ScopedQuotaModel {
    async fn complete(&self, _request: ModelRequest) -> Result<ModelResponse> {
        let call = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if call > 0 {
            return Err(Error::Model("scope exhausted".into()).with_usage(Usage {
                input_tokens: 50,
                output_tokens: 7,
                cost_usd: 0.005,
                ..Default::default()
            }));
        }
        Ok(ModelResponse {
            value: answer(),
            model: "test/scoped".into(),
            usage: Usage {
                input_tokens: 100,
                output_tokens: 12,
                cost_usd: 0.01,
                ..Default::default()
            },
        })
    }

    async fn review(
        &self,
        request: ModelRequest,
        tree: &dyn TreeReader,
        _policy: &crate::config::types::LookupPolicy,
    ) -> Result<ModelResponse> {
        self.reviews
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        tree.lookup(&Lookup::Read {
            path: "src/lib.rs".into(),
            start: Some(1),
            end: Some(2),
        })
        .await?;
        self.complete(request).await
    }

    fn scoped_budget(&self, budget_usd: f64) -> Option<Arc<dyn Model>> {
        assert!(budget_usd > 0.0);
        self.scopes
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Some(Arc::new(Self {
            calls: self.calls.clone(),
            scopes: self.scopes.clone(),
            reviews: self.reviews.clone(),
        }))
    }
}

#[tokio::test]
async fn scoped_review_and_direct_calls_share_the_root_cassette_and_refusal_spend() {
    let dir = tempfile::tempdir().expect("tempdir");
    let model = Arc::new(ScopedQuotaModel {
        calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        scopes: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        reviews: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
    });
    let recorder = Arc::new(Cassette::record(model.clone(), dir.path()));
    let models = crate::config::types::Models {
        agentic_reviewers: true,
        budget_usd_per_pr: 100.0,
        ..Default::default()
    };
    let capability = crate::flows::caps::ModelCapability::new(recorder.clone(), models);
    let policy = crate::config::types::LookupPolicy::default();
    let call = crate::flows::panel::Call {
        id: "reviewer".into(),
        model: "test/scoped".into(),
        system: "inspect".into(),
        prompt: "change evidence".into(),
        schema_name: "tinysweeper_critique".into(),
    };
    capability
        .review(&call, &json!({}), &ReviewTree, &policy)
        .await
        .expect("first call goes through scoped review");
    let err = capability
        .model()
        .complete(request("test/scoped", "direct follow-up"))
        .await
        .expect_err("the same finite model scope refuses the second call");

    assert!(err.to_string().contains("scope exhausted"));
    assert!(model.scopes.load(std::sync::atomic::Ordering::SeqCst) >= 1);
    assert_eq!(model.reviews.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(model.calls.load(std::sync::atomic::Ordering::SeqCst), 2);
    assert_eq!(recorder.served(), 1);
    assert_eq!(recorder.spend().usage.input_tokens, 150);
    assert_eq!(recorder.cost_usd(), 0.015);
    assert_eq!(recorder.flush().expect("root sees scoped response"), 1);
}

#[cfg(feature = "harness")]
struct CountingReviewTree {
    inner: crate::ports::tree::MockTree,
    lookups: std::sync::atomic::AtomicUsize,
}

#[cfg(feature = "harness")]
#[async_trait::async_trait]
impl TreeReader for CountingReviewTree {
    async fn lookup(&self, query: &Lookup) -> Result<Found> {
        self.lookups
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.inner.lookup(query).await
    }

    fn describe(&self) -> String {
        "cassette agentic review tree".into()
    }
}

fn request(model: &str, prompt: &str) -> ModelRequest {
    ModelRequest {
        model: model.to_string(),
        messages: vec![Message::system("instructions"), Message::user(prompt)],
        schema: json!({"type": "object"}),
        schema_name: "tinysweeper_critique".into(),
        reasoning_effort: None,
        max_tokens: 8000,
    }
}

fn answer() -> serde_json::Value {
    json!({"summary": "Nothing to report.", "findings": []})
}

#[test]
fn the_key_covers_everything_that_can_change_an_answer() {
    let base = request("z-ai/glm-5.2", "the diff");

    assert_eq!(key(&base), key(&request("z-ai/glm-5.2", "the diff")));

    let other_model = request("deepseek/deepseek-v4-pro", "the diff");
    assert_ne!(
        key(&base),
        key(&other_model),
        "the model decides the answer"
    );

    let other_prompt = request("z-ai/glm-5.2", "a different diff");
    assert_ne!(key(&base), key(&other_prompt), "so does the prompt");

    let mut other_schema = base.clone();
    other_schema.schema_name = "tinysweeper_falsify".into();
    assert_ne!(key(&base), key(&other_schema), "and the schema");

    // The same name with a different body is a different output contract: a
    // provider that answers against `properties: {summary: string}` does not
    // answer like one against `properties: {findings: []}`.
    let mut other_schema = base.clone();
    other_schema.schema = json!({"type": "object", "properties": {"findings": {"type": "array"}}});
    assert_ne!(key(&base), key(&other_schema), "and the schema body");

    let mut other_ceiling = base.clone();
    other_ceiling.max_tokens = 4000;
    assert_ne!(
        key(&base),
        key(&other_ceiling),
        "a model that runs out of tokens answers differently"
    );

    let mut other_effort = base.clone();
    other_effort.reasoning_effort = Some("low".into());
    assert_ne!(
        key(&base),
        key(&other_effort),
        "a role-specific reasoning override changes the model request"
    );
}

#[test]
fn the_key_is_the_schema_not_its_key_order() {
    // `serde_json`'s map order is a feature flag, not a property of the value.
    // The `serve` build enables `preserve_order` (via `bson`), so the same
    // contract stringifies with its keys in a different order than the offline
    // build. The key must hash the contract, not this build's serialization
    // quirk. Both schemas below are the same schema, keys written in different
    // order at two depths; without canonical sorting they hash differently
    // exactly in the `serve` CI job.
    let first = json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "summary": {"type": "string"},
            "findings": {"type": "array"}
        },
        "required": ["summary"]
    });
    let second = json!({
        "properties": {
            "findings": {"type": "array"},
            "summary": {"type": "string"}
        },
        "required": ["summary"],
        "additionalProperties": false,
        "type": "object"
    });

    let with_schema = |schema| {
        let mut r = request("z-ai/glm-5.2", "the diff");
        r.schema = schema;
        r
    };
    assert_eq!(
        key(&with_schema(first)),
        key(&with_schema(second)),
        "the key must hash the schema, not the build's key serialization order"
    );
}

#[test]
fn the_key_distinguishes_who_said_what() {
    // The same text as a system instruction and as user evidence is not the
    // same prompt: one is policy and the other is attacker-controlled.
    let mut swapped = request("z-ai/glm-5.2", "text");
    swapped.messages = vec![Message::user("instructions"), Message::system("text")];
    assert_ne!(key(&request("z-ai/glm-5.2", "text")), key(&swapped));
}

#[tokio::test]
async fn a_recording_replays_the_same_answer_the_same_cost_and_the_same_model() {
    let dir = tempfile::tempdir().expect("tempdir");

    let live = Arc::new(
        MockModel::always(answer())
            .answering_as("deepseek/deepseek-v4-pro")
            .with_usage(Usage {
                input_tokens: 1_200,
                output_tokens: 300,
                reasoning_tokens: 0,
                calls_attempted: 0,
                calls_succeeded: 0,
                calls_refused: 0,
                calls_errored: 0,
                cached_tokens: 900,
                embed_tokens: 0,
                cost_usd: 0.000_15,
            }),
    );
    let recorder = Cassette::record(live, dir.path());
    let recorded = recorder
        .complete(request("z-ai/glm-5.2", "the diff"))
        .await
        .expect("records");
    assert_eq!(recorder.flush().expect("flushes"), 1);

    let player = Cassette::replay(dir.path(), Mode::Strict).expect("loads");
    let replayed = player
        .complete(request("z-ai/glm-5.2", "the diff"))
        .await
        .expect("replays");

    assert_eq!(replayed.value, recorded.value);
    // The fallback is part of the recording: the run being reproduced is the
    // one where a cheaper model answered, not the one that was configured.
    assert_eq!(replayed.model, "deepseek/deepseek-v4-pro");
    // Replayed verbatim rather than re-derived through the price table, so a
    // re-score reports the dollars the live run actually paid.
    assert_eq!(replayed.usage.cost_usd, 0.000_15);
    assert_eq!(replayed.usage.cached_tokens, 900);
}

#[tokio::test]
async fn a_changed_prompt_is_a_loud_miss_rather_than_a_stale_answer() {
    let dir = tempfile::tempdir().expect("tempdir");
    let recorder = Cassette::record(Arc::new(MockModel::always(answer())), dir.path());
    recorder
        .complete(request("z-ai/glm-5.2", "the diff"))
        .await
        .expect("records");
    recorder.flush().expect("flushes");

    let player = Cassette::replay(dir.path(), Mode::Strict).expect("loads");
    let err = player
        .complete(request("z-ai/glm-5.2", "the diff, reworded"))
        .await
        .expect_err("must not serve the old answer");

    let message = err.to_string();
    // The message has to say what to do, because "the corpus is stale" is the
    // single most common thing this will report.
    assert!(message.contains("prompt changed"), "{message}");
    assert!(message.contains("re-record"), "{message}");
    assert!(message.contains("tinysweeper_critique"), "{message}");
}

#[tokio::test]
async fn a_loose_replay_survives_a_cosmetic_edit_and_counts_that_it_did() {
    let dir = tempfile::tempdir().expect("tempdir");
    let recorder = Cassette::record(Arc::new(MockModel::always(answer())), dir.path());
    recorder
        .complete(request("z-ai/glm-5.2", "the diff"))
        .await
        .expect("records");
    recorder.flush().expect("flushes");

    let player = Cassette::replay(dir.path(), Mode::Loose).expect("loads");
    let replayed = player
        .complete(request("z-ai/glm-5.2", "the  diff"))
        .await
        .expect("falls back to call order");

    assert_eq!(replayed.value, answer());
    // Counted, and reported, so a loose run is never mistaken for a strict one.
    assert_eq!(player.loose_hits(), 1);
}

#[tokio::test]
async fn an_exhausted_loose_cassette_says_how_far_it_got() {
    let dir = tempfile::tempdir().expect("tempdir");
    let recorder = Cassette::record(Arc::new(MockModel::always(answer())), dir.path());
    recorder
        .complete(request("z-ai/glm-5.2", "one"))
        .await
        .expect("records");
    recorder.flush().expect("flushes");

    let player = Cassette::replay(dir.path(), Mode::Loose).expect("loads");
    player
        .complete(request("z-ai/glm-5.2", "two"))
        .await
        .expect("first falls back");
    let err = player
        .complete(request("z-ai/glm-5.2", "three"))
        .await
        .expect_err("nothing left");

    assert!(err.to_string().contains("exhausted"), "{err}");
}

#[tokio::test]
async fn cost_usd_sums_every_answer_served_recorded_or_replayed() {
    let dir = tempfile::tempdir().expect("tempdir");
    let live = Arc::new(MockModel::always(answer()).with_usage(Usage {
        input_tokens: 100,
        output_tokens: 20,
        reasoning_tokens: 0,
        calls_attempted: 0,
        calls_succeeded: 0,
        calls_refused: 0,
        calls_errored: 0,
        cached_tokens: 0,
        embed_tokens: 0,
        cost_usd: 0.01,
    }));
    let recorder = Cassette::record(live, dir.path());
    recorder
        .complete(request("z-ai/glm-5.2", "one"))
        .await
        .expect("records");
    recorder
        .complete(request("z-ai/glm-5.2", "two"))
        .await
        .expect("records");
    assert_eq!(recorder.cost_usd(), 0.02);
    recorder.flush().expect("flushes");

    // Replay reports the same figure, because usage is replayed verbatim.
    let player = Cassette::replay(dir.path(), Mode::Strict).expect("loads");
    player
        .complete(request("z-ai/glm-5.2", "one"))
        .await
        .expect("replays");
    player
        .complete(request("z-ai/glm-5.2", "two"))
        .await
        .expect("replays");
    assert_eq!(player.cost_usd(), 0.02);

    // A strict miss served no answer and must cost nothing — a failed case is
    // charged only for what it actually used.
    let miss_dir = tempfile::tempdir().expect("tempdir");
    let recorder = Cassette::record(Arc::new(MockModel::always(answer())), miss_dir.path());
    recorder
        .complete(request("z-ai/glm-5.2", "known"))
        .await
        .expect("records");
    recorder.flush().expect("flushes");
    let strict = Cassette::replay(miss_dir.path(), Mode::Strict).expect("loads");
    strict
        .complete(request("z-ai/glm-5.2", "different"))
        .await
        .expect_err("must not serve the old answer");
    assert_eq!(strict.cost_usd(), 0.0);
}

#[tokio::test]
async fn calls_replay_in_the_order_they_were_made() {
    let dir = tempfile::tempdir().expect("tempdir");
    let live = MockModel::new()
        .then(json!({"n": 1}))
        .then(json!({"n": 2}))
        .then(json!({"n": 3}));
    let recorder = Cassette::record(Arc::new(live), dir.path());
    for prompt in ["one", "two", "three"] {
        recorder
            .complete(request("z-ai/glm-5.2", prompt))
            .await
            .expect("records");
    }
    assert_eq!(recorder.flush().expect("flushes"), 3);

    // Filename order is call order, which is what the loose fallback rides on.
    let player = Cassette::replay(dir.path(), Mode::Loose).expect("loads");
    for expected in 1..=3 {
        let got = player
            .complete(request("z-ai/glm-5.2", "unrecognised"))
            .await
            .expect("falls back in order");
        assert_eq!(got.value, json!({"n": expected}));
    }
}

#[tokio::test]
async fn two_identical_prompts_in_one_run_both_survive_the_write() {
    let dir = tempfile::tempdir().expect("tempdir");
    let recorder = Cassette::record(Arc::new(MockModel::always(answer())), dir.path());
    recorder
        .complete(request("z-ai/glm-5.2", "same"))
        .await
        .expect("records");
    recorder
        .complete(request("z-ai/glm-5.2", "same"))
        .await
        .expect("records");

    // Same key both times. Keying the filename on the hash alone would have
    // written one file and lost a call, which shows up later as a cassette
    // that runs out early.
    assert_eq!(recorder.flush().expect("flushes"), 2);
    let files = std::fs::read_dir(dir.path()).expect("readable").count();
    assert_eq!(files, 2);
}

#[tokio::test]
async fn a_prompt_is_not_written_to_disk_unless_it_is_asked_for() {
    let dir = tempfile::tempdir().expect("tempdir");
    let recorder = Cassette::record(Arc::new(MockModel::always(answer())), dir.path());
    recorder
        .complete(request("z-ai/glm-5.2", "a private repository's diff"))
        .await
        .expect("records");
    recorder.flush().expect("flushes");

    let written = std::fs::read_dir(dir.path())
        .expect("readable")
        .filter_map(|e| e.ok())
        .map(|e| std::fs::read_to_string(e.path()).unwrap_or_default())
        .collect::<String>();

    // A cassette committed from a private repository must not carry its diff.
    assert!(
        !written.contains("a private repository's diff"),
        "the prompt was written without being asked for: {written}"
    );

    let dir2 = tempfile::tempdir().expect("tempdir");
    let opted_in =
        Cassette::record(Arc::new(MockModel::always(answer())), dir2.path()).with_prompts(true);
    opted_in
        .complete(request("z-ai/glm-5.2", "a public repository's diff"))
        .await
        .expect("records");
    opted_in.flush().expect("flushes");
    let written2 = std::fs::read_dir(dir2.path())
        .expect("readable")
        .filter_map(|e| e.ok())
        .map(|e| std::fs::read_to_string(e.path()).unwrap_or_default())
        .collect::<String>();
    assert!(written2.contains("a public repository's diff"));
}

#[test]
fn replaying_a_directory_that_holds_no_cassette_says_how_to_make_one() {
    let dir = tempfile::tempdir().expect("tempdir");
    let err = Cassette::replay(dir.path().join("absent"), Mode::Strict).expect_err("no cassette");
    assert!(err.to_string().contains("eval run --record"), "{err}");
}

struct RefusingModel(Option<crate::ports::model::Usage>);

#[async_trait::async_trait]
impl Model for RefusingModel {
    async fn complete(&self, _request: ModelRequest) -> Result<ModelResponse> {
        let error = crate::error::Error::Model("refused".into());
        Err(match self.0 {
            Some(usage) => error.with_usage(usage),
            None => error,
        })
    }
}

#[tokio::test]
async fn reported_refusal_usage_is_counted_without_inventing_an_answer_model() {
    let dir = tempfile::tempdir().unwrap();
    let recorder = Cassette::record(
        Arc::new(RefusingModel(Some(crate::ports::model::Usage {
            input_tokens: 100,
            output_tokens: 10,
            cost_usd: 0.01,
            ..Default::default()
        }))),
        dir.path(),
    );
    assert!(
        recorder
            .complete(request("requested-model", "review"))
            .await
            .is_err()
    );
    assert_eq!(recorder.cost_usd(), 0.01);
    assert_eq!(recorder.spend().usage.input_tokens, 100);
    assert!(recorder.spend().models.is_empty());
    assert!(!recorder.has_unreported_usage());
    assert_eq!(
        recorder.flush().unwrap(),
        0,
        "A refusal is not an invented response cassette"
    );
}

#[tokio::test]
async fn unreported_refusal_charge_remains_explicitly_unknown() {
    let dir = tempfile::tempdir().unwrap();
    let recorder = Cassette::record(Arc::new(RefusingModel(None)), dir.path());
    assert!(
        recorder
            .complete(request("requested-model", "review"))
            .await
            .is_err()
    );
    assert_eq!(recorder.cost_usd(), 0.0);
    assert!(recorder.has_unreported_usage());
    assert!(recorder.spend().models.is_empty());
}

struct IncompleteTypedRefusalModel;

#[async_trait::async_trait]
impl Model for IncompleteTypedRefusalModel {
    async fn complete(&self, _request: ModelRequest) -> Result<ModelResponse> {
        Err(Error::ModelExecutionRefused {
            refusal: crate::ports::model_execution::ModelExecutionRefusal::Cancelled,
            usage: None,
            accounting_complete: false,
        })
    }
}

#[tokio::test]
async fn typed_refusal_without_usage_stays_unknown_when_accounting_is_incomplete() {
    let dir = tempfile::tempdir().expect("tempdir");
    let recorder = Cassette::record(Arc::new(IncompleteTypedRefusalModel), dir.path());
    let refusal = recorder
        .complete(request("requested-model", "review"))
        .await
        .expect_err("the model refuses");

    assert_eq!(refusal.usage(), None);
    assert_eq!(refusal.usage_accounting_complete(), None);
    assert_eq!(recorder.cost_usd(), 0.0);
    assert!(recorder.has_unreported_usage());
}

#[cfg(feature = "harness")]
#[tokio::test]
async fn partial_gateway_refusal_keeps_known_spend_and_marks_unknown_usage() {
    use crate::config::types::StructuredOutput;
    use crate::harness::fake_gateway::{FakeGateway, Reply};
    use crate::harness::mock::execution_test_support::execution_control;
    use crate::ports::model::{Model, ModelRequest};
    use crate::ports::model_execution::ModelExecutionRefusal;

    let _guard = crate::harness::agentic::TEST_LOCK.lock().await;
    let gateway = FakeGateway::start(vec![
        Reply::completion(
            "b",
            "{broken",
            "stop",
            json!({"prompt_tokens":5,"completion_tokens":2,"cost":0.125}),
        ),
        Reply::completion(
            "b",
            "{still broken",
            "stop",
            json!({"prompt_tokens":7,"completion_tokens":3}),
        ),
    ])
    .await;
    let mut config = crate::config::types::Models {
        base_url: gateway.base_url.clone(),
        structured_output: StructuredOutput::JsonObject,
        ..Default::default()
    };
    config.budget_prices.insert(
        "b".into(),
        crate::config::types::BudgetPriceBound {
            input: 0.0,
            cached: 0.0,
            output: 5_000.0,
        },
    );
    let gateway_model = crate::harness::embed::GatewayModel::with_key(&config, "fixture".into());
    let (execution, control) = execution_control(2);
    let dir = tempfile::tempdir().expect("temporary cassette directory");
    let cassette = Arc::new(Cassette::record(Arc::new(gateway_model), dir.path()));
    let model: Arc<dyn Model> = cassette.clone();
    let model = model
        .with_execution_control(control)
        .expect("cassette forwards execution control");
    let events = Arc::new(ModelEventCounter::default());
    let result = crate::ports::model::with_model_call_observer(
        events.clone(),
        model.complete(ModelRequest {
            model: "b".into(),
            messages: vec![
                Message::system("Return an object."),
                Message::user("bad JSON"),
            ],
            schema: json!({"type":"object"}),
            schema_name: "review".into(),
            reasoning_effort: None,
            max_tokens: 128,
        }),
    )
    .await;

    assert_eq!(gateway.requests().len(), 2);
    assert_eq!(execution.snapshot().committed_calls, 2);
    let refusal = result.expect_err("the third repair dispatch exceeds the cap");
    assert!(matches!(
        refusal,
        Error::ModelExecutionRefused {
            refusal: ModelExecutionRefusal::CallLimit,
            ..
        }
    ));
    let usage = refusal.usage().expect("known dispatch usage is retained");
    assert_eq!(usage.input_tokens, 12);
    assert_eq!(usage.output_tokens, 5);
    assert_eq!(usage.calls_attempted, 2);
    assert_eq!(usage.calls_succeeded, 2);
    assert_eq!(usage.cost_usd, 0.125);
    assert_eq!(refusal.usage_accounting_complete(), Some(false));
    assert_eq!(cassette.spend().usage.input_tokens, 12);
    assert_eq!(cassette.spend().usage.output_tokens, 5);
    assert_eq!(cassette.cost_usd(), 0.125);
    assert!(cassette.has_unreported_usage());
    let events = events.0.lock().expect("model event lock");
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].cost_usd, 0.125);
    assert_eq!(events[1].input_tokens, 7);
    assert_eq!(events[1].output_tokens, 3);
    // The outer observer may estimate a missing event cost; refusal accounting
    // above must retain only the provider-reported lower bound.
}
