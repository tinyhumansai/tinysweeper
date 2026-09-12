//! The always-compiled offline memory.
//!
//! Every port has one of these, and this is the one for
//! [`Memory`](crate::ports::memory::Memory). It is not a stub: recall is a real
//! keyword ranking over what was remembered, so a test can assert that the
//! convention about the changed path is the one that reaches the prompt and
//! the one about an unrelated path does not. `local-review` uses it too, so a
//! run with no engine behind it still exercises the whole recall path.
//!
//! Answers are the one thing a keyword store cannot produce, so they are
//! canned: a test registers the answer it wants for a question, and a question
//! with no registered answer comes back ungrounded — which is exactly what a
//! real engine returns for a question it has nothing on.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use crate::error::{Error, Result};
use crate::memory::types::{
    Citation, MemoryAnswer, MemoryItem, MemoryScope, Recollection, RememberReport,
};
use crate::ports::memory::Memory;

/// An in-process memory that records every write.
#[derive(Debug, Clone, Default)]
pub struct MockMemory {
    /// Items by scope, then by content id — which is what makes replays
    /// visible: an item offered twice occupies one slot.
    items: Arc<Mutex<BTreeMap<MemoryScope, BTreeMap<String, MemoryItem>>>>,
    /// Canned answers, matched by substring of the question.
    answers: Arc<Mutex<Vec<(String, MemoryAnswer)>>>,
    /// When set, every call fails with this message. For the tests that prove
    /// an unreachable engine costs context and never the review.
    failure: Arc<Mutex<Option<String>>>,
}

impl MockMemory {
    /// An empty memory.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a canned answer for any question containing `needle`.
    pub fn with_answer(self, needle: impl Into<String>, answer: impl Into<String>) -> Self {
        let needle = needle.into();
        let answer = MemoryAnswer {
            question: needle.clone(),
            answer: answer.into(),
            citations: vec![Citation {
                id: "canned".into(),
                path: None,
                excerpt: None,
            }],
            model: Some("mock".into()),
        };
        self.answers
            .lock()
            .expect("answers lock")
            .push((needle, answer));
        self
    }

    /// Make every call fail from now on.
    pub fn fail_with(&self, message: impl Into<String>) {
        *self.failure.lock().expect("failure lock") = Some(message.into());
    }

    /// Everything remembered in `scope`, in key order.
    pub fn remembered(&self, scope: &MemoryScope) -> Vec<MemoryItem> {
        let items = self.items.lock().expect("items lock");
        let mut out: Vec<MemoryItem> = items
            .iter()
            .filter(|(stored, _)| scope.covers(stored))
            .flat_map(|(_, by_id)| by_id.values().cloned())
            .collect();
        out.sort_by(|a, b| a.key.cmp(&b.key));
        out
    }

    /// How many items are held, over every scope.
    pub fn len(&self) -> usize {
        self.items
            .lock()
            .expect("items lock")
            .values()
            .map(BTreeMap::len)
            .sum()
    }

    /// Whether nothing has been remembered.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn check(&self) -> Result<()> {
        match self.failure.lock().expect("failure lock").as_deref() {
            Some(message) => Err(Error::Model(format!("mock memory: {message}"))),
            None => Ok(()),
        }
    }
}

/// Lowercased alphanumeric terms of `text`, deduplicated.
pub(crate) fn terms(text: &str) -> Vec<String> {
    let mut out: Vec<String> = text
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .filter(|t| t.len() >= 2)
        .map(str::to_lowercase)
        .collect();
    out.sort();
    out.dedup();
    out
}

