//! The knowledge centre: what a reviewer knows before it reads the diff.
//!
//! Always compiled. Two sources, and they are treated very differently because
//! they *are* very different:
//!
//! - **Curated documents**, written by operators through the admin API and
//!   stored behind the [`KnowledgeStore`](crate::ports::knowledge::KnowledgeStore)
//!   port, scoped to an organisation or a repository. Pinned ones go into every
//!   prompt under a per-document and a total cap ([`pinned`]); the rest are left
//!   to retrieval.
//! - **Repository instruction files** — `AGENTS.md` and friends — which are
//!   written by whoever opened the pull request and are therefore hostile input.
//!   They go through the sandboxed extraction pass in [`extract`] and land in
//!   the volatile half of the prompt, fenced and labelled.
//!
//! This module replaced a function that read `AGENTS.md` from the *process
//! working directory* — that is, from tinysweeper's own checkout rather than
//! the repository under review — and injected the first 6000 characters of it
//! straight into the cacheable system prefix. Both halves of that were wrong:
//! the wrong repository's conventions, and attacker-controlled text in the one
//! position the model is told to obey.

pub mod cache;
pub mod extract;
pub mod pinned;
pub mod types;

use std::sync::Arc;

use crate::config::types::Config;
use crate::forge::types::RepoId;
use crate::index::types::KnowledgeScope;
use crate::ports::forge::ForgeRead;
use crate::ports::knowledge::KnowledgeStore;
use crate::ports::model::{Model, Usage};

pub use crate::knowledge::cache::RuleCache;
pub use crate::knowledge::extract::Extractor;
pub use crate::knowledge::types::{
    MAX_RULE_CHARS, MAX_RULES, ReviewKnowledge, doc_id, valid_instruction_file, valid_slug,
};

/// Pinned knowledge and instruction-file paths prepared before extraction.
///
/// This stage performs no model calls. Large-review admission can therefore
/// happen after the host has prepared review metadata and before any
/// repository-authored instructions are sent to a model.
#[derive(Debug, Clone, Default)]
pub struct PreparedKnowledge {
    /// Operator-pinned documents rendered under the configured caps.
    pub knowledge: ReviewKnowledge,
    /// Safe, scoped repository instruction paths eligible for extraction.
    pub instruction_files: Vec<String>,
}

/// Read pinned documents and prepare the bounded instruction-file list.
///
/// This is the no-model-call half of [`gather`]. Repository instruction
/// contents remain unread until [`extract_prepared`] runs after dispatch
/// admission.
pub async fn prepare(
    config: &Config,
    repo: &RepoId,
    changed_paths: &[String],
    store: Option<&dyn KnowledgeStore>,
) -> PreparedKnowledge {
    let mut prepared = PreparedKnowledge::default();

    if let Some(store) = store {
        let scope = KnowledgeScope::repo(repo.to_string());
        match store.pinned(&scope).await {
            Ok(docs) => {
                prepared.knowledge.pinned = pinned::render(
                    &docs,
                    config.knowledge.pinned_doc_chars,
                    config.knowledge.pinned_total_chars,
                );
            }
            Err(err) => tracing::warn!(%err, "could not read pinned knowledge documents"),
        }
    }

    if config.knowledge.extract && config.review.respect_agents_md {
        prepared.instruction_files =
            types::scoped_instruction_files(&config.knowledge.files, changed_paths);
    }

    prepared
}

/// Extract the prepared repository instructions through the paid model port.
pub async fn extract_prepared(
    mut prepared: PreparedKnowledge,
    forge: &dyn ForgeRead,
    model: &Arc<dyn Model>,
    config: &Config,
    repo: &RepoId,
    head_sha: &str,
) -> (ReviewKnowledge, Usage) {
    let mut usage = Usage::default();
    if !prepared.instruction_files.is_empty() {
        let extractor = Extractor::new(
            model.as_ref(),
            config.model_for_workload(crate::config::types::Workload::KnowledgeExtraction),
            config.knowledge.max_file_bytes,
        );
        let (rules, extract_usage) = extractor
            .extract(forge, repo, head_sha, &prepared.instruction_files)
            .await;
        usage.add(extract_usage);
        prepared.knowledge.extracted_rules = rules;
    }

    (prepared.knowledge, usage)
}

