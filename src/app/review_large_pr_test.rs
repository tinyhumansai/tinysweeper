//! End-to-end acceptance fixtures for bounded large pull-request reviews.

use crate::app::review::{Proposal, review};
use crate::config::types::{Config, CouncilAgent};
use crate::error::{Error, Result};
use crate::forge::mock::{MockForge, MockState, Write};
use crate::forge::types::{
    ChangedFile, FileStatus, PullRequest, RepoId, ReviewThread, ThreadComment,
};
use crate::harness::mock::MockModel;
use crate::ports::model::{Model, ModelRequest, ModelResponse, Usage};
use crate::ports::model_execution::ModelExecutionControl;
use serde_json::json;
use std::sync::{Arc, Mutex};

#[derive(Clone)]
struct LargeReviewModel {
    control: Option<Arc<dyn ModelExecutionControl>>,
    attempts: Arc<Mutex<Vec<ModelRequest>>>,
    calls: Arc<Mutex<Vec<ModelRequest>>>,
    finding: bool,
    advance_after: Option<&'static str>,
}

#[derive(Clone)]
struct AdaptiveCoverageRefusalModel {
    control: Option<Arc<dyn ModelExecutionControl>>,
    lane_schema: &'static str,
    attempts: Arc<Mutex<Vec<String>>>,
    calls: Arc<Mutex<Vec<String>>>,
    advance_after: Option<&'static str>,
}

#[async_trait::async_trait]
impl Model for AdaptiveCoverageRefusalModel {
    async fn complete(&self, request: ModelRequest) -> Result<ModelResponse> {
        let schema = request.schema_name.clone();
        self.attempts
            .lock()
            .expect("attempt list lock")
            .push(schema.clone());
        let control = self
            .control
            .as_ref()
            .ok_or(Error::ReviewExecutionUnavailable)?;
        let permit = control
            .acquire()
            .await
            .map_err(|refusal| Error::ModelExecutionRefused {
                refusal,
                usage: None,
                accounting_complete: true,
            })?;
        permit
            .mark_dispatched()
            .map_err(|refusal| Error::ModelExecutionRefused {
                refusal,
                usage: None,
                accounting_complete: true,
            })?;
        self.calls
            .lock()
            .expect("successful call list lock")
            .push(schema.clone());

        let prompt = request
            .messages
            .iter()
            .map(|message| message.content.as_str())
            .collect::<String>();
        let value = match schema.as_str() {
            "tinysweeper_critique" | "tinysweeper_security"
                if schema == self.lane_schema && prompt.contains("src/z_api.rs") =>
            {
                json!({
                    "summary": "The public API result should be documented.",
                    "findings": [{
                        "path": "src/z_api.rs",
                        "existing_code": "pub fn z_api() -> bool { true }",
                        "rule": "api-documentation",
                        "title": "Document the public API result",
                        "body": "The public result contract is not described.",
                        "severity": "medium",
                        "confidence": 0.95
                    }]
                })
            }
            "tinysweeper_critique" | "tinysweeper_security" => json!({
                "summary": "No additional issue was found.",
                "findings": []
            }),
            "tinysweeper_falsify" => json!({ "incorrect": [] }),
            schema => return Err(Error::Model(format!("unexpected schema {schema}"))),
        };
        drop(permit);
        if self.advance_after == Some(schema.as_str()) {
            tokio::time::advance(std::time::Duration::from_secs(80)).await;
        }
        Ok(ModelResponse {
            value,
            model: request.model,
            usage: Usage {
                input_tokens: 2,
                output_tokens: 1,
                cost_usd: 0.0001,
                ..Usage::default()
            },
        })
    }

    fn with_execution_control(
        &self,
        control: Arc<dyn ModelExecutionControl>,
    ) -> Option<Arc<dyn Model>> {
        let mut bound = self.clone();
        bound.control = Some(control);
        Some(Arc::new(bound))
    }
}

#[async_trait::async_trait]
impl Model for LargeReviewModel {
    async fn complete(&self, request: ModelRequest) -> Result<ModelResponse> {
        let advance_after = self.advance_after == Some(request.schema_name.as_str());
        self.attempts
            .lock()
            .expect("attempt list lock")
            .push(request.clone());
        let control = self
            .control
            .as_ref()
            .ok_or(Error::ReviewExecutionUnavailable)?;
        let permit = control
            .acquire()
            .await
            .map_err(|refusal| Error::ModelExecutionRefused {
                refusal,
                usage: None,
                accounting_complete: true,
            })?;
        permit
            .mark_dispatched()
            .map_err(|refusal| Error::ModelExecutionRefused {
                refusal,
                usage: None,
                accounting_complete: true,
            })?;
        self.calls
            .lock()
            .expect("successful call list lock")
            .push(request.clone());

        let prompt = request
            .messages
            .iter()
            .map(|message| message.content.as_str())
            .collect::<String>();
        let value = match request.schema_name.as_str() {
            "tinysweeper_critique" if self.finding && prompt.contains("src/z_api.rs") => {
                json!({
                    "summary": "The public API result should be documented.",
                    "findings": [{
                        "path": "src/z_api.rs",
                        "existing_code": "pub fn z_api() -> bool { true }",
                        "rule": "api-documentation",
                        "title": "Document the public API result",
                        "body": "The public result contract is not described.",
                        "severity": "medium",
                        "confidence": 0.95
                    }]
                })
            }
            "tinysweeper_critique" | "tinysweeper_security" => json!({
                "summary": "No additional issue was found.",
                "findings": []
            }),
            "tinysweeper_falsify" => json!({ "incorrect": [] }),
            "review_summary" => json!({
                "executive_summary": "The change updates the reviewed repository.",
                "changes": "The reviewed changes update source and interface files.",
                "features": [],
                "tests": [],
                "positive_observations": {}
            }),
            "tinysweeper_thread_resolution" => json!({
                "resolve": false,
                "reason": "The reply does not establish that the finding is fixed."
            }),
            "tinysweeper_wireframe" => json!({ "screens": [] }),
            schema => return Err(Error::Model(format!("unexpected schema {schema}"))),
        };
        drop(permit);
        if advance_after {
            tokio::time::advance(std::time::Duration::from_secs(80)).await;
        }
        Ok(ModelResponse {
            value,
            model: request.model,
            usage: Usage {
                input_tokens: 2,
                output_tokens: 1,
                cost_usd: 0.0001,
                ..Usage::default()
            },
        })
    }

    fn with_execution_control(
        &self,
        control: Arc<dyn ModelExecutionControl>,
    ) -> Option<Arc<dyn Model>> {
        let mut bound = self.clone();
        bound.control = Some(control);
        Some(Arc::new(bound))
    }
}

fn config() -> Config {
    let mut config: Config = crate::config::DEFAULTS
        .parse::<toml::Table>()
        .expect("default config parses")
        .try_into()
        .expect("default config converts");
    config.review.lanes = vec!["critique".into()];
    config.summary.enabled = true;
    config.knowledge.extract = false;
    config.retrieval.enabled = false;
    config
}

fn changed_file(path: &str) -> ChangedFile {
    ChangedFile {
        path: path.into(),
        status: FileStatus::Modified,
        additions: 1,
        deletions: 1,
        patch: Some(format!(
            "@@ -0,0 +1,1 @@\n+fn {}() {{}}\n",
            path.rsplit('/').next().unwrap().trim_end_matches(".rs")
        )),
        ..ChangedFile::default()
    }
}

fn large_fixture_files() -> Vec<ChangedFile> {
    let mut files = vec![
        ChangedFile {
            path: "src/x_api.rs".into(),
            status: FileStatus::Modified,
            additions: 1,
            deletions: 1,
            patch: Some("@@ -0,0 +1,1 @@\n+pub fn x_api() -> bool { true }\n".into()),
            ..ChangedFile::default()
        },
        ChangedFile {
            path: "src/y_api.rs".into(),
            status: FileStatus::Modified,
            additions: 1,
            deletions: 1,
            patch: Some("@@ -0,0 +1,1 @@\n+pub fn y_api() -> bool { true }\n".into()),
            ..ChangedFile::default()
        },
        ChangedFile {
            path: "src/z_api.rs".into(),
            status: FileStatus::Modified,
            additions: 1,
            deletions: 1,
            patch: Some("@@ -0,0 +1,1 @@\n+pub fn z_api() -> bool { true }\n".into()),
            ..ChangedFile::default()
        },
        ChangedFile {
            path: "app/src/Button.tsx".into(),
            status: FileStatus::Modified,
            additions: 1,
            deletions: 1,
            patch: Some("@@ -0,0 +1,1 @@\n+function Button() { return <button />; }\n".into()),
            ..ChangedFile::default()
        },
    ];
    files.extend((0..56).map(|index| ChangedFile {
        path: format!("src/plain_{index:02}.rs"),
        status: FileStatus::Modified,
        additions: 1,
        deletions: 1,
        patch: Some(format!("@@ -0,0 +1,1 @@\n+fn plain_{index}() {{}}\n")),
        ..ChangedFile::default()
    }));
    files
}

