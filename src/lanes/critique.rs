//! The `critique` lane: correctness of the diff.
//!
//! The first lane, and the template for the rest. Its shape is the point:
//!
//! 1. Decide whether there is anything to review at all, before spending a
//!    token.
//! 2. Build the prompt in cache-friendly layers.
//! 3. Ask for structured output; refuse to parse prose.
//! 4. Drop anything the model anchored outside the diff — *before* it can
//!    become a comment.
//!
//! Step 4 is not a nicety. Models reliably produce findings anchored to lines
//! they inferred rather than read, and a comment on an unrelated line is how a
//! review bot loses a team's trust in one shot.

use std::sync::Arc;

use async_trait::async_trait;

use crate::config::types::LaneId;
use crate::error::Result;
use crate::evidence::diff::FileDiff;
use crate::findings::types::Finding;
use crate::harness::prompt::{self, PromptInputs};
use crate::harness::schema;
use crate::lanes::{Lane, LaneInput, LaneOutcome};
use crate::ports::model::{Message, Model, ModelRequest};

/// The correctness lane.
pub struct Critique {
    model: Arc<dyn Model>,
}

impl Critique {
    /// Build the lane over `model`.
    pub fn new(model: Arc<dyn Model>) -> Self {
        Self { model }
    }
}

#[async_trait]
impl Lane for Critique {
    fn id(&self) -> LaneId {
        LaneId::Critique
    }