/// Gather everything the knowledge centre contributes to one review.
///
/// Best-effort throughout: a knowledge store that is unreachable, a file that
/// does not exist and an extraction that fails validation all contribute
/// nothing, and none of them fail the review. Curated context makes a review
/// better; losing the review because the context was unavailable would make it
/// worse, and would hand any contributor a way to break the bot by editing one
/// file.
pub async fn gather(
    forge: &dyn ForgeRead,
    model: &Arc<dyn Model>,
    config: &Config,
    repo: &RepoId,
    head_sha: &str,
    changed_paths: &[String],
    store: Option<&dyn KnowledgeStore>,
) -> (ReviewKnowledge, Usage) {
    let prepared = prepare(config, repo, changed_paths, store).await;
    extract_prepared(prepared, forge, model, config, repo, head_sha).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::forge::{MockForge, MockState};
    use crate::harness::mock::MockModel;
    use crate::index::MockKnowledgeStore;
    use crate::index::types::KnowledgeDoc;
    use serde_json::json;

    fn config() -> Config {
        crate::config::DEFAULTS
            .parse::<toml::Table>()
            .unwrap()
            .try_into()
            .unwrap()
    }

    fn repo() -> RepoId {
        RepoId::parse("acme/app").unwrap()
    }

    fn forge_with_agents(content: &str) -> MockForge {
        let mut state = MockState::default();
        state.set_file("headsha", "AGENTS.md", content);
        MockForge::with_state(state)
    }

    #[tokio::test]
    async fn extraction_and_pinned_documents_arrive_separately() {
        let forge = forge_with_agents("Never unwrap in library code.");
        let model: Arc<dyn Model> = Arc::new(MockModel::new().then(json!({
            "rules_markdown": "- Never unwrap in library code."
        })));
        let store = MockKnowledgeStore::new();
        store
            .put(
                &KnowledgeDoc::new(
                    "org:acme:style",
                    KnowledgeScope::org("acme"),
                    "Style",
                    "Tabs.",
                )
                .pinned(),
            )
            .await
            .expect("stores");

        let (knowledge, _usage) = gather(
            &forge,
            &model,
            &config(),
            &repo(),
            "headsha",
            &[],
            Some(&store as &dyn KnowledgeStore),
        )
        .await;

        assert!(knowledge.pinned.contains("Tabs."));
        assert_eq!(
            knowledge.extracted_rules,
            vec!["Never unwrap in library code.".to_string()]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn pinned_knowledge_survives_instruction_extraction_refused_at_soft_deadline() {
        let forge = forge_with_agents(
            "pinned_knowledge_survives_instruction_extraction_refused_at_soft_deadline: retain local context.",
        );
        let mock = MockModel::silent();
        let model: Arc<dyn Model> = Arc::new(mock.clone());
        let mut config = config();
        config.knowledge.extract = true;
        config.review.respect_agents_md = true;
        config.knowledge.files = vec!["AGENTS.md".into()];
        let store = MockKnowledgeStore::new();
        store
            .put(
                &KnowledgeDoc::new(
                    "pinned-before-extraction",
                    KnowledgeScope::repo(repo().to_string()),
                    "Curated guidance",
                    "Preserve the curated convention when extraction cannot run.",
                )
                .pinned(),
            )
            .await
            .expect("seed pinned context");

        let started_at = tokio::time::Instant::now();
        let execution = crate::app::review_execution::ReviewRunContext::new(
            crate::app::review_execution::ReviewExecutionOptions {
                started_at,
                hard_deadline: started_at + std::time::Duration::from_secs(100),
                mode: crate::lanes::large_pr::ReviewMode::Large,
                max_calls: Some(8),
                soft_deadline_percent: 80,
                drain_seconds: 5,
            },
        );
        let bound = execution.bind_model(Arc::clone(&model), config.models.budget_usd_per_pr);

        let prepared = prepare(
            &config,
            &repo(),
            &["src/lib.rs".into()],
            Some(&store as &dyn KnowledgeStore),
        )
        .await;
        assert!(
            prepared
                .knowledge
                .pinned
                .contains("Preserve the curated convention when extraction cannot run.")
        );
        assert!(
            prepared
                .instruction_files
                .contains(&"AGENTS.md".to_string())
        );

        tokio::time::advance(std::time::Duration::from_secs(80)).await;
        let (knowledge, usage) =
            extract_prepared(prepared, &forge, &bound.model, &config, &repo(), "headsha").await;

        assert!(
            knowledge
                .pinned
                .contains("Preserve the curated convention when extraction cannot run.")
        );
        assert!(knowledge.extracted_rules.is_empty());
        assert_eq!(usage.input_tokens, 0);
        assert_eq!(usage.output_tokens, 0);
        assert_eq!(usage.cost_usd, 0.0);
        assert_eq!(mock.calls(), 0, "refused extraction consumes no response");
    }

    #[tokio::test]
    async fn a_repository_whose_only_policy_file_is_under_dot_github_is_still_read() {
        // The acceptance criterion of issue #37, end to end: nothing at the
        // root, policy only at the conventional `.github/` location.
        let mut state = MockState::default();
        state.set_file(
            "headsha",
            ".github/AGENTS.md",
            "Never unwrap in library code.",
        );
        let forge = MockForge::with_state(state);
        let model: Arc<dyn Model> = Arc::new(MockModel::new().then(json!({
            "rules_markdown": "- Never unwrap in library code."
        })));

        let (knowledge, _usage) =
            gather(&forge, &model, &config(), &repo(), "headsha", &[], None).await;

        assert_eq!(
            knowledge.extracted_rules,
            vec!["Never unwrap in library code.".to_string()]
        );
    }

    #[tokio::test]
    async fn a_store_failure_costs_context_not_the_review() {
        let forge = forge_with_agents("Never unwrap.");
        let model: Arc<dyn Model> =
            Arc::new(MockModel::new().then(json!({"rules_markdown": "NO_RULES"})));

        let (knowledge, _usage) =
            gather(&forge, &model, &config(), &repo(), "headsha", &[], None).await;

        assert!(knowledge.is_empty());
    }

    #[tokio::test]
    async fn opting_out_of_agents_md_skips_extraction_entirely() {
        let forge = forge_with_agents("Never unwrap.");
        // No queued response: a model call here would be an error, which is the
        // assertion — the opt-out must be checked before any spend.
        let model: Arc<dyn Model> = Arc::new(MockModel::new());
        let mut config = config();
        config.review.respect_agents_md = false;

        let (knowledge, usage) =
            gather(&forge, &model, &config, &repo(), "headsha", &[], None).await;

        assert!(knowledge.extracted_rules.is_empty());
        assert_eq!(usage.input_tokens, 0);
    }
}
