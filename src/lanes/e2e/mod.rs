//! The `e2e` lane: whether changed behaviour is verified end to end, and
//! whether the repository's own end-to-end jobs ran on this head.
//!
//! The `tests` lane deliberately refuses to demand an integration or
//! end-to-end test. This lane owns exactly that concern, and nothing else:
//! the two are a partition, so an author is never told the same thing twice.
//!
//! It **does not run anything**. The same invariant as `tests`, held the same
//! way — this lane owns a `Model` and reads evidence someone else gathered.
//! The repository's own CI is the hands, in its own trust domain with its own
//! secrets; the check runs it reports on the head are what this lane reads.
//! See `docs/modules/lanes/e2e.md`.
//!
//! Deterministic first, model second. The harness inventory, the trigger
//! analysis and the job states come from `inventory` and `runs` before a
//! token is spent, and the two findings they produce — a job that will not
//! trigger, a job that failed — are republished unchanged. The model is asked
//! only the part that needs reading: does an end-to-end test actually drive
//! each behavioural change.
//!
//! Opt-in. Absent from the default `review.lanes`, because demanding an e2e
//! test from a repository with no harness is the noise the gates exist to
//! suppress.

pub mod evidence;
pub mod inventory;
pub mod runs;

use std::fmt::Write as _;
use std::sync::Arc;

use async_trait::async_trait;

use crate::config::types::{LaneId, Severity};
use crate::council;
use crate::error::Result;
use crate::evidence::diff::render as render_diffs;
use crate::findings::types::Finding;
use crate::flows::panel::Call;
use crate::flows::runner;
use crate::harness::prompt::{self, PromptInputs};
use crate::harness::schema;
use crate::lanes::tests::Inventory;
use crate::lanes::{
    Anchoring, Lane, LaneInput, LaneOutcome, aggregate_reviewer_responses, reviewer_responses,
};
use crate::ports::model::Model;

/// The `e2e` lane.
pub struct E2e {
    model: Arc<dyn Model>,
}

impl E2e {
    /// Build the lane over `model`.
    pub fn new(model: Arc<dyn Model>) -> Self {
        Self { model }
    }
}

/// The rule the model uses for a change it judges unreachable end to end.
///
/// Forced to `Low` whatever the model said: the finding exists so the
/// decision is recorded, and a lane that blocked a merge on "this cannot be
/// tested" would be blocking on its own inability to help.
const UNOBSERVABLE: &str = "e2e-unobservable";

#[async_trait]
impl Lane for E2e {
    fn id(&self) -> LaneId {
        LaneId::E2e
    }