fn large_config() -> Config {
    let mut config = config();
    config.review.lanes = vec!["critique".into(), "security".into()];
    config.review.strictness = 3;
    config.review.passes = 3;
    config.council.enabled = true;
    config.council.agents = vec![CouncilAgent {
        id: "extra-reviewer".into(),
        ..CouncilAgent::default()
    }];
    config.review.large_pr.files = 30;
    config.review.large_pr.max_groups = 6;
    config.review.large_pr.max_calls = 8;
    config.grouping.enabled = false;
    config.review.approve_when_clean = true;
    config.automerge.enabled = true;
    config.threads.resolve_fixed = true;
    config.threads.ask_model = true;
    config.summary.enabled = true;
    config.wireframe.enabled = true;
    config
}

fn large_forge() -> MockForge {
    let mut state = MockState::default();
    state.pull_requests.insert(
        7,
        PullRequest {
            number: 7,
            title: "feat: bounded large review".into(),
            body: "Review the changed API and interface files.".into(),
            head_sha: "abc123".into(),
            ..PullRequest::default()
        },
    );
    state.files.insert(7, large_fixture_files());
    state.review_threads.insert(
        7,
        vec![ReviewThread {
            id: "PRRT_prior_bot_finding".into(),
            is_resolved: false,
            is_outdated: false,
            comments: vec![
                ThreadComment {
                    author: "tinysweeper[bot]".into(),
                    body: "**Existing invariant**".into(),
                    bot: true,
                    maintainer: false,
                },
                ThreadComment {
                    author: "maintainer".into(),
                    body: "This behavior is intentional.".into(),
                    bot: false,
                    maintainer: true,
                },
            ],
            ..ReviewThread::default()
        }],
    );
    MockForge::with_state(state)
}

async fn run_large_fixture(finding: bool) -> (Proposal, MockForge, Arc<LargeReviewModel>, Config) {
    let forge = large_forge();
    let config = large_config();
    let model = Arc::new(LargeReviewModel {
        control: None,
        attempts: Arc::new(Mutex::new(Vec::new())),
        calls: Arc::new(Mutex::new(Vec::new())),
        finding,
        advance_after: None,
    });
    let proposal = review(
        &forge,
        model.clone() as Arc<dyn Model>,
        &config,
        &RepoId::parse("tinyhumansai/tinysweeper").expect("repo id"),
        7,
    )
    .await
    .expect("the bounded large review returns a proposal");
    (proposal, forge, model, config)
}

fn all_default_lanes_config() -> Config {
    let mut config: Config = crate::config::DEFAULTS
        .parse::<toml::Table>()
        .expect("default config parses")
        .try_into()
        .expect("default config converts");
    config.review.large_pr.files = 1;
    config.review.large_pr.max_groups = 2;
    config.review.large_pr.max_calls = 3;
    config.review.strictness = 3;
    config.grouping.enabled = false;
    config.knowledge.extract = false;
    config.retrieval.enabled = false;
    config.summary.enabled = false;
    config.threads.resolve_fixed = false;
    config.threads.ask_model = false;
    config.wireframe.enabled = false;
    config.review.approve_when_clean = true;
    config.automerge.enabled = true;
    config
}

fn two_file_public_api_forge() -> MockForge {
    let mut state = MockState::default();
    state.pull_requests.insert(
        7,
        PullRequest {
            number: 7,
            title: "feat: expose an API result".into(),
            body: "Review the public API change and its behavior.".into(),
            head_sha: "abc123".into(),
            ..PullRequest::default()
        },
    );
    state.files.insert(
        7,
        vec![
            ChangedFile {
                path: "src/z_api.rs".into(),
                status: FileStatus::Modified,
                additions: 1,
                deletions: 1,
                patch: Some("@@ -0,0 +1,1 @@\n+pub fn z_api() -> bool { true }\n".into()),
                ..ChangedFile::default()
            },
            ChangedFile {
                path: "src/plain.rs".into(),
                status: FileStatus::Modified,
                additions: 1,
                deletions: 1,
                patch: Some("@@ -0,0 +1,1 @@\n+fn plain() {}\n".into()),
                ..ChangedFile::default()
            },
        ],
    );
    MockForge::with_state(state)
}

async fn run_all_default_lanes(
    forge: &MockForge,
    model: Arc<dyn Model>,
    config: &Config,
    execution: Arc<crate::app::review_execution::ReviewRunContext>,
    timing: crate::review_timing::ReviewTiming,
) -> Result<Proposal> {
    let model = execution
        .bind_model(model, config.models.budget_usd_per_pr)
        .model
        .clone();
    crate::app::review::review_with_tree_observed_at_head_in_run(
        forge,
        model,
        config,
        &RepoId::parse("tinyhumansai/tinysweeper").expect("repo id"),
        7,
        None,
        None,
        None,
        None,
        None,
        execution,
        Some(timing),
    )
    .await
}

fn f1_run_context(
    started_at: tokio::time::Instant,
    hard_deadline: tokio::time::Instant,
) -> Arc<crate::app::review_execution::ReviewRunContext> {
    Arc::new(crate::app::review_execution::ReviewRunContext::new(
        crate::app::review_execution::ReviewExecutionOptions {
            started_at,
            hard_deadline,
            mode: crate::lanes::large_pr::ReviewMode::Normal,
            max_calls: None,
            soft_deadline_percent: 80,
            drain_seconds: 5,
        },
    ))
}

fn f1_model(advance_after: Option<&'static str>) -> Arc<LargeReviewModel> {
    Arc::new(LargeReviewModel {
        control: None,
        attempts: Arc::new(Mutex::new(Vec::new())),
        calls: Arc::new(Mutex::new(Vec::new())),
        finding: true,
        advance_after,
    })
}

fn adaptive_normal_config(lane: &str, max_calls: usize) -> Config {
    let mut config = all_default_lanes_config();
    config.review.lanes = vec![lane.into()];
    config.review.passes = 3;
    config.review.large_pr.enabled = false;
    config.review.large_pr.max_calls = max_calls;
    config
}

fn one_large_api_forge() -> MockForge {
    let mut state = MockState::default();
    state.pull_requests.insert(
        7,
        PullRequest {
            number: 7,
            title: "feat: expose a documented API result".into(),
            body: "Review the public API change and its behavior.".into(),
            head_sha: "abc123".into(),
            ..PullRequest::default()
        },
    );
    let mut lines = vec!["pub fn z_api() -> bool { true }".to_string()];
    lines.extend((0..39).map(|index| format!("// changed line {index}")));
    let patch = format!(
        "@@ -0,0 +1,40 @@\n{}\n",
        lines
            .into_iter()
            .map(|line| format!("+{line}"))
            .collect::<Vec<_>>()
            .join("\n")
    );
    state.files.insert(
        7,
        vec![ChangedFile {
            path: "src/z_api.rs".into(),
            status: FileStatus::Modified,
            additions: 40,
            patch: Some(patch),
            ..ChangedFile::default()
        }],
    );
    MockForge::with_state(state)
}