/// How well `item` matches `query`: the share of query terms it contains,
/// with a bonus for a path or title hit. Title and path hits are what a
/// convention keyed to a directory needs — the body may never name the path.
fn score(item: &MemoryItem, query_terms: &[String]) -> f64 {
    if query_terms.is_empty() {
        return 0.0;
    }
    let haystack = format!(
        "{} {} {} {}",
        item.title,
        item.body,
        item.path.as_deref().unwrap_or(""),
        item.symbol.as_deref().unwrap_or("")
    )
    .to_lowercase();
    let heading = format!(
        "{} {} {}",
        item.title,
        item.path.as_deref().unwrap_or(""),
        item.symbol.as_deref().unwrap_or("")
    )
    .to_lowercase();
    let mut hits = 0.0;
    for term in query_terms {
        if heading.contains(term.as_str()) {
            hits += 2.0;
        } else if haystack.contains(term.as_str()) {
            hits += 1.0;
        }
    }
    hits / query_terms.len() as f64
}

#[async_trait]
impl Memory for MockMemory {
    fn name(&self) -> &str {
        "mock"
    }

    async fn health(&self) -> Result<()> {
        self.check()
    }

    async fn remember(&self, scope: &MemoryScope, items: &[MemoryItem]) -> Result<RememberReport> {
        self.check()?;
        let mut report = RememberReport::default();
        let mut store = self.items.lock().expect("items lock");
        for item in items {
            let filed = MemoryScope::section(scope.repo.clone(), item.section());
            if !scope.covers(&filed) {
                return Err(Error::Model(format!(
                    "memory item `{}` is a {} and cannot be filed under {scope}",
                    item.key,
                    item.kind.as_str()
                )));
            }
            let slot = store.entry(filed).or_default();
            let id = item.content_id();
            if slot.contains_key(&id) {
                report.replayed += 1;
            } else {
                slot.insert(id, item.clone());
                report.written += 1;
            }
        }
        Ok(report)
    }