    async fn run(&self, input: LaneInput<'_>) -> Result<LaneOutcome> {
        if !input.has_reviewable_content() {
            return Ok(LaneOutcome::skipped(
                "No added or modified lines to review.",
            ));
        }

        if let Some(skipped) = input.skip_as_draft() {
            return Ok(skipped);
        }

        // The same classification the `tests` lane uses, for the same reason:
        // a change with no behavioural component needs no end-to-end test,
        // and that is decided by a path table before any spend.
        let inventory = Inventory::of(input.diffs);
        if inventory.source.is_empty() {
            return Ok(LaneOutcome::skipped(
                "No behavioural change: nothing outside documentation, configuration and tests.",
            ));
        }

        let Some(evidence) = input.e2e else {
            return Ok(LaneOutcome::skipped(
                "No end-to-end evidence was gathered for this review.",
            ));
        };

        let changed_paths = input.changed_paths();
        let settings = input.config.lane(LaneId::E2e);

        if evidence.harness.is_empty() {
            let require = settings
                .and_then(|l| l.missing_harness.as_deref())
                .is_some_and(|policy| policy == "require");
            if !require {
                return Ok(LaneOutcome::skipped(
                    "No end-to-end harness in this repository: no e2e test files and no e2e workflow.",
                ));
            }
            // One finding, no model call: the policy asked for a harness and
            // there is none, and there is nothing for a model to add.
            return Ok(LaneOutcome {
                summary: "This repository has no end-to-end harness, and its policy requires one."
                    .into(),
                findings: vec![missing_harness_finding(&inventory.source[0])],
                ..LaneOutcome::default()
            });
        }

        let runs = runs::job_runs(
            &evidence.harness,
            &evidence.checks,
            &changed_paths,
            &input.pull_request.labels,
        );
        let deterministic = runs::findings(&runs);
        let pending = runs::pending(&runs);

        // Assembled above the diff, in the order a reader needs it: what
        // changed, what the harness is, what it did, what might cover the
        // change. Every line of it is decided; the model is told, not asked.
        let rendered = render_diffs(input.diffs);
        let (reviewed_evidence, fresh) =
            crate::evidence::replay::split(input.reviewed_evidence, &rendered);
        let mut assembled = inventory.render();
        assembled.push('\n');
        assembled.push_str(&render_changed_e2e_tests(&evidence.harness, &changed_paths));
        assembled.push_str(&inventory::render(&evidence.harness, &changed_paths));
        assembled.push('\n');
        assembled.push_str(&runs::render(&runs, &input.pull_request.head_sha));
        assembled.push('\n');
        assembled.push_str(&evidence.render_candidates());
        assembled.push('\n');
        assembled.push_str(&fresh);

        let built = prompt::build(&PromptInputs {
            repo_policy: input.repo_policy,
            extracted_rules: input.extracted_rules,
            reviewed_evidence: &reviewed_evidence,
            prior_findings: input.prior_findings,
            new_evidence: &assembled,
            changed_paths: &changed_paths,
            retrieved_context: input.retrieved_context,
            memory_context: input.memory_context,
            ..PromptInputs::new(LaneId::E2e, input.config)
        });

        let reviewers = council::reviewers(input.config, LaneId::E2e);
        let calls: Vec<Call> = reviewers
            .iter()
            .map(|reviewer| Call {
                id: reviewer.id.to_string(),
                model: reviewer.model.to_string(),
                system: built.prefix().to_string(),
                prompt: built.suffix().to_string(),
                schema_name: "tinysweeper_e2e".into(),
            })
            .collect();

        let llm = runner::lane_llm(
            self.model.clone(),
            input.config,
            input.config.models.budget_usd_per_pr,
        );
        let answers = runner::ask_all(
            llm.clone(),
            LaneId::E2e,
            &calls,
            &schema::json_schema(),
            input
                .config
                .council
                .subagents
                .then_some(input.config.models.flash.as_str()),
        )
        .await?;

        let Some(mut outcome) = aggregate_reviewer_responses(
            LaneId::E2e,
            reviewer_responses(LaneId::E2e, &reviewers, &answers)?,
            input.diffs,
            Anchoring::Strict,
            input.config.council.corroboration,
        ) else {
            // The deterministic half stands without a reviewer: a job that
            // will not trigger is a fact whether or not anyone could read the
            // diff. What is lost is the coverage judgement, and the summary
            // says so.
            let mut outcome = LaneOutcome {
                summary: "No reviewer could be consulted; only the job states below are reported."
                    .into(),
                findings: deterministic,
                spend: llm.spend(),
                pending,
                ..LaneOutcome::default()
            };
            append_run_notes(&mut outcome, &runs, evidence);
            return Ok(outcome);
        };

        outcome.spend.merge(llm.spend());
        for finding in &mut outcome.findings {
            if finding.rule == UNOBSERVABLE {
                finding.severity = Severity::Low;
            }
        }
        // A model finding that restates a job state is the double report the
        // partition exists to prevent; the deterministic one is kept.
        outcome.findings.retain(|model| {
            !deterministic
                .iter()
                .any(|fact| fact.path == model.path && fact.rule == model.rule)
        });
        let mut findings = deterministic;
        findings.append(&mut outcome.findings);
        outcome.findings = findings;
        outcome.pending = pending;
        append_run_notes(&mut outcome, &runs, evidence);
        Ok(outcome)
    }
}

