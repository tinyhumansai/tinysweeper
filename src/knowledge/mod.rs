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
    let mut knowledge = ReviewKnowledge::default();
    let mut usage = Usage::default();

    if let Some(store) = store {
        let scope = KnowledgeScope::repo(repo.to_string());
        match store.pinned(&scope).await {
            Ok(docs) => {
                knowledge.pinned = pinned::render(
                    &docs,
                    config.knowledge.pinned_doc_chars,
                    config.knowledge.pinned_total_chars,
                );
            }
            Err(err) => tracing::warn!(%err, "could not read pinned knowledge documents"),
        }
    }

    // `respect_agents_md` already means "treat the repository's own instruction
    // files as review policy", so it gates the extraction too rather than
    // introducing a second switch that means almost the same thing.
    if config.knowledge.extract && config.review.respect_agents_md {
        let files = types::scoped_instruction_files(&config.knowledge.files, changed_paths);
        if !files.is_empty() {
            let extractor = Extractor::new(
                model.as_ref(),
                config.model_for_workload(crate::config::types::Workload::KnowledgeExtraction),
                config.knowledge.max_file_bytes,
            );
            let (rules, extract_usage) = extractor.extract(forge, repo, head_sha, &files).await;
            usage.add(extract_usage);
            knowledge.extracted_rules = rules;
        }
    }

    (knowledge, usage)
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