async fn assert_adaptive_refusal_is_partial(
    lane: &str,
    max_calls: usize,
    advance_after: Option<&'static str>,
    reason: crate::lanes::fanout::OmissionReason,
) {
    let mut config = adaptive_normal_config(lane, max_calls);
    config.review.strictness = 3;
    let forge = one_large_api_forge();
    let model = Arc::new(AdaptiveCoverageRefusalModel {
        control: None,
        lane_schema: match lane {
            "critique" => "tinysweeper_critique",
            "security" => "tinysweeper_security",
            _ => unreachable!("fixture only supports critique and security"),
        },
        attempts: Arc::new(Mutex::new(Vec::new())),
        calls: Arc::new(Mutex::new(Vec::new())),
        advance_after,
    });
    let started_at = tokio::time::Instant::now();
    let execution = Arc::new(crate::app::review_execution::ReviewRunContext::new(
        crate::app::review_execution::ReviewExecutionOptions {
            started_at,
            hard_deadline: started_at + std::time::Duration::from_secs(100),
            mode: crate::lanes::large_pr::ReviewMode::Normal,
            max_calls: Some(max_calls),
            soft_deadline_percent: 80,
            drain_seconds: 5,
        },
    ));
    let timing = crate::review_timing::ReviewTiming::new("tinyhumansai/tinysweeper", 7, "Normal");

    let proposal = run_all_default_lanes(
        &forge,
        model.clone() as Arc<dyn Model>,
        &config,
        execution,
        timing.clone(),
    )
    .await
    .expect("a host refusal during adaptive coverage publishes a partial proposal");

    {
        let calls = model.calls.lock().expect("successful call list lock");
        assert_eq!(calls.len(), 2, "round one and its falsifier are retained");
        assert!(calls.contains(&model.lane_schema.to_string()));
        assert!(calls.contains(&"tinysweeper_falsify".to_string()));
    }
    assert!(timing.snapshot().calls_refused >= 1);

    assert!(!proposal.complete());
    assert!(proposal.unreviewed.contains(&"src/z_api.rs".to_string()));
    let lane_id = match lane {
        "critique" => crate::config::types::LaneId::Critique,
        "security" => crate::config::types::LaneId::Security,
        _ => unreachable!(),
    };
    let lane_proposal = proposal
        .lanes
        .iter()
        .find(|candidate| candidate.lane == lane_id)
        .expect("enabled lane is published");
    assert_eq!(
        lane_proposal.conclusion,
        crate::forge::types::CheckConclusion::Neutral
    );
    assert!(!lane_proposal.completed);
    assert!(
        lane_proposal
            .unanswered
            .contains(&"src/z_api.rs".to_string())
    );
    assert!(
        lane_proposal
            .omissions
            .iter()
            .any(|omission| { omission.reason == reason && omission.paths == ["src/z_api.rs"] })
    );
    assert!(lane_proposal.findings.iter().any(|finding| {
        finding.title == "Document the public API result" && finding.path == "src/z_api.rs"
    }));
    assert_eq!(proposal.usage().input_tokens, 4);
    assert_eq!(proposal.usage().output_tokens, 2);
    assert!((proposal.usage().cost_usd - 0.0002).abs() < 1e-9);
    assert_f1_never_approves(&forge);
    assert_eq!(
        crate::app::apply::apply(&forge, &forge, &config, &proposal, None)
            .await
            .expect("partial review is published"),
        crate::app::apply::ApplyDisposition::Published
    );
    assert_f1_never_approves(&forge);
}

#[tokio::test]
async fn critique_adaptive_call_limit_preserves_findings_but_marks_coverage_unanswered() {
    assert_adaptive_refusal_is_partial(
        "critique",
        2,
        None,
        crate::lanes::fanout::OmissionReason::CallLimit,
    )
    .await;
}

#[tokio::test(start_paused = true)]
async fn critique_adaptive_soft_deadline_preserves_findings_but_marks_coverage_unanswered() {
    assert_adaptive_refusal_is_partial(
        "critique",
        48,
        Some("tinysweeper_falsify"),
        crate::lanes::fanout::OmissionReason::SoftDeadline,
    )
    .await;
}

#[tokio::test]
async fn security_adaptive_call_limit_preserves_findings_but_marks_coverage_unanswered() {
    assert_adaptive_refusal_is_partial(
        "security",
        2,
        None,
        crate::lanes::fanout::OmissionReason::CallLimit,
    )
    .await;
}

#[tokio::test(start_paused = true)]
async fn security_adaptive_soft_deadline_preserves_findings_but_marks_coverage_unanswered() {
    assert_adaptive_refusal_is_partial(
        "security",
        48,
        Some("tinysweeper_falsify"),
        crate::lanes::fanout::OmissionReason::SoftDeadline,
    )
    .await;
}

#[tokio::test]
async fn a_subagent_settling_call_limit_publishes_an_unanswered_normal_review() {
    let forge = one_large_api_forge();
    let mut config = adaptive_normal_config("critique", 2);
    config.council.subagents = true;
    config.review.passes = 1;
    let usage = Usage {
        input_tokens: 3,
        output_tokens: 1,
        cost_usd: 0.01,
        ..Usage::default()
    };
    let model = Arc::new(
        MockModel::new()
            .then(json!({
                "summary": "The provisional response needs context.",
                "findings": [],
                "questions": [{
                    "question": "Does the caller validate this?",
                    "why": "Validation changes whether the concern is real."
                }]
            }))
            .then(json!({
                "answer": "The available evidence says the caller validates it.",
                "confident": true
            }))
            .then(
                json!({ "summary": "This settling response must not be reached.", "findings": [] }),
            )
            .with_usage(usage),
    );
    let observed = model.clone();
    let started_at = tokio::time::Instant::now();
    let execution = Arc::new(crate::app::review_execution::ReviewRunContext::new(
        crate::app::review_execution::ReviewExecutionOptions {
            started_at,
            hard_deadline: started_at + std::time::Duration::from_secs(100),
            mode: crate::lanes::large_pr::ReviewMode::Normal,
            max_calls: Some(2),
            soft_deadline_percent: 80,
            drain_seconds: 5,
        },
    ));
    let timing = crate::review_timing::ReviewTiming::new("tinyhumansai/tinysweeper", 7, "Normal");

    let proposal = run_all_default_lanes(&forge, model, &config, execution, timing.clone())
        .await
        .expect("a refusal during settlement publishes the partial proposal");

    assert_eq!(observed.calls(), 2, "settlement is refused before dispatch");
    assert!(timing.snapshot().calls_refused >= 1);
    assert!(!proposal.complete());
    assert!(proposal.unreviewed.contains(&"src/z_api.rs".to_string()));
    let critique = proposal
        .lanes
        .iter()
        .find(|lane| lane.lane == crate::config::types::LaneId::Critique)
        .expect("the critique result is published");
    assert_eq!(
        critique.conclusion,
        crate::forge::types::CheckConclusion::Neutral
    );
    assert!(!critique.completed);
    assert!(critique.unanswered.contains(&"src/z_api.rs".to_string()));
    assert!(critique.omissions.iter().any(|omission| {
        omission.paths == ["src/z_api.rs"]
            && omission.reason == crate::lanes::fanout::OmissionReason::CallLimit
    }));
    assert_eq!(proposal.usage().input_tokens, 6);
    assert_eq!(proposal.usage().output_tokens, 2);
    assert!((proposal.usage().cost_usd - 0.02).abs() < f64::EPSILON);
    assert_f1_never_approves(&forge);
    assert_eq!(
        crate::app::apply::apply(&forge, &forge, &config, &proposal, None)
            .await
            .expect("partial review is publishable"),
        crate::app::apply::ApplyDisposition::Published
    );
    assert_f1_never_approves(&forge);
}

fn assert_host_stopped_lane(
    proposal: &Proposal,
    lane_id: crate::config::types::LaneId,
    reason: crate::lanes::fanout::OmissionReason,
) {
    let lane = proposal
        .lanes
        .iter()
        .find(|lane| lane.lane == lane_id)
        .expect("enabled lane is represented in the partial proposal");
    assert_eq!(
        lane.conclusion,
        crate::forge::types::CheckConclusion::Neutral
    );
    assert!(!lane.completed, "a refused lane did not complete a review");
    assert!(lane.unanswered.contains(&lane_id.check_name()));
    assert!(lane.omissions.iter().any(|omission| {
        omission.reason == reason && omission.paths.contains(&"src/z_api.rs".to_string())
    }));
}

fn assert_f1_kept_prior_review(proposal: &Proposal) {
    let critique = proposal
        .lanes
        .iter()
        .find(|lane| lane.lane == crate::config::types::LaneId::Critique)
        .expect("the completed critique lane is retained");
    assert!(critique.findings.iter().any(|finding| {
        finding.title == "Document the public API result" && finding.path == "src/z_api.rs"
    }));
    assert!(proposal.usage().input_tokens >= 6);
    assert!(proposal.usage().output_tokens >= 3);
    assert!((proposal.usage().cost_usd - 0.0003).abs() < 1e-9);
    assert!(!proposal.complete());
}