/// Say in the summary what is still running and what could not be read.
fn append_run_notes(outcome: &mut LaneOutcome, runs: &[runs::JobRun], evidence: &evidence::Evidence) {
    if !outcome.pending.is_empty() {
        let _ = write!(
            outcome.summary,
            " Waiting on {}: {}.",
            if outcome.pending.len() == 1 {
                "1 end-to-end job"
            } else {
                "end-to-end jobs"
            },
            outcome
                .pending
                .iter()
                .map(|job| format!("`{job}`"))
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    let passed = runs
        .iter()
        .filter(|run| run.state == runs::State::Passed)
        .count();
    if passed > 0 {
        let _ = write!(
            outcome.summary,
            " {passed} end-to-end job{} passed on this head.",
            if passed == 1 { "" } else { "s" }
        );
    }
    if !evidence.degraded.is_empty() {
        let _ = write!(
            outcome.summary,
            " Not everything could be read: {}.",
            evidence.degraded.join("; ")
        );
    }
}

/// The e2e test files this pull request itself changed, for the prompt.
fn render_changed_e2e_tests(harness: &inventory::Harness, changed: &[String]) -> String {
    let touched: Vec<&String> = changed
        .iter()
        .filter(|path| harness.tests.contains(path))
        .collect();
    if touched.is_empty() {
        return "End-to-end tests changed by this pull request: none\n\n".to_string();
    }
    format!(
        "End-to-end tests changed by this pull request: {}\n\n",
        touched
            .iter()
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    )
}

fn missing_harness_finding(path: &str) -> Finding {
    Finding {
        lane: LaneId::E2e,
        severity: Severity::Medium,
        confidence: 1.0,
        path: path.to_string(),
        line: None,
        end_line: None,
        rule: "e2e-missing-harness".into(),
        title: "Add an end-to-end harness this change can be verified with".into(),
        body: "This repository's review policy requires end-to-end verification \
               (`lanes.e2e.missing_harness = \"require\"`), and the tree at this head has \
               no end-to-end test files and no workflow that runs one."
            .into(),
        suggestion: None,
        applicable: None,
        late: false,
        identity: None,
        corroboration: 1,
    }
}

// Not `mod tests`: `lanes::e2e::tests` would read as a lane called tests.
#[cfg(test)]
mod lane_tests {
    use super::*;
    use crate::config::types::Config;
    use crate::evidence::diff::{FileDiff, parse_file_patch};
    use crate::forge::types::{CheckConclusion, CheckStatus, PullRequest};
    use crate::harness::mock::MockModel;
    use crate::lanes::e2e::evidence::{Candidate, Evidence};
    use crate::lanes::e2e::inventory::classify_workflow;
    use serde_json::json;
    use std::collections::BTreeMap;

    fn config() -> Config {
        crate::config::DEFAULTS
            .parse::<toml::Table>()
            .unwrap()
            .try_into()
            .unwrap()
    }

    fn route_diff() -> FileDiff {
        parse_file_patch(
            "src/server/routes.rs",
            "@@ -1,2 +1,3 @@\n fn routes() {\n+    router.post(\"/preview/sessions\", open_session);\n }\n",
        )
    }

    fn pull_request() -> PullRequest {
        PullRequest {
            number: 7,
            title: "feat: open preview sessions".into(),
            head_sha: "abc1234567".into(),
            ..PullRequest::default()
        }
    }

    /// A harness whose only workflow is filtered to `src/server/**`, with one
    /// spec that never mentions the new route.
    fn evidence(filter: &str, checks: Vec<CheckStatus>) -> Evidence {
        let text = format!(
            "name: e2e\non:\n  pull_request:\n    paths:\n      - '{filter}'\njobs:\n  playwright:\n    steps:\n      - run: npx playwright test\n"
        );
        Evidence {
            harness: inventory::Harness {
                tests: vec!["e2e/home.spec.ts".into()],
                workflows: vec![
                    classify_workflow(".github/workflows/e2e.yml", &text, &[]).unwrap(),
                ],
                truncated: false,
            },
            checks,
            candidates: vec![],
            searched: vec!["e2e/home.spec.ts".into()],
            degraded: vec![],
        }
    }

    async fn run_with(
        model: MockModel,
        config: &Config,
        diffs: &[FileDiff],
        evidence: Option<&Evidence>,
    ) -> LaneOutcome {
        let pr = pull_request();
        E2e::new(Arc::new(model))
            .run(LaneInput {
                config,
                pull_request: &pr,
                diffs,
                file_contents: &BTreeMap::new(),
                scan_findings: &[],
                commits: &[],
                repo_policy: None,
                extracted_rules: &[],
                reviewed_evidence: "",
                prior_findings: &[],
                retrieved_context: "",
                memory_context: "",
                e2e: evidence,
            })
            .await
            .expect("lane runs")
    }

    // --- golden test -------------------------------------------------------

    #[tokio::test]
    async fn golden_an_untriggered_workflow_and_an_uncovered_route_are_both_reported() {
        // The workflow's filter excludes the changed path, so the job will not
        // run: a deterministic finding. The model reads the spec list and says
        // nothing drives the new route: a model finding on the changed line.
        // A third, mis-anchored finding is discarded, and a fourth restating
        // the job state is dropped in favour of the fact.
        let model = MockModel::always(json!({
            "summary": "The new route has no end-to-end coverage.",
            "findings": [
                {
                    "path": "src/server/routes.rs", "line": 2,
                    "rule": "e2e-uncovered",
                    "title": "Drive POST /preview/sessions from an e2e test",
                    "body": "e2e/home.spec.ts only loads the home page.",
                    "severity": "medium", "confidence": 0.9
                },
                {
                    "path": "src/server/routes.rs", "line": 1,
                    "rule": "e2e-uncovered",
                    "title": "Something about an unchanged line",
                    "body": "…", "severity": "medium", "confidence": 0.9
                },
                {
                    "path": ".github/workflows/e2e.yml", "line": 4,
                    "rule": "e2e-not-triggered",
                    "title": "The model noticed the filter too",
                    "body": "…", "severity": "low", "confidence": 0.5
                }
            ]
        }));
        let evidence = evidence("src/server/**", vec![]);
        // The change is under `src/preview/`, outside the filter.
        let diff = parse_file_patch(
            "src/preview/apply.rs",
            "@@ -1,2 +1,3 @@\n fn apply() {\n+    publish(\"/preview/sessions\");\n }\n",
        );
        let model_findings_path = "src/preview/apply.rs";
        let model = MockModel::always(json!({
            "summary": "The new publish path has no end-to-end coverage.",
            "findings": [
                {
                    "path": model_findings_path, "line": 2,
                    "rule": "e2e-uncovered",
                    "title": "Drive the preview publish from an e2e test",
                    "body": "e2e/home.spec.ts only loads the home page.",
                    "severity": "medium", "confidence": 0.9
                },
                {
                    "path": model_findings_path, "line": 1,
                    "rule": "e2e-uncovered",
                    "title": "Something about an unchanged line",
                    "body": "…", "severity": "medium", "confidence": 0.9
                },
                {
                    "path": ".github/workflows/e2e.yml", "line": 4,
                    "rule": "e2e-not-triggered",
                    "title": "The model noticed the filter too",
                    "body": "…", "severity": "low", "confidence": 0.5
                }
            ]
        }))
        .or(model);
        let outcome = run_with(model, &config(), &[diff], Some(&evidence)).await;

        let rules: Vec<(&str, Severity)> = outcome
            .findings
            .iter()
            .map(|f| (f.rule.as_str(), f.severity))
            .collect();
        assert_eq!(
            rules,
            [
                ("e2e-not-triggered", Severity::High),
                ("e2e-uncovered", Severity::Medium)
            ],
            "{:#?}",
            outcome.findings
        );
        assert_eq!(outcome.findings[0].path, ".github/workflows/e2e.yml");
        assert_eq!(
            outcome.findings[0].line,
            Some(4),
            "the deterministic finding anchors on the `paths:` line and is not subject to strict anchoring"
        );
        assert!(outcome.findings.iter().all(|f| f.lane == LaneId::E2e));
        assert!(outcome.pending.is_empty(), "a job that will not run is not pending");
        assert!(
            outcome.summary.contains("1 finding discarded"),
            "{}",
            outcome.summary
        );
        assert_eq!(
            outcome.conclusion(Severity::High),
            CheckConclusion::Failure
        );
    }

    #[tokio::test]
    async fn a_job_the_trigger_promises_but_nothing_reports_leaves_the_lane_pending() {
        let model = MockModel::always(json!({ "summary": "Covered.", "findings": [] }));
        let evidence = evidence("src/**", vec![]);
        let outcome = run_with(model, &config(), &[route_diff()], Some(&evidence)).await;

        assert_eq!(outcome.pending, vec!["playwright".to_string()]);
        assert!(outcome.findings.is_empty());
        assert_eq!(
            outcome.conclusion(Severity::High),
            CheckConclusion::Neutral,
            "an unfinished job is never a pass"
        );
        assert!(outcome.summary.contains("Waiting on 1 end-to-end job"), "{}", outcome.summary);
    }

    #[tokio::test]
    async fn a_passed_job_and_a_clean_model_pass_the_lane() {
        let model = MockModel::always(json!({ "summary": "Covered.", "findings": [] }));
        let evidence = evidence(
            "src/**",
            vec![CheckStatus {
                name: "playwright".into(),
                conclusion: Some(CheckConclusion::Success),
            }],
        );
        let outcome = run_with(model, &config(), &[route_diff()], Some(&evidence)).await;

        assert!(outcome.pending.is_empty());
        assert_eq!(
            outcome.conclusion(Severity::High),
            CheckConclusion::Success
        );
        assert!(outcome.summary.contains("1 end-to-end job passed"), "{}", outcome.summary);
    }

    #[tokio::test]
    async fn a_failed_job_is_a_high_finding_the_model_cannot_remove() {
        let model = MockModel::always(json!({ "summary": "All fine.", "findings": [] }));
        let evidence = evidence(
            "src/**",
            vec![CheckStatus {
                name: "playwright".into(),
                conclusion: Some(CheckConclusion::Failure),
            }],
        );
        let outcome = run_with(model, &config(), &[route_diff()], Some(&evidence)).await;

        assert_eq!(outcome.findings.len(), 1);
        assert_eq!(outcome.findings[0].rule, "e2e-failed");
        assert_eq!(
            outcome.conclusion(Severity::High),
            CheckConclusion::Failure
        );
    }

    #[tokio::test]
    async fn unobservable_is_always_informational() {
        let model = MockModel::always(json!({
            "summary": "Internal.",
            "findings": [{
                "path": "src/server/routes.rs", "line": 2,
                "rule": "e2e-unobservable",
                "title": "Needs a third-party callback to observe",
                "body": "…", "severity": "high", "confidence": 0.9
            }]
        }));
        let evidence = evidence(
            "src/**",
            vec![CheckStatus {
                name: "playwright".into(),
                conclusion: Some(CheckConclusion::Success),
            }],
        );
        let outcome = run_with(model, &config(), &[route_diff()], Some(&evidence)).await;
        assert_eq!(outcome.findings[0].severity, Severity::Low);
        assert_eq!(
            outcome.conclusion(Severity::High),
            CheckConclusion::Success
        );
    }

    #[tokio::test]
    async fn a_documentation_only_change_never_calls_the_model() {
        let model = MockModel::new();
        let evidence = evidence("src/**", vec![]);
        let diffs = vec![parse_file_patch("README.md", "@@ -1 +1,2 @@\n a\n+b\n")];
        let outcome = run_with(model.clone(), &config(), &diffs, Some(&evidence)).await;
        assert_eq!(model.calls(), 0);
        assert!(outcome.skipped.is_some());
    }

    #[tokio::test]
    async fn no_harness_skips_by_default_and_asks_for_one_when_required() {
        let model = MockModel::new();
        let none = Evidence::default();
        let outcome = run_with(model.clone(), &config(), &[route_diff()], Some(&none)).await;
        assert_eq!(model.calls(), 0);
        assert!(outcome.skipped.as_deref().unwrap_or("").contains("No end-to-end harness"));

        let mut config = config();
        config
            .lanes
            .get_mut("e2e")
            .expect("defaults declare the lane")
            .missing_harness = Some("require".into());
        let outcome = run_with(model.clone(), &config, &[route_diff()], Some(&none)).await;
        assert_eq!(model.calls(), 0, "nothing for a model to add");
        assert_eq!(outcome.findings.len(), 1);
        assert_eq!(outcome.findings[0].rule, "e2e-missing-harness");
        assert_eq!(outcome.findings[0].severity, Severity::Medium);
        assert!(outcome.skipped.is_none());
    }

    #[tokio::test]
    async fn without_gathered_evidence_the_lane_skips_rather_than_guessing() {
        let model = MockModel::new();
        let outcome = run_with(model.clone(), &config(), &[route_diff()], None).await;
        assert_eq!(model.calls(), 0);
        assert!(outcome.skipped.is_some());
    }

    #[tokio::test]
    async fn the_prompt_carries_the_decided_evidence_not_the_question() {
        let model = MockModel::silent();
        let mut evidence = evidence("src/server/**", vec![]);
        evidence.candidates.push(Candidate {
            path: "e2e/home.spec.ts".into(),
            line: 9,
            text: "await request.post('/preview/sessions')".into(),
            token: "/preview/sessions".into(),
            added_at: "src/server/routes.rs:2".into(),
        });
        run_with(model.clone(), &config(), &[route_diff()], Some(&evidence)).await;

        let prompt = model.last_prompt().expect("recorded");
        assert!(prompt.contains("behaviour: src/server/routes.rs"), "{prompt}");
        assert!(prompt.contains("End-to-end harness in this repository"), "{prompt}");
        assert!(prompt.contains("triggers for this pull request"), "{prompt}");
        assert!(prompt.contains("PENDING"), "{prompt}");
        assert!(prompt.contains("e2e/home.spec.ts:9"), "{prompt}");
        assert!(prompt.contains("End-to-end tests changed by this pull request: none"), "{prompt}");
    }

    #[tokio::test]
    async fn the_lane_never_executes_anything() {
        // The invariant is structural — this lane holds a `Model` and reads
        // evidence — but the assertion is here so that adding a command
        // runner has to consciously delete a test that says not to.
        for file in ["mod.rs", "evidence.rs", "inventory.rs", "runs.rs"] {
            let path = std::path::Path::new(file!())
                .parent()
                .expect("in a directory")
                .join(file);
            let source = std::fs::read_to_string(&path).expect("reads its own source");
            let body = source
                .split("#[cfg(test)]")
                .next()
                .expect("source before the tests");
            for forbidden in ["std::process", "Command::new", "tokio::process"] {
                assert!(
                    !body.contains(forbidden),
                    "the e2e lane must not execute contributor code: found `{forbidden}` in {file}"
                );
            }
        }
    }
}