    async fn run(&self, input: LaneInput<'_>) -> Result<LaneOutcome> {
        // Cheapest possible check first. A pull request that only deletes
        // files, or only touches ignored paths, has nothing for this lane, and
        // asking a model to confirm that is money for a foregone conclusion.
        if !input.has_reviewable_content() {
            return Ok(LaneOutcome::skipped(
                "No added or modified lines to review.",
            ));
        }

        if input.pull_request.draft && !input.config.review.draft_prs {
            return Ok(LaneOutcome::skipped(
                "Draft pull request; set `review.draft_prs = true` to review drafts.",
            ));
        }

        let new_evidence = render_diffs(input.diffs);
        let changed_paths = input.changed_paths();
        let built = prompt::build(&PromptInputs {
            repo_policy: input.repo_policy,
            reviewed_evidence: input.reviewed_evidence,
            prior_findings: input.prior_findings,
            new_evidence: &new_evidence,
            changed_paths: &changed_paths,
            ..PromptInputs::new(LaneId::Critique, input.config)
        });

        // The prefix goes in the system message and the suffix in the user
        // message. Providers cache on the serialised prefix, and a system
        // message is the one part guaranteed to be sent first.
        let response = self
            .model
            .complete(ModelRequest {
                model: input.config.model_for(LaneId::Critique).to_string(),
                messages: vec![
                    Message::system(built.prefix()),
                    Message::user(built.suffix()),
                ],
                schema: schema::json_schema(),
                schema_name: "tinysweeper_critique".into(),
                max_tokens: input.config.models.max_tokens,
            })
            .await?;

        let parsed = schema::parse(LaneId::Critique, response.value)?;

        let mut findings = Vec::new();
        let mut discarded = 0usize;
        for raw in parsed.findings {
            let finding = raw.into_finding(LaneId::Critique);
            if anchored_in_diff(&finding, input.diffs) {
                findings.push(finding);
            } else {
                // Not an error: models do this routinely. It is dropped
                // quietly and counted, so the count can surface in the summary
                // if it ever gets large enough to mean something.
                discarded += 1;
            }
        }

        let summary = if discarded > 0 {
            format!(
                "{} ({discarded} finding{} discarded for not matching a changed line)",
                parsed.summary.trim(),
                if discarded == 1 { "" } else { "s" }
            )
        } else {
            parsed.summary.trim().to_string()
        };

        Ok(LaneOutcome {
            summary,
            findings,
            resolved: parsed.resolved,
            usage: response.usage,
            skipped: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::types::{Config, Severity};
    use crate::evidence::diff::parse_file_patch;
    use crate::forge::types::PullRequest;
    use crate::harness::mock::MockModel;
    use serde_json::json;

    fn config() -> Config {
        crate::config::DEFAULTS
            .parse::<toml::Table>()
            .unwrap()
            .try_into()
            .unwrap()
    }

    const PATCH: &str =
        "@@ -1,3 +1,5 @@\n fn main() {\n+    let x = items[i];\n+    println!(\"{x}\");\n }\n";

    fn diffs() -> Vec<FileDiff> {
        vec![parse_file_patch("src/main.rs", PATCH)]
    }

    fn pull_request() -> PullRequest {
        PullRequest {
            number: 7,
            title: "feat: index items".into(),
            head_sha: "abc123".into(),
            ..PullRequest::default()
        }
    }

    async fn run_with(model: MockModel, config: &Config, diffs: &[FileDiff]) -> LaneOutcome {
        let pr = pull_request();
        Critique::new(Arc::new(model))
            .run(LaneInput {
                config,
                pull_request: &pr,
                diffs,
                scan_findings: &[],
                repo_policy: None,
                reviewed_evidence: "",
                prior_findings: &[],
            })
            .await
            .expect("lane runs")
    }

    fn finding_at(line: u64) -> serde_json::Value {
        json!({
            "path": "src/main.rs",
            "line": line,
            "rule": "unchecked-index",
            "title": "Guard the index before dereferencing",
            "body": "`i` is never bounds-checked.",
            "severity": "high",
            "confidence": 0.9
        })
    }

    #[tokio::test]
    async fn a_finding_on_a_changed_line_survives() {
        let model = MockModel::new().then(json!({
            "summary": "Adds an unchecked index.",
            "findings": [finding_at(2)]
        }));
        let outcome = run_with(model, &config(), &diffs()).await;

        assert_eq!(outcome.findings.len(), 1);
        assert_eq!(outcome.findings[0].severity, Severity::High);
        assert_eq!(outcome.findings[0].lane, LaneId::Critique);
    }

    #[tokio::test]
    async fn a_finding_on_an_unchanged_line_is_discarded() {
        // Line 1 is context, not an addition. Posting there would put a comment
        // on code the author did not write in this pull request.
        let model = MockModel::new().then(json!({
            "summary": "…",
            "findings": [finding_at(1)]
        }));
        let outcome = run_with(model, &config(), &diffs()).await;

        assert!(outcome.findings.is_empty(), "{:#?}", outcome.findings);
        assert!(
            outcome.summary.contains("1 finding discarded"),
            "{}",
            outcome.summary
        );
    }

    #[tokio::test]
    async fn a_finding_in_a_file_the_pull_request_never_touched_is_discarded() {
        let model = MockModel::new().then(json!({
            "summary": "…",
            "findings": [{
                "path": "src/elsewhere.rs",
                "line": 2,
                "rule": "r", "title": "t", "body": "b",
                "severity": "high", "confidence": 0.9
            }]
        }));
        let outcome = run_with(model, &config(), &diffs()).await;
        assert!(outcome.findings.is_empty());
    }

    #[tokio::test]
    async fn a_late_finding_may_sit_on_unchanged_lines_of_a_touched_file() {
        let mut late = finding_at(1);
        late["late"] = json!(true);
        let model = MockModel::new().then(json!({"summary": "…", "findings": [late]}));

        let outcome = run_with(model, &config(), &diffs()).await;
        assert_eq!(outcome.findings.len(), 1);
        assert!(outcome.findings[0].late);
    }

    #[tokio::test]
    async fn an_empty_review_is_reported_as_such() {
        let outcome = run_with(MockModel::silent(), &config(), &diffs()).await;

        assert!(outcome.findings.is_empty());
        assert_eq!(outcome.summary, "Nothing to report.");
        assert!(
            outcome.skipped.is_none(),
            "silence is not the same as skipping"
        );
    }

    #[tokio::test]
    async fn a_pull_request_with_nothing_to_review_never_calls_the_model() {
        let model = MockModel::new();
        let outcome = run_with(model.clone(), &config(), &[]).await;

        assert_eq!(model.calls(), 0, "spent money on a foregone conclusion");
        assert!(outcome.skipped.is_some());
    }

    #[tokio::test]
    async fn a_draft_is_skipped_unless_the_repository_opts_in() {
        let config = config();
        let model = MockModel::silent();
        let pr = PullRequest {
            draft: true,
            ..pull_request()
        };
        let diffs = diffs();

        let outcome = Critique::new(Arc::new(model.clone()))
            .run(LaneInput {
                config: &config,
                pull_request: &pr,
                diffs: &diffs,
                scan_findings: &[],
                repo_policy: None,
                reviewed_evidence: "",
                prior_findings: &[],
            })
            .await
            .expect("runs");

        assert!(outcome.skipped.is_some());
        assert_eq!(model.calls(), 0);
    }

    #[tokio::test]
    async fn the_prompt_carries_line_numbers_so_anchors_are_read_not_counted() {
        let model = MockModel::silent();
        run_with(model.clone(), &config(), &diffs()).await;

        let prompt = model.last_prompt().expect("recorded");
        assert!(prompt.contains("2 +    let x = items[i];"), "{prompt}");
    }

    #[tokio::test]
    async fn the_configured_tier_is_the_model_actually_called() {
        let mut config = config();
        config.models.deep = "some/deep-model".into();
        let model = MockModel::silent();
        run_with(model.clone(), &config, &diffs()).await;

        assert_eq!(model.requests()[0].model, "some/deep-model");
    }

    #[tokio::test]
    async fn a_re_review_replays_the_earlier_diff_into_the_cacheable_system_message() {
        let config = config();
        let model = MockModel::silent();
        let pr = pull_request();
        let diffs = diffs();

        Critique::new(Arc::new(model.clone()))
            .run(LaneInput {
                config: &config,
                pull_request: &pr,
                diffs: &diffs,
                scan_findings: &[],
                repo_policy: None,
                reviewed_evidence: "@@ -1 +1 @@\n+earlier\n",
                prior_findings: &["Close the socket on the error path".to_string()],
            })
            .await
            .expect("runs");

        let request = &model.requests()[0];
        let system = &request.messages[0].content;
        let user = &request.messages[1].content;

        assert!(
            system.contains("+earlier"),
            "replay must be in the cached half"
        );
        assert!(!user.contains("+earlier"));
        assert!(
            user.contains("Close the socket"),
            "prior findings are volatile"
        );
        assert!(
            !system.contains("Close the socket"),
            "prior findings must not enter the cached prefix"
        );
    }

    #[tokio::test]
    async fn malformed_model_output_fails_the_lane_rather_than_posting_nonsense() {
        let model = MockModel::new().then(json!({"summary": "…", "findings": [{"path": "x"}]}));
        let pr = pull_request();
        let config = config();
        let diffs = diffs();

        let err = Critique::new(Arc::new(model))
            .run(LaneInput {
                config: &config,
                pull_request: &pr,
                diffs: &diffs,
                scan_findings: &[],
                repo_policy: None,
                reviewed_evidence: "",
                prior_findings: &[],
            })
            .await
            .unwrap_err();

        assert!(
            err.to_string().contains("did not match the schema"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn resolved_findings_are_carried_through() {
        let model = MockModel::new().then(json!({
            "summary": "Earlier issue is fixed.",
            "findings": [],
            "resolved": ["Guard the index before dereferencing"]
        }));
        let outcome = run_with(model, &config(), &diffs()).await;

        assert_eq!(
            outcome.resolved,
            vec!["Guard the index before dereferencing"]
        );
    }
}
