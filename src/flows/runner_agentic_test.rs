//! Agentic runner evidence and default completion compatibility.

use super::*;
use crate::config::types::Config;
use crate::ports::model::{ModelRequest, ModelResponse};
use crate::ports::tree::MockTree;
use async_trait::async_trait;
use serde_json::json;

struct Exploring;
#[async_trait]
impl Model for Exploring {
    async fn complete(&self, request: ModelRequest) -> Result<ModelResponse> {
        Ok(ModelResponse {
            value: json!({"summary":"completion"}),
            model: request.model,
            usage: Usage::default(),
        })
    }
    async fn review(
        &self,
        request: ModelRequest,
        tree: &dyn TreeReader,
        _: &LookupPolicy,
    ) -> Result<ModelResponse> {
        assert!(
            request
                .schema
                .get("properties")
                .and_then(|p| p.get("lookups"))
                .is_none()
        );
        assert!(
            !request.messages[0]
                .content
                .contains("This is your last turn")
        );
        tree.lookup(&Lookup::Read {
            path: "src/config.rs".into(),
            start: Some(1),
            end: Some(2),
        })
        .await?;
        Ok(ModelResponse {
            value: json!({"summary":"explored"}),
            model: request.model,
            usage: Usage::default(),
        })
    }
}
#[tokio::test]
async fn opted_in_review_preserves_tool_evidence_without_changing_disabled_behavior() {
    let key = format!("AKIA{}", "IOSFODNN7EXAMPLE");
    let tree = MockTree::from_files([(
        "src/config.rs",
        format!("KEY=\"{key}\"\nlet visible = true;"),
    )]);
    let calls = [Call {
        id: "reviewer".into(),
        model: "fixture".into(),
        system: "review".into(),
        prompt: "diff evidence".into(),
        schema_name: "review".into(),
    }];
    let policy = LookupPolicy::default();
    let mut config = Config::default();
    config.models.agentic_reviewers = true;
    let answer = ask_all(
        lane_llm(Arc::new(Exploring), &config, 1.0),
        LaneId::Critique,
        &calls,
        &json!({"type":"object"}),
        Asking {
            tree: Some(&tree),
            lookup: Some(&policy),
            ..Asking::default()
        },
    )
    .await
    .unwrap()
    .remove(0);
    assert_eq!(answer.value.unwrap()["summary"], "explored");
    assert!(answer.looked_up.contains("let visible = true"));
    assert!(!answer.looked_up.contains(&key));
    config.models.agentic_reviewers = false;
    let answer = ask_all(
        lane_llm(Arc::new(Exploring), &config, 1.0),
        LaneId::Critique,
        &calls,
        &json!({"type":"object"}),
        Asking {
            tree: Some(&tree),
            lookup: Some(&policy),
            ..Asking::default()
        },
    )
    .await
    .unwrap()
    .remove(0);
    assert_eq!(answer.value.unwrap()["summary"], "completion");
    assert!(answer.looked_up.is_empty());
}
