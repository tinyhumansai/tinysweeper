//! Description claims can read unchanged guards before settling the verdict.
use super::*;
use crate::config::types::Config;
use crate::evidence::diff::parse_file_patch;
use crate::harness::mock::MockModel;
use crate::ports::tree::MockTree;
use serde_json::json;
use std::collections::BTreeMap;

#[tokio::test]
async fn unchanged_aggregate_guard_reaches_the_description_before_its_verdict() {
    // The changed call alone cannot show whether the enclosing emitter bounds
    // a merged group. This is the unsupported claim observed on live PR206.
    let model = MockModel::new()
        .then(json!({
            "summary":"Need to check the unchanged aggregate guard.",
            "findings":[{"path":"src/chunk/tree.rs","rule":"imperative-bounds",
                "title":"Bound aggregate parsed chunks before embedding",
                "body":"Several smaller nodes can merge above the provider bound.",
                "severity":"high","confidence":0.84}],
            "lookups":[{"kind":"read","path":"src/chunk/tree.rs",
                "start":337,"end":357,"why":"Does emit bound the complete merged span?"}]
        }))
        .then(json!({"summary":"The complete aggregate is already bounded.","findings":[]}));
    let tree = MockTree::from_files([("src/chunk/tree.rs", include_str!("../chunk/tree.rs"))]);
    let mut config: Config = crate::config::DEFAULTS
        .parse::<toml::Table>()
        .unwrap()
        .try_into()
        .unwrap();
    config.models.agentic_reviewers = false;
    config.lookup.enabled = true;
    config.lookup.rounds = 1;
    let pr = PullRequest {
        number: 206,
        title: "Bound embedding inputs conservatively".into(),
        body: "Split oversized embedding source and retain every source byte.".into(),
        ..PullRequest::default()
    };
    let diffs = [parse_file_patch(
        "src/chunk/tree.rs",
        "@@ -352,1 +352,1 @@\n-    chunks.extend(lines::split(text, start_line, options));\n+    chunks.extend(lines::split_preserving_whitespace(text, start_line, options));\n",
    )];
    let outcome = Description::new(Arc::new(model.clone()))
        .run(LaneInput {
            config: &config,
            pull_request: &pr,
            diffs: &diffs,
            file_contents: &BTreeMap::new(),
            scan_findings: &[],
            commits: &[],
            repo_policy: None,
            extracted_rules: &[],
            reviewed_evidence: "",
            prior_findings: &[],
            retrieved_context: "",
            memory_context: "",
            redaction_note: "",
            e2e: None,
            tree: Some(&tree),
            graph: None,
        })
        .await
        .unwrap();
    assert!(
        outcome.findings.is_empty(),
        "the provisional unsupported finding must be replaced after reading the guard"
    );
    let requests = model.requests();
    assert_eq!(requests.len(), 2, "one lookup turn and one settled verdict");
    let evidence = &requests[1].messages[1].content;
    assert!(evidence.contains("## What you looked up"), "{evidence}");
    assert!(evidence.contains("(first.start, last.end)"), "{evidence}");
    assert!(
        evidence.contains("text.len() > options.split_ceiling()"),
        "{evidence}"
    );
    assert_eq!(
        outcome.summary,
        "The complete aggregate is already bounded."
    );
}

struct GuardReviewer;
#[async_trait]
impl Model for GuardReviewer {
    async fn complete(
        &self,
        request: crate::ports::model::ModelRequest,
    ) -> Result<crate::ports::model::ModelResponse> {
        Ok(crate::ports::model::ModelResponse {
            value: json!({"summary":"No unchanged implementation was available.","findings":[]}),
            model: request.model,
            usage: Default::default(),
        })
    }
    async fn review(
        &self,
        request: crate::ports::model::ModelRequest,
        tree: &dyn crate::ports::tree::TreeReader,
        policy: &crate::config::types::LookupPolicy,
    ) -> Result<crate::ports::model::ModelResponse> {
        assert!(policy.enabled);
        let found = tree
            .lookup(&crate::ports::tree::Lookup::Read {
                path: "src/chunk/tree.rs".into(),
                start: Some(337),
                end: Some(357),
            })
            .await?;
        let crate::ports::tree::Found::Text { text, .. } = found else {
            panic!("guard is available")
        };
        assert!(text.contains("text.len() > options.split_ceiling()"));
        Ok(crate::ports::model::ModelResponse {
            value: json!({"summary":"Oversized aggregates are split on lines.",
                "findings":[{"path":"src/chunk/tree.rs","line":354,"rule":"description-mismatch",
                    "title":"Describe the exception for oversized definitions",
                    "body":"The body promises every definition stays whole; the aggregate guard splits oversized spans.",
                    "severity":"medium","confidence":0.9}]}),
            model: request.model,
            usage: Default::default(),
        })
    }
}

#[tokio::test]
async fn agentic_description_reads_unchanged_guard_and_keeps_true_mismatch_summary_only() {
    let tree = MockTree::from_files([("src/chunk/tree.rs", include_str!("../chunk/tree.rs"))]);
    let mut config: Config = crate::config::DEFAULTS
        .parse::<toml::Table>()
        .unwrap()
        .try_into()
        .unwrap();
    config.models.agentic_reviewers = true;
    config.lookup.enabled = true;
    let pr = PullRequest {
        title: "Bound embedding source".into(),
        body: "Every definition is retained whole without any splits.".into(),
        ..PullRequest::default()
    };
    let diffs = [parse_file_patch(
        "src/chunk/tree.rs",
        "@@ -354,1 +354,1 @@\n-    chunks.extend(old_split(text));\n+    chunks.extend(new_split(text));\n",
    )];
    let outcome = Description::new(Arc::new(GuardReviewer))
        .run(LaneInput {
            config: &config,
            pull_request: &pr,
            diffs: &diffs,
            file_contents: &BTreeMap::new(),
            scan_findings: &[],
            commits: &[],
            repo_policy: None,
            extracted_rules: &[],
            reviewed_evidence: "",
            prior_findings: &[],
            retrieved_context: "",
            memory_context: "",
            redaction_note: "",
            e2e: None,
            tree: Some(&tree),
            graph: None,
        })
        .await
        .unwrap();
    assert_eq!(
        outcome.findings.len(),
        1,
        "the actual mismatch must survive after reading implementation evidence"
    );
    let finding = &outcome.findings[0];
    assert_eq!(finding.rule, "description-mismatch");
    assert_eq!(finding.path, DESCRIPTION_SUBJECT);
    assert_eq!(finding.line, None);
    assert_eq!(finding.end_line, None);
}
