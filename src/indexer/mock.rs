//! The offline manifest, and a counting embedder.
//!
//! Always compiled, and not a stub: [`MockManifest`] implements the same claim
//! semantics as the MongoDB adapter — one holder at a time, a refused claim
//! answered rather than waited on — so the freshness machine is exercised
//! rather than assumed by the tests that run everywhere.
//!
//! [`CountingEmbedder`] is here because the central promise of incremental
//! indexing is a *negative*: re-indexing unchanged content must issue **zero**
//! embedding calls. Nothing about the produced chunks demonstrates that. Only a
//! counter does.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;

use crate::error::Result;
use crate::index::types::EmbedSignature;
use crate::indexer::types::{Claim, IndexLease, IndexState, IndexedFile, RepoIndex, Settled};
use crate::ports::embed::Embedder;
use crate::ports::manifest::IndexManifest;

/// An in-memory index manifest.
#[derive(Debug, Clone, Default)]
pub struct MockManifest {
    // One lock over both maps, which is also what makes `claim` atomic: taking
    // the claim and writing the holder must not be separable, or two workers
    // interleave between them and both believe they won.
    inner: Arc<Mutex<Inner>>,
}

#[derive(Debug, Default)]
struct Inner {
    records: BTreeMap<(String, String), (RepoIndex, Option<String>)>,
    files: BTreeMap<(String, String, String), IndexedFile>,
}

impl MockManifest {
    /// An empty manifest.
    pub fn new() -> Self {
        Self::default()
    }

    /// The record for a repository, as stored.
    ///
    /// Named `snapshot` rather than `record` so it does not shadow the port's
    /// `record` at a call site — a mock whose inherent method hides the trait
    /// method it is testing is a trap.
    pub fn snapshot(&self, repo_id: &str, signature: &EmbedSignature) -> RepoIndex {
        let inner = self.inner.lock().expect("manifest lock");
        inner
            .records
            .get(&key(repo_id, signature))
            .map(|(record, _)| record.clone())
            .unwrap_or_else(|| RepoIndex::absent(repo_id, signature.key()))
    }

}

fn key(repo_id: &str, signature: &EmbedSignature) -> (String, String) {
    (repo_id.to_string(), signature.key())
}

#[async_trait]
impl IndexManifest for MockManifest {
    async fn claim(&self, repo_id: &str, signature: &EmbedSignature, holder: &str) -> Result<Claim> {
        let mut inner = self.inner.lock().expect("manifest lock");
        let entry = inner
            .records
            .entry(key(repo_id, signature))
            .or_insert_with(|| (RepoIndex::absent(repo_id, signature.key()), None));

        if !entry.0.state.claimable() {
            return Ok(Claim::Busy {
                holder: entry.1.clone(),
            });
        }
        entry.0.state = IndexState::Indexing;
        entry.1 = Some(holder.to_string());
        Ok(Claim::Granted(IndexLease {
            repo_id: repo_id.to_string(),
            signature: signature.key(),
            holder: holder.to_string(),
        }))
    }

    async fn release(&self, lease: &IndexLease, settled: &Settled) -> Result<()> {
        let mut inner = self.inner.lock().expect("manifest lock");
        let Some(entry) = inner
            .records
            .get_mut(&(lease.repo_id.clone(), lease.signature.clone()))
        else {
            return Ok(());
        };
        entry.0.state = entry.0.state.after(settled);
        entry.1 = None;
        match settled {
            Settled::Done {
                revision,
                chunks,
                usage,
            } => {
                entry.0.revision.clone_from(revision);
                entry.0.chunks = *chunks;
                entry.0.message = None;
                entry.0.usage.add(*usage);
            }
            Settled::Failed { message } => {
                entry.0.message = Some(message.clone());
            }
        }
        Ok(())
    }

    async fn state(&self, repo_id: &str, signature: &EmbedSignature) -> Result<RepoIndex> {
        Ok(self.record(repo_id, signature))
    }

    async fn indexed(
        &self,
        repo_id: &str,
        signature: &EmbedSignature,
        paths: &[String],
    ) -> Result<Vec<IndexedFile>> {
        let inner = self.inner.lock().expect("manifest lock");
        Ok(paths
            .iter()
            .filter_map(|path| {
                inner
                    .files
                    .get(&(repo_id.to_string(), signature.key(), path.clone()))
                    .cloned()
            })
            .collect())
    }

    async fn paths(&self, repo_id: &str, signature: &EmbedSignature) -> Result<Vec<String>> {
        let inner = self.inner.lock().expect("manifest lock");
        Ok(inner
            .files
            .keys()
            .filter(|(repo, sig, _)| repo == repo_id && *sig == signature.key())
            .map(|(_, _, path)| path.clone())
            .collect())
    }

    async fn record(
        &self,
        repo_id: &str,
        signature: &EmbedSignature,
        files: &[IndexedFile],
    ) -> Result<()> {
        let mut inner = self.inner.lock().expect("manifest lock");
        for file in files {
            inner.files.insert(
                (repo_id.to_string(), signature.key(), file.path.clone()),
                file.clone(),
            );
        }
        Ok(())
    }

    async fn forget(
        &self,
        repo_id: &str,
        signature: &EmbedSignature,
        paths: &[String],
    ) -> Result<()> {
        let mut inner = self.inner.lock().expect("manifest lock");
        for path in paths {
            inner
                .files
                .remove(&(repo_id.to_string(), signature.key(), path.clone()));
        }
        Ok(())
    }
}

/// An embedder that records how much work it was asked to do.
///
/// Wraps another embedder rather than replacing it, so the vectors are still
/// the deterministic ones every other retrieval test depends on.
#[derive(Debug)]
pub struct CountingEmbedder<E: Embedder> {
    inner: E,
    calls: AtomicU64,
    texts: AtomicU64,
}