fn assert_f1_never_approves(forge: &MockForge) {
    assert!(forge.writes().iter().all(|write| !matches!(
        write,
        Write::Review {
            event: crate::forge::types::ReviewEvent::Approve,
            ..
        } | Write::Merged { .. }
    )));
}

#[tokio::test]
async fn default_tests_lane_refusal_publishes_the_completed_partial_review() {
    let mut config = all_default_lanes_config();
    config.review.large_pr.max_calls = 3;
    config.review.large_pr.max_groups = 2;
    let forge = two_file_public_api_forge();
    let model = f1_model(None);
    let started_at = tokio::time::Instant::now();
    let execution = f1_run_context(started_at, started_at + std::time::Duration::from_secs(900));
    let timing = crate::review_timing::ReviewTiming::new("tinyhumansai/tinysweeper", 7, "Large");

    let result = run_all_default_lanes(
        &forge,
        model.clone() as Arc<dyn Model>,
        &config,
        execution,
        timing.clone(),
    )
    .await;

    let attempts = model.attempts.lock().expect("attempt list lock").clone();
    let successful = model
        .calls
        .lock()
        .expect("successful call list lock")
        .clone();
    let schemas: Vec<_> = attempts
        .iter()
        .map(|request| request.schema_name.as_str())
        .collect();
    assert!(schemas.contains(&"tinysweeper_security"));
    assert!(schemas.contains(&"tinysweeper_critique"));
    assert!(schemas.contains(&"tinysweeper_falsify"));
    assert!(
        !schemas.contains(&"tinysweeper_tests"),
        "the logical host gate refuses Tests before invoking the backend"
    );
    assert_eq!(
        successful.len(),
        3,
        "two lanes and falsification spend the cap"
    );
    assert!(timing.snapshot().calls_refused >= 1);

    let proposal = result
        .expect("a CallLimit in the enabled Tests lane must publish prior completed lane results");
    assert_f1_kept_prior_review(&proposal);
    assert_host_stopped_lane(
        &proposal,
        crate::config::types::LaneId::Tests,
        crate::lanes::fanout::OmissionReason::CallLimit,
    );
    assert_f1_never_approves(&forge);
    let applied = crate::app::apply::apply(&forge, &forge, &config, &proposal, None)
        .await
        .expect("an honest partial proposal remains publishable");
    assert_eq!(applied, crate::app::apply::ApplyDisposition::Published);
    assert_f1_never_approves(&forge);
}

#[tokio::test(start_paused = true)]
async fn default_tests_lane_soft_deadline_publishes_the_completed_partial_review() {
    let mut config = all_default_lanes_config();
    config.review.large_pr.max_calls = 48;
    config.review.large_pr.max_groups = 2;
    let forge = two_file_public_api_forge();
    let model = f1_model(Some("tinysweeper_falsify"));
    let started_at = tokio::time::Instant::now();
    let execution = f1_run_context(started_at, started_at + std::time::Duration::from_secs(100));
    let timing = crate::review_timing::ReviewTiming::new("tinyhumansai/tinysweeper", 7, "Large");

    let result = run_all_default_lanes(
        &forge,
        model.clone() as Arc<dyn Model>,
        &config,
        execution.clone(),
        timing.clone(),
    )
    .await;

    let attempts = model.attempts.lock().expect("attempt list lock").clone();
    let successful = model
        .calls
        .lock()
        .expect("successful call list lock")
        .clone();
    let schemas: Vec<_> = attempts
        .iter()
        .map(|request| request.schema_name.as_str())
        .collect();
    assert!(schemas.contains(&"tinysweeper_security"));
    assert!(schemas.contains(&"tinysweeper_critique"));
    assert!(schemas.contains(&"tinysweeper_falsify"));
    assert!(!schemas.contains(&"tinysweeper_tests"));
    assert_eq!(successful.len(), 3, "the completed work precedes soft time");
    assert_eq!(
        tokio::time::Instant::now(),
        started_at + std::time::Duration::from_secs(80)
    );
    assert!(timing.snapshot().calls_refused >= 1);
    assert_eq!(execution.logical_usage().input_tokens, 6);
    assert_eq!(execution.logical_usage().output_tokens, 3);
    assert!((execution.logical_usage().cost_usd - 0.0003).abs() < 1e-9);

    let proposal = result.expect(
        "a SoftDeadline in the enabled Tests lane must publish prior completed lane results",
    );
    assert_f1_kept_prior_review(&proposal);
    assert_host_stopped_lane(
        &proposal,
        crate::config::types::LaneId::Tests,
        crate::lanes::fanout::OmissionReason::SoftDeadline,
    );
    assert_f1_never_approves(&forge);
    let applied = crate::app::apply::apply(&forge, &forge, &config, &proposal, None)
        .await
        .expect("an honest partial proposal remains publishable");
    assert_eq!(applied, crate::app::apply::ApplyDisposition::Published);
    assert_f1_never_approves(&forge);
}