    async fn recall(
        &self,
        scope: &MemoryScope,
        query: &str,
        limit: usize,
    ) -> Result<Vec<Recollection>> {
        self.check()?;
        let query_terms = terms(query);
        let mut scored: Vec<Recollection> = self
            .remembered(scope)
            .into_iter()
            .map(|item| {
                let score = score(&item, &query_terms);
                Recollection {
                    item,
                    score: Some(score),
                }
            })
            .filter(|r| r.score.is_some_and(|s| s > 0.0))
            .collect();
        // Best first; ties in key order so the ranking is deterministic.
        scored.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.item.key.cmp(&b.item.key))
        });
        scored.truncate(limit);
        Ok(scored)
    }

    async fn answer(
        &self,
        _scope: &MemoryScope,
        question: &str,
        _instructions: Option<&str>,
    ) -> Result<MemoryAnswer> {
        self.check()?;
        let canned = self
            .answers
            .lock()
            .expect("answers lock")
            .iter()
            .find(|(needle, _)| question.contains(needle.as_str()))
            .map(|(_, answer)| answer.clone());
        Ok(match canned {
            Some(mut answer) => {
                answer.question = question.to_string();
                answer
            }
            None => MemoryAnswer {
                question: question.to_string(),
                answer: String::new(),
                citations: Vec::new(),
                model: None,
            },
        })
    }

    async fn forget(&self, scope: &MemoryScope) -> Result<u64> {
        self.check()?;
        let mut store = self.items.lock().expect("items lock");
        let doomed: Vec<MemoryScope> = store
            .keys()
            .filter(|stored| scope.covers(stored))
            .cloned()
            .collect();
        let mut gone = 0u64;
        for key in doomed {
            gone += store.remove(&key).map(|m| m.len() as u64).unwrap_or(0);
        }
        Ok(gone)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::types::{MemoryKind, MemorySection};

    fn convention(key: &str, path: &str, body: &str) -> MemoryItem {
        MemoryItem::new(key, MemoryKind::Convention, key, body).at_path(path)
    }

    #[tokio::test]
    async fn remembering_the_same_item_twice_writes_once() {
        let memory = MockMemory::new();
        let scope = MemoryScope::repo("o/r");
        let item = convention("errors", "CLAUDE.md", "Return Result, never panic.");
        let first = memory.remember(&scope, &[item.clone()]).await.unwrap();
        let second = memory.remember(&scope, &[item]).await.unwrap();
        assert_eq!(first.written, 1);
        assert_eq!(second.replayed, 1);
        assert_eq!(memory.len(), 1);
    }

    #[tokio::test]
    async fn a_section_scope_refuses_items_of_another_section() {
        let memory = MockMemory::new();
        let scope = MemoryScope::section("o/r", MemorySection::Code);
        let err = memory
            .remember(&scope, &[convention("k", "p", "b")])
            .await
            .unwrap_err();
        assert!(err.to_string().contains("cannot be filed"), "{err}");
    }

    #[tokio::test]
    async fn recall_ranks_by_query_overlap_and_prefers_heading_hits() {
        let memory = MockMemory::new();
        let scope = MemoryScope::repo("o/r");
        memory
            .remember(
                &scope,
                &[
                    convention("ports", "src/ports", "One trait per file."),
                    convention("docs", "docs", "Keep markdown under 500 lines."),
                    convention("errors", "src/error.rs", "Return the crate Result."),
                ],
            )
            .await
            .unwrap();
        let hits = memory.recall(&scope, "src/ports trait", 10).await.unwrap();
        assert_eq!(hits[0].item.key, "ports");
        assert!(hits.iter().all(|h| h.item.key != "docs"));
    }

    #[tokio::test]
    async fn recall_honours_the_section_scope() {
        let memory = MockMemory::new();
        let repo = MemoryScope::repo("o/r");
        memory
            .remember(
                &repo,
                &[
                    convention("ports", "src/ports", "One trait per file."),
                    MemoryItem::new("f1", MemoryKind::ReviewFinding, "ports finding", "ports"),
                ],
            )
            .await
            .unwrap();
        let reviews = MemoryScope::section("o/r", MemorySection::Reviews);
        let hits = memory.recall(&reviews, "ports", 10).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].item.kind, MemoryKind::ReviewFinding);
    }

    #[tokio::test]
    async fn other_repositories_are_invisible() {
        let memory = MockMemory::new();
        memory
            .remember(
                &MemoryScope::repo("o/other"),
                &[convention("ports", "src/ports", "One trait per file.")],
            )
            .await
            .unwrap();
        let hits = memory
            .recall(&MemoryScope::repo("o/r"), "ports", 10)
            .await
            .unwrap();
        assert!(hits.is_empty());
    }

    #[tokio::test]
    async fn answers_are_canned_and_otherwise_ungrounded() {
        let memory = MockMemory::new().with_answer("conventions", "Never unwrap.");
        let scope = MemoryScope::repo("o/r");
        let hit = memory
            .answer(&scope, "What conventions apply?", None)
            .await
            .unwrap();
        assert!(hit.is_grounded());
        assert_eq!(hit.answer, "Never unwrap.");
        let miss = memory.answer(&scope, "Who wrote this?", None).await.unwrap();
        assert!(!miss.is_grounded());
    }

    #[tokio::test]
    async fn forget_removes_a_section_and_leaves_the_rest() {
        let memory = MockMemory::new();
        let repo = MemoryScope::repo("o/r");
        memory
            .remember(
                &repo,
                &[
                    convention("ports", "src/ports", "One trait per file."),
                    MemoryItem::new("f1", MemoryKind::ReviewFinding, "t", "b"),
                ],
            )
            .await
            .unwrap();
        let gone = memory
            .forget(&MemoryScope::section("o/r", MemorySection::Reviews))
            .await
            .unwrap();
        assert_eq!(gone, 1);
        assert_eq!(memory.len(), 1);
    }

    #[tokio::test]
    async fn a_failing_memory_fails_every_call() {
        let memory = MockMemory::new();
        memory.fail_with("down");
        assert!(memory.health().await.is_err());
        assert!(
            memory
                .recall(&MemoryScope::repo("o/r"), "q", 1)
                .await
                .is_err()
        );
    }
}