impl<E: Embedder> CountingEmbedder<E> {
    /// Wrap `inner`.
    pub fn new(inner: E) -> Self {
        Self {
            inner,
            calls: AtomicU64::new(0),
            texts: AtomicU64::new(0),
        }
    }

    /// How many batch calls have been made.
    pub fn calls(&self) -> u64 {
        self.calls.load(Ordering::Relaxed)
    }

    /// How many texts have been embedded across those calls.
    pub fn texts(&self) -> u64 {
        self.texts.load(Ordering::Relaxed)
    }

    /// Reset both counters, for the second half of a before/after assertion.
    pub fn reset(&self) {
        self.calls.store(0, Ordering::Relaxed);
        self.texts.store(0, Ordering::Relaxed);
    }
}

#[async_trait]
impl<E: Embedder> Embedder for CountingEmbedder<E> {
    fn signature(&self) -> EmbedSignature {
        self.inner.signature()
    }

    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        self.texts.fetch_add(texts.len() as u64, Ordering::Relaxed);
        self.inner.embed(texts).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::MockEmbedder;
    use crate::indexer::cost::EmbedUsage;

    fn signature() -> EmbedSignature {
        EmbedSignature::new("mock", "hash-bag", 8)
    }

    #[tokio::test]
    async fn a_second_claim_is_refused_with_the_holder_rather_than_blocking() {
        let manifest = MockManifest::new();
        let first = manifest.claim("o/r", &signature(), "worker-1").await.expect("claims");
        assert!(matches!(first, Claim::Granted(_)));

        let second = manifest.claim("o/r", &signature(), "worker-2").await.expect("answers");
        assert_eq!(
            second,
            Claim::Busy {
                holder: Some("worker-1".into())
            },
            "a contended claim must return, so the caller can requeue"
        );
    }

    #[tokio::test]
    async fn releasing_records_the_revision_and_adds_to_the_running_spend() {
        let manifest = MockManifest::new();
        let lease = match manifest.claim("o/r", &signature(), "w").await.expect("claims") {
            Claim::Granted(lease) => lease,
            other => panic!("{other:?}"),
        };
        manifest
            .release(
                &lease,
                &Settled::Done {
                    revision: Some("abc".into()),
                    chunks: 12,
                    usage: EmbedUsage {
                        calls: 2,
                        tokens: 100,
                        cost_usd: 0.5,
                    },
                },
            )
            .await
            .expect("releases");

        let record = manifest.snapshot("o/r", &signature());
        assert_eq!(record.state, IndexState::Ready);
        assert!(record.is_fresh("abc"));
        assert_eq!(record.usage.cost_usd, 0.5);

        // And the claim is free again, which is what makes the next push work.
        assert!(matches!(
            manifest.claim("o/r", &signature(), "w2").await.expect("claims"),
            Claim::Granted(_)
        ));
    }

    #[tokio::test]
    async fn a_failed_release_keeps_the_message_and_frees_the_claim() {
        let manifest = MockManifest::new();
        let Claim::Granted(lease) = manifest.claim("o/r", &signature(), "w").await.expect("claims")
        else {
            panic!("not granted");
        };
        manifest
            .release(
                &lease,
                &Settled::Failed {
                    message: "provider down".into(),
                },
            )
            .await
            .expect("releases");

        let record = manifest.snapshot("o/r", &signature());
        assert_eq!(record.state, IndexState::Failed);
        assert_eq!(record.message.as_deref(), Some("provider down"));
        assert!(record.state.claimable(), "a failure must be retryable");
    }

    #[tokio::test]
    async fn a_different_signature_is_a_different_record() {
        // Swapping the embedding model must not make the old index look fresh.
        let manifest = MockManifest::new();
        let Claim::Granted(lease) = manifest.claim("o/r", &signature(), "w").await.expect("claims")
        else {
            panic!("not granted");
        };
        manifest
            .release(
                &lease,
                &Settled::Done {
                    revision: Some("abc".into()),
                    chunks: 1,
                    usage: EmbedUsage::default(),
                },
            )
            .await
            .expect("releases");

        let other = EmbedSignature::new("mock", "hash-bag", 16);
        assert_eq!(manifest.snapshot("o/r", &other).state, IndexState::Absent);
    }

    #[tokio::test]
    async fn recorded_files_come_back_and_forgotten_ones_do_not() {
        let manifest = MockManifest::new();
        let file = IndexedFile::confirmed("src/a.rs", vec!["id-1".into()]);
        manifest
            .record("o/r", &signature(), std::slice::from_ref(&file))
            .await
            .expect("records");
        assert_eq!(
            manifest
                .indexed("o/r", &signature(), &["src/a.rs".to_string()])
                .await
                .expect("reads"),
            vec![file]
        );

        manifest
            .forget("o/r", &signature(), &["src/a.rs".to_string()])
            .await
            .expect("forgets");
        assert!(
            manifest
                .indexed("o/r", &signature(), &["src/a.rs".to_string()])
                .await
                .expect("reads")
                .is_empty()
        );
    }

    #[tokio::test]
    async fn the_counting_embedder_counts_calls_and_texts() {
        let embedder = CountingEmbedder::new(MockEmbedder::new(8));
        embedder
            .embed(&["a".to_string(), "b".to_string()])
            .await
            .expect("embeds");
        assert_eq!((embedder.calls(), embedder.texts()), (1, 2));
        embedder.reset();
        assert_eq!((embedder.calls(), embedder.texts()), (0, 0));
    }
}