#[tokio::test(start_paused = true)]
async fn an_admitted_whole_pull_request_call_is_dropped_at_the_original_drain_deadline() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use tokio::sync::watch;

    struct HeldTestsModel {
        tests_started: watch::Sender<bool>,
        release_tests: watch::Receiver<bool>,
        tests_returned: Arc<AtomicBool>,
        tests_dropped: Arc<AtomicBool>,
    }

    struct DropProbe(Arc<AtomicBool>);

    impl Drop for DropProbe {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    #[async_trait::async_trait]
    impl Model for HeldTestsModel {
        async fn complete(&self, request: ModelRequest) -> Result<ModelResponse> {
            let mut value = match request.schema_name.as_str() {
                "tinysweeper_critique" => {
                    let prompt = request
                        .messages
                        .iter()
                        .map(|message| message.content.as_str())
                        .collect::<String>();
                    if prompt.contains("The file is `src/z_api.rs`.") {
                        json!({
                            "summary": "The public API result should be documented.",
                            "findings": [{
                                "path": "src/z_api.rs", "line": 1,
                                "existing_code": "pub fn z_api() -> bool { true }",
                                "rule": "api-documentation", "title": "Document the public API result",
                                "body": "The public result contract is not described.",
                                "severity": "medium", "confidence": 0.95
                            }]
                        })
                    } else {
                        json!({"summary": "No issue found.", "findings": []})
                    }
                }
                "tinysweeper_falsify" => json!({"incorrect": []}),
                "tinysweeper_tests" => {
                    let _drop_probe = DropProbe(self.tests_dropped.clone());
                    self.tests_started.send_replace(true);
                    let mut release = self.release_tests.clone();
                    while !*release.borrow() {
                        if release.changed().await.is_err() {
                            break;
                        }
                    }
                    self.tests_returned.store(true, Ordering::SeqCst);
                    json!({"summary": "No additional tests are needed.", "findings": []})
                }
                schema => return Err(Error::Model(format!("unexpected schema {schema}"))),
            };
            Ok(ModelResponse {
                value: std::mem::take(&mut value),
                model: request.model,
                usage: Usage {
                    input_tokens: 2,
                    output_tokens: 1,
                    cost_usd: 0.0001,
                    ..Usage::default()
                },
            })
        }
    }

    let forge = two_file_public_api_forge();
    let mut config = all_default_lanes_config();
    config.review.lanes = vec!["critique".into(), "tests".into()];
    config.review.large_pr.enabled = false;
    config.review.passes = 1;
    config.council.enabled = false;
    config.summary.enabled = false;
    let (tests_started_tx, tests_started_rx) = watch::channel(false);
    let (release_tests_tx, release_tests_rx) = watch::channel(false);
    let tests_returned = Arc::new(AtomicBool::new(false));
    let tests_dropped = Arc::new(AtomicBool::new(false));
    let model: Arc<dyn Model> = Arc::new(HeldTestsModel {
        tests_started: tests_started_tx,
        release_tests: release_tests_rx,
        tests_returned: tests_returned.clone(),
        tests_dropped: tests_dropped.clone(),
    });
    let started_at = tokio::time::Instant::now();
    let execution = f1_run_context(started_at, started_at + std::time::Duration::from_secs(100));
    let task_forge = forge.clone();
    let task_config = config.clone();
    let task_execution = execution.clone();
    let task = tokio::spawn(async move {
        let proposal = run_all_default_lanes(
            &task_forge,
            model,
            &task_config,
            task_execution,
            crate::review_timing::ReviewTiming::new("tinyhumansai/tinysweeper", 7, "Normal"),
        )
        .await?;
        let disposition =
            crate::app::apply::apply(&task_forge, &task_forge, &task_config, &proposal, None)
                .await?;
        Ok::<_, Error>((proposal, disposition, tokio::time::Instant::now()))
    });

    for _ in 0..2_000 {
        if *tests_started_rx.borrow() || task.is_finished() {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert!(*tests_started_rx.borrow(), "the whole-PR Tests call starts");

    for deadline in [80, 84] {
        let target = started_at + std::time::Duration::from_secs(deadline);
        tokio::time::advance(target - tokio::time::Instant::now()).await;
        for _ in 0..32 {
            tokio::task::yield_now().await;
        }
        assert_eq!(tokio::time::Instant::now(), target);
        assert!(
            !task.is_finished(),
            "the admitted whole-PR call is retained through t={deadline}"
        );
    }

    let drain_cutoff = started_at + std::time::Duration::from_secs(85);
    tokio::time::advance(drain_cutoff - tokio::time::Instant::now()).await;
    for _ in 0..128 {
        tokio::task::yield_now().await;
        if task.is_finished() {
            break;
        }
    }
    let finished_at_cutoff = task.is_finished();
    // On the unfixed implementation this releases the call so the spawned
    // task exits cleanly after the assertion reports the missing drain bound.
    release_tests_tx.send_replace(true);
    let (proposal, disposition, published_at) = task
        .await
        .expect("the app task stays alive")
        .expect("an interrupted Tests call produces a partial proposal");

    assert!(
        finished_at_cutoff,
        "the run must drop admitted post-processing work at the original t=85 drain cutoff"
    );
    assert_eq!(published_at, drain_cutoff);
    assert!(!tests_returned.load(Ordering::SeqCst));
    assert!(tests_dropped.load(Ordering::SeqCst));
    assert_eq!(disposition, crate::app::apply::ApplyDisposition::Published);
    assert!(!proposal.complete());
    let critique = proposal
        .lanes
        .iter()
        .find(|lane| lane.lane == crate::config::types::LaneId::Critique)
        .expect("the completed critique lane remains in the proposal");
    assert!(critique.findings.iter().any(|finding| {
        finding.path == "src/z_api.rs" && finding.title == "Document the public API result"
    }));
    assert_eq!(proposal.usage().input_tokens, 6);
    assert_eq!(proposal.usage().output_tokens, 3);
    assert!((proposal.usage().cost_usd - 0.0003).abs() < 1e-9);
    assert_host_stopped_lane(
        &proposal,
        crate::config::types::LaneId::Tests,
        crate::lanes::fanout::OmissionReason::DrainDeadline,
    );
    assert_f1_never_approves(&forge);
}

#[tokio::test(start_paused = true)]
async fn a_held_summary_falls_back_at_the_original_drain_deadline() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use tokio::sync::watch;

    struct HeldSummaryModel {
        started_at: tokio::time::Instant,
        summary_started: watch::Sender<bool>,
        release_summary: watch::Receiver<bool>,
        summary_returned: Arc<AtomicBool>,
        summary_dropped: Arc<AtomicBool>,
    }

    struct DropProbe(Arc<AtomicBool>);

    impl Drop for DropProbe {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    #[async_trait::async_trait]
    impl Model for HeldSummaryModel {
        async fn complete(&self, request: ModelRequest) -> Result<ModelResponse> {
            let value = match request.schema_name.as_str() {
                "tinysweeper_critique" => {
                    let prompt = request
                        .messages
                        .iter()
                        .map(|message| message.content.as_str())
                        .collect::<String>();
                    if prompt.contains("The file is `src/z_api.rs`.") {
                        json!({
                            "summary": "The public API result should be documented.",
                            "findings": [{
                                "path": "src/z_api.rs", "line": 1,
                                "existing_code": "pub fn z_api() -> bool { true }",
                                "rule": "api-documentation", "title": "Document the public API result",
                                "body": "The public result contract is not described.",
                                "severity": "high", "confidence": 0.95
                            }]
                        })
                    } else {
                        json!({"summary": "No issue found.", "findings": []})
                    }
                }
                "tinysweeper_falsify" => json!({"incorrect": []}),
                "review_summary" => {
                    let _drop_probe = DropProbe(self.summary_dropped.clone());
                    self.summary_started.send_replace(true);
                    let mut release = self.release_summary.clone();
                    while !*release.borrow() {
                        if release.changed().await.is_err() {
                            break;
                        }
                    }
                    self.summary_returned.store(true, Ordering::SeqCst);
                    json!({
                        "executive_summary": "This model-generated summary must not replace the fallback.",
                        "changes": "The change is reviewed.",
                        "features": [],
                        "tests": [],
                        "positive_observations": {}
                    })
                }
                schema => return Err(Error::Model(format!("unexpected schema {schema}"))),
            };

            if request.schema_name == "tinysweeper_falsify" {
                let target = self.started_at + std::time::Duration::from_secs(75);
                tokio::time::advance(target - tokio::time::Instant::now()).await;
            }

            Ok(ModelResponse {
                value,
                model: request.model,
                usage: Usage {
                    input_tokens: 2,
                    output_tokens: 1,
                    cost_usd: 0.0001,
                    ..Usage::default()
                },
            })
        }
    }

    let forge = two_file_public_api_forge();
    let mut config = config();
    config.review.lanes = vec!["critique".into()];
    config.review.large_pr.enabled = false;
    config.review.passes = 1;
    config.review.strictness = 3;
    config.council.enabled = false;
    config.summary.enabled = true;
    let (summary_started_tx, summary_started_rx) = watch::channel(false);
    let (release_summary_tx, release_summary_rx) = watch::channel(false);
    let summary_returned = Arc::new(AtomicBool::new(false));
    let summary_dropped = Arc::new(AtomicBool::new(false));
    let started_at = tokio::time::Instant::now();
    let model: Arc<dyn Model> = Arc::new(HeldSummaryModel {
        started_at,
        summary_started: summary_started_tx,
        release_summary: release_summary_rx,
        summary_returned: summary_returned.clone(),
        summary_dropped: summary_dropped.clone(),
    });
    let execution = f1_run_context(started_at, started_at + std::time::Duration::from_secs(100));
    let task_forge = forge.clone();
    let task_config = config.clone();
    let task_execution = execution.clone();
    let task = tokio::spawn(async move {
        let proposal = run_all_default_lanes(
            &task_forge,
            model,
            &task_config,
            task_execution,
            crate::review_timing::ReviewTiming::new("tinyhumansai/tinysweeper", 7, "Normal"),
        )
        .await?;
        let disposition =
            crate::app::apply::apply(&task_forge, &task_forge, &task_config, &proposal, None)
                .await?;
        Ok::<_, Error>((proposal, disposition, tokio::time::Instant::now()))
    });

    for _ in 0..2_000 {
        if *summary_started_rx.borrow() || task.is_finished() {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert!(
        *summary_started_rx.borrow(),
        "summary generation starts before t=80"
    );
    assert_eq!(
        tokio::time::Instant::now(),
        started_at + std::time::Duration::from_secs(75)
    );

    for deadline in [80, 84] {
        let target = started_at + std::time::Duration::from_secs(deadline);
        tokio::time::advance(target - tokio::time::Instant::now()).await;
        for _ in 0..32 {
            tokio::task::yield_now().await;
        }
        assert_eq!(tokio::time::Instant::now(), target);
        assert!(
            !task.is_finished(),
            "the summary remains in drain at t={deadline}"
        );
    }

    let drain_cutoff = started_at + std::time::Duration::from_secs(85);
    tokio::time::advance(drain_cutoff - tokio::time::Instant::now()).await;
    for _ in 0..128 {
        tokio::task::yield_now().await;
        if task.is_finished() {
            break;
        }
    }
    let finished_at_cutoff = task.is_finished();
    release_summary_tx.send_replace(true);
    let (proposal, disposition, published_at) = task
        .await
        .expect("the app task stays alive")
        .expect("a held advisory summary falls back deterministically");

    assert!(
        finished_at_cutoff,
        "the optional summary call is bounded by the original t=85 drain cutoff"
    );
    assert_eq!(published_at, drain_cutoff);
    assert!(!summary_returned.load(Ordering::SeqCst));
    assert!(summary_dropped.load(Ordering::SeqCst));
    assert_eq!(disposition, crate::app::apply::ApplyDisposition::Published);
    let summary = proposal
        .summary
        .as_ref()
        .expect("the host always has a deterministic summary fallback");
    assert!(summary
        .executive_summary
        .contains("Tiny Sweeper reviewed this change across 1 lane(s) and found 1 active actionable finding(s)."));
    assert!(
        !summary
            .executive_summary
            .contains("This model-generated summary must not replace the fallback.")
    );
    assert_eq!(proposal.usage().input_tokens, 6);
    assert_eq!(proposal.usage().output_tokens, 3);
    assert!((proposal.usage().cost_usd - 0.0003).abs() < 1e-9);
    assert!(proposal.lanes.iter().any(|lane| {
        lane.lane == crate::config::types::LaneId::Critique
            && lane.findings.iter().any(|finding| {
                finding.path == "src/z_api.rs" && finding.title == "Document the public API result"
            })
    }));
    assert_f1_never_approves(&forge);
}

fn selected_lane_paths(requests: &[ModelRequest]) -> std::collections::BTreeSet<(String, String)> {
    let mut selected = std::collections::BTreeSet::new();
    for request in requests {
        if !matches!(
            request.schema_name.as_str(),
            "tinysweeper_critique" | "tinysweeper_security"
        ) {
            continue;
        }
        let prompt = request
            .messages
            .iter()
            .map(|message| message.content.as_str())
            .collect::<String>();
        let path = ["src/x_api.rs", "src/y_api.rs", "src/z_api.rs"]
            .into_iter()
            .find(|path| prompt.contains(&format!("The file is `{path}`.")))
            .expect("lane request names one selected API unit");
        selected.insert((request.schema_name.clone(), path.to_string()));
    }
    selected
}

fn observed_group_paths(requests: &[ModelRequest]) -> std::collections::BTreeSet<(String, String)> {
    requests
        .iter()
        .filter(|request| {
            matches!(
                request.schema_name.as_str(),
                "tinysweeper_critique" | "tinysweeper_security"
            )
        })
        .map(|request| {
            let prompt = request
                .messages
                .iter()
                .map(|message| message.content.as_str())
                .collect::<String>();
            let path = prompt
                .split("The file is `")
                .nth(1)
                .and_then(|remainder| remainder.split("`.").next())
                .expect("per-file request identifies its selected path");
            (request.schema_name.clone(), path.to_string())
        })
        .collect()
}

fn replay_fixture_files(include_new_file: bool) -> Vec<ChangedFile> {
    let mut files = vec![
        ChangedFile {
            path: "src/a_api.rs".into(),
            status: FileStatus::Modified,
            additions: 1,
            deletions: 1,
            patch: Some("@@ -0,0 +1,1 @@\n+pub fn a_api() -> bool { true }\n".into()),
            ..ChangedFile::default()
        },
        changed_file("src/b_private.rs"),
    ];
    if include_new_file {
        files.push(changed_file("src/c_private.rs"));
    }
    files
}

fn replay_fixture_config() -> Config {
    let mut config = config();
    config.review.lanes = vec!["critique".into(), "security".into()];
    config.review.large_pr.files = 1;
    config.review.large_pr.max_groups = 2;
    config.review.large_pr.max_calls = 12;
    config.review.strictness = 3;
    config.grouping.enabled = false;
    config.knowledge.extract = false;
    config.retrieval.enabled = false;
    config.summary.enabled = false;
    config.threads.resolve_fixed = false;
    config.threads.ask_model = false;
    config.wireframe.enabled = false;
    config
}

fn replay_fixture_forge() -> MockForge {
    let mut state = MockState::default();
    state.pull_requests.insert(
        7,
        PullRequest {
            number: 7,
            title: "feat: review changed files".into(),
            body: "Review the changed source files.".into(),
            head_sha: "head-a".into(),
            ..PullRequest::default()
        },
    );
    state.files.insert(7, replay_fixture_files(false));
    MockForge::with_state(state)
}

#[tokio::test]
async fn large_review_does_not_replay_an_omitted_file_as_covered() {
    use crate::ports::review_state::ReviewStateStore;

    let forge = replay_fixture_forge();
    let store = crate::state::memory::MemoryState::new();
    let config = replay_fixture_config();
    let model = Arc::new(LargeReviewModel {
        control: None,
        attempts: Arc::new(Mutex::new(Vec::new())),
        calls: Arc::new(Mutex::new(Vec::new())),
        finding: false,
        advance_after: None,
    });
    let repo = RepoId::parse("tinyhumansai/tinysweeper").expect("repo id");

    let first = crate::app::review::review_with_state(
        &forge,
        model.clone(),
        &config,
        &repo,
        7,
        Some(&store),
    )
    .await
    .expect("first large review");
    assert!(first.unreviewed.contains(&"src/b_private.rs".to_string()));
    crate::app::apply::apply(&forge, &forge, &config, &first, Some(&store))
        .await
        .expect("first partial review publishes");

    let remembered = store
        .load_state("tinyhumansai/tinysweeper#7")
        .await
        .expect("state loads")
        .expect("first review saves replay evidence");
    assert!(remembered.evidence.contains("--- src/a_api.rs"));

    model.attempts.lock().expect("attempt list lock").clear();
    model
        .calls
        .lock()
        .expect("successful call list lock")
        .clear();
    forge.push(7, "head-b", replay_fixture_files(true));

    let second = crate::app::review::review_with_state(
        &forge,
        model.clone(),
        &config,
        &repo,
        7,
        Some(&store),
    )
    .await
    .expect("second large review");
    let requests = model
        .calls
        .lock()
        .expect("successful call list lock")
        .clone();
    let observed = observed_group_paths(&requests);

    assert_eq!(
        observed,
        std::collections::BTreeSet::from([
            ("tinysweeper_critique".into(), "src/b_private.rs".into()),
            ("tinysweeper_security".into(), "src/b_private.rs".into()),
        ]),
        "the public A file is valid replay, but previously omitted B must be rereviewed"
    );
    assert!(second.unreviewed.contains(&"src/c_private.rs".to_string()));
    crate::app::apply::apply(&forge, &forge, &config, &second, Some(&store))
        .await
        .expect("second review publishes");
}

#[tokio::test]
async fn sixty_file_empty_large_review_uses_six_global_groups_and_the_shared_call_ceiling() {
    let (proposal, forge, model, config) = run_large_fixture(false).await;
    let calls = model.calls.lock().expect("successful calls lock").clone();
    let expected: std::collections::BTreeSet<_> = [
        (
            "tinysweeper_critique".to_string(),
            "src/x_api.rs".to_string(),
        ),
        (
            "tinysweeper_security".to_string(),
            "src/x_api.rs".to_string(),
        ),
        (
            "tinysweeper_critique".to_string(),
            "src/y_api.rs".to_string(),
        ),
        (
            "tinysweeper_security".to_string(),
            "src/y_api.rs".to_string(),
        ),
        (
            "tinysweeper_critique".to_string(),
            "src/z_api.rs".to_string(),
        ),
        (
            "tinysweeper_security".to_string(),
            "src/z_api.rs".to_string(),
        ),
    ]
    .into_iter()
    .collect();
    assert_eq!(selected_lane_paths(&calls), expected);
    assert_eq!(selected_lane_paths(&calls).len(), 6);
    assert!(calls.len() <= config.review.large_pr.max_calls);
    assert_eq!(proposal.usage().calls_attempted, 8);
    assert!(
        calls
            .iter()
            .any(|request| request.schema_name == "review_summary")
    );
    assert!(
        calls
            .iter()
            .any(|request| request.schema_name == "tinysweeper_thread_resolution")
    );
    assert!(config.threads.resolve_fixed && config.threads.ask_model);
    assert!(config.wireframe.enabled);
    assert!(
        model
            .attempts
            .lock()
            .expect("attempt list lock")
            .iter()
            .all(|request| request.schema_name != "tinysweeper_wireframe")
    );
    assert!(
        proposal.wireframe.is_none(),
        "the shared ceiling refuses later optional work"
    );
    assert!(proposal.partial_review.is_some());
    assert!(!proposal.complete());

    let changed_paths: std::collections::BTreeSet<_> = large_fixture_files()
        .into_iter()
        .map(|file| file.path)
        .collect();
    assert_eq!(changed_paths.len(), 60);
    let reviewed_paths: std::collections::BTreeSet<_> = proposal
        .lanes
        .iter()
        .flat_map(|lane| lane.coverage.iter())
        .flat_map(|coverage| coverage.reviewed.iter().cloned())
        .collect();
    let expected_reviewed: std::collections::BTreeSet<_> = [
        "src/x_api.rs".to_string(),
        "src/y_api.rs".to_string(),
        "src/z_api.rs".to_string(),
    ]
    .into_iter()
    .collect();
    assert_eq!(reviewed_paths, expected_reviewed);
    let expected_unreviewed = changed_paths
        .difference(&expected_reviewed)
        .cloned()
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        proposal
            .unreviewed
            .iter()
            .cloned()
            .collect::<std::collections::BTreeSet<_>>(),
        expected_unreviewed,
        "coverage is per path across both lanes, so the six selected groups cover three files"
    );

    crate::app::apply::apply(&forge, &forge, &config, &proposal, None)
        .await
        .expect("partial proposal is published");
    let writes = forge.writes();
    assert!(!writes.iter().any(|write| matches!(
        write,
        Write::Review {
            event: crate::forge::types::ReviewEvent::Approve,
            ..
        } | Write::Merged { .. }
    )));
}

#[tokio::test]
async fn sixty_file_large_review_places_and_falsifies_a_selected_finding_without_approval() {
    let (proposal, forge, model, config) = run_large_fixture(true).await;
    let calls = model.calls.lock().expect("successful calls lock").clone();
    let expected: std::collections::BTreeSet<_> = [
        (
            "tinysweeper_critique".to_string(),
            "src/x_api.rs".to_string(),
        ),
        (
            "tinysweeper_security".to_string(),
            "src/x_api.rs".to_string(),
        ),
        (
            "tinysweeper_critique".to_string(),
            "src/y_api.rs".to_string(),
        ),
        (
            "tinysweeper_security".to_string(),
            "src/y_api.rs".to_string(),
        ),
        (
            "tinysweeper_critique".to_string(),
            "src/z_api.rs".to_string(),
        ),
        (
            "tinysweeper_security".to_string(),
            "src/z_api.rs".to_string(),
        ),
    ]
    .into_iter()
    .collect();
    assert_eq!(selected_lane_paths(&calls), expected);
    assert!(
        calls
            .iter()
            .any(|request| request.schema_name == "tinysweeper_falsify")
    );
    assert!(
        calls
            .iter()
            .any(|request| request.schema_name == "review_summary")
    );
    assert!(calls.len() <= config.review.large_pr.max_calls);
    assert_eq!(proposal.usage().calls_attempted, 8);
    assert!(
        proposal
            .lanes
            .iter()
            .flat_map(|lane| lane.findings.iter())
            .any(|finding| finding.title == "Document the public API result")
    );
    assert!(!proposal.complete());

    crate::app::apply::apply(&forge, &forge, &config, &proposal, None)
        .await
        .expect("finding and partial coverage are published");
    let writes = forge.writes();
    assert!(!writes.iter().any(|write| matches!(
        write,
        Write::Review {
            event: crate::forge::types::ReviewEvent::Approve,
            ..
        } | Write::Merged { .. }
    )));
    assert!(writes.iter().any(|write| matches!(write, Write::Check(_))));
    assert!(writes.iter().any(|write| matches!(write, Write::Comment { body, .. } if body.contains("Document the public API result"))));
}

#[cfg(feature = "harness")]
async fn wait_for_gateway_requests(
    gateway: &crate::harness::fake_gateway::FakeGateway,
    expected: usize,
) {
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while gateway.requests().len() < expected {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the Gateway receives every admitted physical request");
}

#[cfg(feature = "harness")]
fn gateway_empty_lane_reply(model: &str) -> crate::harness::fake_gateway::Reply {
    crate::harness::fake_gateway::Reply::completion(
        model,
        r#"{"summary":"No issue found.","findings":[]}"#,
        "stop",
        json!({"prompt_tokens":2,"completion_tokens":1,"cost":0.0001}),
    )
}

#[cfg(feature = "harness")]
fn assert_gateway_large_coverage(proposal: &Proposal) -> usize {
    use crate::app::review::PartialReviewReason;
    use crate::config::types::LaneId;
    use std::collections::BTreeSet;

    let reviewed_pairs = proposal
        .lanes
        .iter()
        .flat_map(|lane| {
            lane.coverage.iter().flat_map(move |coverage| {
                coverage.reviewed.iter().map(move |path| (lane.lane, path))
            })
        })
        .collect::<BTreeSet<_>>();
    assert_eq!(
        reviewed_pairs.len(),
        2,
        "only the two successful continuation responses count as reviewed groups"
    );
    let call_limited_pairs = proposal
        .partial_review
        .as_ref()
        .expect("the physical ceiling leaves partial-review metadata")
        .omitted
        .iter()
        .filter(|omission| omission.reason == PartialReviewReason::CallLimit)
        .filter_map(|omission| omission.lane.map(|lane| (lane, omission.path.as_str())))
        .collect::<BTreeSet<_>>();
    assert_eq!(
        call_limited_pairs.len(),
        4,
        "four selected lane/path groups have a typed call-limit omission"
    );
    let paths_for = |lane| {
        proposal
            .lanes
            .iter()
            .find(|proposal| proposal.lane == lane)
            .and_then(|proposal| proposal.coverage.as_ref())
            .map(|coverage| coverage.reviewed.iter().collect::<BTreeSet<_>>())
            .unwrap_or_default()
    };
    let critique = paths_for(LaneId::Critique);
    let security = paths_for(LaneId::Security);
    let fully_covered = critique
        .intersection(&security)
        .map(|path| (*path).clone())
        .collect::<BTreeSet<_>>();
    assert!(
        fully_covered.len() <= 1,
        "the six selected lane groups cover no more than one whole file"
    );
    let changed = large_fixture_files()
        .into_iter()
        .map(|file| file.path)
        .collect::<BTreeSet<_>>();
    assert_eq!(changed.len(), 60);
    assert_eq!(
        proposal.unreviewed.iter().cloned().collect::<BTreeSet<_>>(),
        changed
            .difference(&fully_covered)
            .cloned()
            .collect::<BTreeSet<_>>(),
        "a file is covered only when both lane groups completed"
    );
    fully_covered.len()
}

#[cfg(feature = "harness")]
#[tokio::test]
async fn sixty_file_gateway_repairs_share_the_physical_call_ceiling() {
    use crate::harness::embed::GatewayModel;
    use crate::harness::fake_gateway::{FakeGateway, Reply};

    let mut config = large_config();
    config.models.fallback.clear();
    config.models.provider.last_resort_unpinned = false;
    config.models.budget_usd_per_pr = 10.0;
    let primary = config.models.deep.clone();
    let mut replies = (0..6)
        .map(|_| {
            Reply::completion(
                &primary,
                "{ malformed JSON",
                "stop",
                json!({"prompt_tokens":2,"completion_tokens":1,"cost":0.0001}),
            )
        })
        .collect::<Vec<_>>();
    replies.extend((0..2).map(|_| gateway_empty_lane_reply(&primary)));
    let (gateway, release) = FakeGateway::start_paused(replies).await;
    config.models.base_url = gateway.base_url.clone();
    let forge = large_forge();
    let repo = RepoId::parse("tinyhumansai/tinysweeper").expect("repo id");
    let model: Arc<dyn Model> =
        Arc::new(GatewayModel::with_key(&config.models, "fixture-key".into()));
    let run_config = config.clone();
    let task = tokio::spawn(async move {
        let result = review(&forge, model, &run_config, &repo, 7).await;
        result.map(|proposal| (proposal, forge))
    });

    wait_for_gateway_requests(&gateway, 6).await;
    let initial = gateway.requests();
    assert!(
        initial[..6]
            .iter()
            .all(|request| request["model"] == primary)
    );
    release.add_permits(6);
    wait_for_gateway_requests(&gateway, 8).await;
    release.add_permits(2);

    let proposal = tokio::time::timeout(std::time::Duration::from_secs(10), task)
        .await
        .expect("the call-limited review finishes after its two repairs")
        .expect("review task joins")
        .expect("partial review is returned");
    let (proposal, forge) = proposal;
    let requests = gateway.requests();
    assert_eq!(
        requests.len(),
        8,
        "repairs consume the shared physical limit"
    );
    assert!(requests.iter().all(|request| request["model"] == primary));
    assert_eq!(proposal.usage().calls_attempted, 8);
    assert!(!proposal.complete());
    assert_eq!(
        proposal
            .partial_review
            .as_ref()
            .and_then(|partial| partial.stop_reason),
        Some(crate::app::review::PartialReviewReason::CallLimit)
    );
    let fully_covered = assert_gateway_large_coverage(&proposal);
    crate::app::apply::apply(&forge, &forge, &config, &proposal, None)
        .await
        .expect("the partial repair-limited proposal is published");
    let writes = forge.writes();
    let check = writes
        .iter()
        .find_map(|write| match write {
            Write::Check(check) | Write::CheckUpdate { check, .. } => Some(check),
            _ => None,
        })
        .expect("the check publishes retained coverage");
    assert_eq!(
        check.conclusion,
        Some(crate::forge::types::CheckConclusion::Neutral)
    );
    assert!(check.summary.contains("stopped at the physical-call limit"));
    let hub = writes.iter().find_map(|write| match write {
        Write::Comment { body, .. } | Write::CommentUpdate { body, .. } => Some(body),
        _ => None,
    });
    assert!(hub.is_some_and(|body| {
        body.contains("not reviewed (call budget)")
            && body.contains(&format!("Reviewed {fully_covered} of 60 files"))
    }));
    assert!(!writes.iter().any(|write| matches!(
        write,
        Write::Review {
            event: crate::forge::types::ReviewEvent::Approve,
            ..
        } | Write::Merged { .. }
    )));
}

#[cfg(feature = "harness")]
#[tokio::test]
async fn sixty_file_gateway_fallbacks_share_the_physical_call_ceiling() {
    use crate::harness::embed::GatewayModel;
    use crate::harness::fake_gateway::{FakeGateway, Reply};

    let mut config = large_config();
    let primary = config.models.deep.clone();
    let fallback = "z-ai/glm-5.2".to_string();
    config.models.fallback = vec![fallback.clone()];
    config.models.provider.last_resort_unpinned = false;
    config.models.budget_usd_per_pr = 10.0;
    let mut replies = (0..6)
        .map(|_| Reply::error(404, "No endpoints found"))
        .collect::<Vec<_>>();
    replies.extend((0..2).map(|_| gateway_empty_lane_reply(&fallback)));
    let (gateway, release) = FakeGateway::start_paused(replies).await;
    config.models.base_url = gateway.base_url.clone();
    let forge = large_forge();
    let repo = RepoId::parse("tinyhumansai/tinysweeper").expect("repo id");
    let model: Arc<dyn Model> =
        Arc::new(GatewayModel::with_key(&config.models, "fixture-key".into()));
    let run_config = config.clone();
    let task = tokio::spawn(async move {
        let result = review(&forge, model, &run_config, &repo, 7).await;
        result.map(|proposal| (proposal, forge))
    });

    wait_for_gateway_requests(&gateway, 6).await;
    let initial = gateway.requests();
    assert!(
        initial[..6]
            .iter()
            .all(|request| request["model"] == primary)
    );
    release.add_permits(6);
    wait_for_gateway_requests(&gateway, 8).await;
    release.add_permits(2);

    let proposal = tokio::time::timeout(std::time::Duration::from_secs(10), task)
        .await
        .expect("the call-limited review finishes after two fallback dispatches")
        .expect("review task joins")
        .expect("partial review is returned");
    let (proposal, forge) = proposal;
    let requests = gateway.requests();
    assert_eq!(
        requests.len(),
        8,
        "fallbacks consume the shared physical limit"
    );
    assert!(
        requests[..6]
            .iter()
            .all(|request| request["model"] == primary)
    );
    assert!(
        requests[6..]
            .iter()
            .all(|request| request["model"] == fallback)
    );
    assert_eq!(proposal.usage().calls_attempted, 8);
    assert!(!proposal.complete());
    assert_eq!(
        proposal
            .partial_review
            .as_ref()
            .and_then(|partial| partial.stop_reason),
        Some(crate::app::review::PartialReviewReason::CallLimit)
    );
    let fully_covered = assert_gateway_large_coverage(&proposal);
    crate::app::apply::apply(&forge, &forge, &config, &proposal, None)
        .await
        .expect("the partial fallback-limited proposal is published");
    let writes = forge.writes();
    let check = writes
        .iter()
        .find_map(|write| match write {
            Write::Check(check) | Write::CheckUpdate { check, .. } => Some(check),
            _ => None,
        })
        .expect("the check publishes retained coverage");
    assert_eq!(
        check.conclusion,
        Some(crate::forge::types::CheckConclusion::Neutral)
    );
    assert!(check.summary.contains("stopped at the physical-call limit"));
    let hub = writes.iter().find_map(|write| match write {
        Write::Comment { body, .. } | Write::CommentUpdate { body, .. } => Some(body),
        _ => None,
    });
    assert!(hub.is_some_and(|body| {
        body.contains("not reviewed (call budget)")
            && body.contains(&format!("Reviewed {fully_covered} of 60 files"))
    }));
    assert!(!writes.iter().any(|write| matches!(
        write,
        Write::Review {
            event: crate::forge::types::ReviewEvent::Approve,
            ..
        } | Write::Merged { .. }
    )));
}

#[tokio::test]
async fn a_partial_large_review_is_published_without_approval_and_keeps_host_gaps() {
    let mut state = MockState::default();
    state.pull_requests.insert(
        7,
        PullRequest {
            number: 7,
            title: "feat: partial review".into(),
            body: "Review two changed files.".into(),
            head_sha: "abc123".into(),
            ..PullRequest::default()
        },
    );
    state
        .files
        .insert(7, vec![changed_file("src/a.rs"), changed_file("src/b.rs")]);
    let forge = MockForge::with_state(state);
    let model: Arc<dyn Model> = Arc::new(MockModel::always(json!({
        "summary": "No finding.",
        "findings": [],
        "executive_summary": "The pull request changes two source files.",
        "changes": "",
        "features": [],
        "tests": [],
        "positive_observations": {}
    })));
    let mut config = config();
    config.grouping.enabled = false;
    config.review.large_pr.files = 1;
    config.review.large_pr.max_groups = 1;
    config.review.large_pr.max_calls = 8;
    config.review.approve_when_clean = true;

    let repo = RepoId::parse("tinyhumansai/tinysweeper").expect("repo id");
    let proposal = review(&forge, model, &config, &repo, 7)
        .await
        .expect("the selected group can be reviewed");
    let encoded = serde_json::to_value(&proposal).expect("proposal serializes");

    assert_eq!(
        encoded["partial_review"]["mode"], "large",
        "the published proposal records host-side partial-review metadata"
    );
    assert_eq!(encoded["partial_review"]["estimate"]["files"], 2);
    assert_eq!(
        encoded["partial_review"]["selected_groups"][0]["paths"],
        json!(["src/a.rs"])
    );
    assert!(
        encoded["partial_review"]["omitted"]
            .as_array()
            .unwrap()
            .iter()
            .any(|omission| omission["path"] == "src/b.rs" && omission["reason"] == "not_selected")
    );
    assert!(proposal.unreviewed.iter().any(|path| path == "src/b.rs"));
    assert!(
        !proposal.complete(),
        "an omitted changed path blocks approval"
    );
    let mut legacy = encoded.clone();
    legacy
        .as_object_mut()
        .expect("proposal is an object")
        .remove("partial_review");
    let decoded: Proposal = serde_json::from_value(legacy).expect("legacy proposal decodes");
    assert!(decoded.partial_review.is_none());

    crate::app::apply::apply(&forge, &forge, &config, &proposal, None)
        .await
        .expect("the partial proposal is published");
    let writes = forge.writes();
    assert!(
        !writes.iter().any(|write| matches!(
            write,
            Write::Review {
                event: crate::forge::types::ReviewEvent::Approve,
                ..
            }
        )),
        "publishing partial coverage must never approve"
    );
    assert!(
        writes.iter().any(|write| matches!(write, Write::Check(_))),
        "the partial check remains publishable"
    );
    let hub = writes.iter().find_map(|write| match write {
        Write::Comment { body, .. } | Write::CommentUpdate { body, .. } => Some(body),
        _ => None,
    });
    let hub = hub.expect("partial review hub is published");
    assert!(hub.contains("not reviewed (large PR)"), "{hub}");
    assert!(hub.contains("src/b.rs"), "{hub}");
    assert!(
        hub.contains("not selected by the large-review budget"),
        "{hub}"
    );
}
