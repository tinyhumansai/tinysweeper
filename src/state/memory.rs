//! The always-compiled offline review-state store.
//!
//! Every port has one of these, and this is the one for
//! [`ReviewStateStore`](crate::ports::review_state::ReviewStateStore). It is
//! not a stub: it is what the dedupe tests run against, and what `local-review`
//! uses when there is no database — a process-lifetime memory is still a
//! memory, and it keeps the incremental path exercised offline.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use crate::error::Result;
use crate::ports::review_state::ReviewStateStore;
use crate::state::types::ReviewedState;

/// An in-memory review-state store, cheap to clone and shared between clones.
#[derive(Debug, Clone, Default)]
pub struct MemoryState {
    entries: Arc<Mutex<BTreeMap<String, ReviewedState>>>,
}

impl MemoryState {
    /// An empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// How many pull requests it remembers.
    pub fn len(&self) -> usize {
        self.entries.lock().expect("memory state lock").len()
    }

    /// Whether it remembers nothing.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[async_trait]
impl ReviewStateStore for MemoryState {
    async fn load_state(&self, key: &str) -> Result<Option<ReviewedState>> {
        Ok(self
            .entries
            .lock()
            .expect("memory state lock")
            .get(key)
            .cloned())
    }

    async fn save_state(&self, key: &str, state: &ReviewedState) -> Result<()> {
        self.entries
            .lock()
            .expect("memory state lock")
            .insert(key.to_string(), state.clone());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn an_unknown_key_is_absent_rather_than_an_error() {
        let store = MemoryState::new();
        assert!(store.load_state("nobody#1").await.expect("loads").is_none());
    }

    #[tokio::test]
    async fn state_survives_and_is_shared_between_clones() {
        let store = MemoryState::new();
        let state = ReviewedState {
            head_sha: "abc123".into(),
            ..ReviewedState::default()
        };
        store.save_state("repo#7", &state).await.expect("saves");

        let other = store.clone();
        assert_eq!(
            other.load_state("repo#7").await.expect("loads"),
            Some(state)
        );
    }

    #[tokio::test]
    async fn saving_again_replaces_the_previous_review() {
        let store = MemoryState::new();
        for sha in ["one", "two"] {
            store
                .save_state(
                    "repo#7",
                    &ReviewedState {
                        head_sha: sha.into(),
                        ..ReviewedState::default()
                    },
                )
                .await
                .expect("saves");
        }
        assert_eq!(store.len(), 1);
        assert_eq!(
            store
                .load_state("repo#7")
                .await
                .expect("loads")
                .expect("present")
                .head_sha,
            "two"
        );
    }
}
