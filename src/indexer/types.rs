//! The domain types the indexer speaks in, and its freshness state machine.
//!
//! Always compiled. Nothing here touches a database: the states and the
//! transitions between them are ordinary values, so the machine is tested
//! offline and the MongoDB adapter only has to persist it.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::chunk::types::SkippedFile;
use crate::indexer::cost::EmbedUsage;

/// Where a repository's index stands.
///
/// Four states, and the useful thing about them is which one a *failure* lands
/// in. An index that failed halfway is [`IndexState::Failed`] with whatever it
/// managed to write still in place and still queryable — because chunks are
/// upserted before stale ones are deleted, a partial index is a smaller index,
/// never an empty or a wrong one. The alternative, deleting the repository's
/// chunks first, turns every transient failure into a repository with zero
/// chunks and a `failed` status, which is a strictly worse thing to leave a
/// reviewer with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum IndexState {
    /// Never indexed under this signature.
    #[default]
    Absent,
    /// A worker holds the claim right now.
    Indexing,
    /// Indexed and usable.
    Ready,
    /// The last attempt failed. Whatever it wrote is still usable.
    Failed,
}

impl IndexState {
    /// Whether a worker may take the claim from this state.
    ///
    /// Everything except [`IndexState::Indexing`]. Re-indexing a `Ready` index
    /// is normal — that is what a push is — and retrying a `Failed` one must
    /// stay possible or one bad run wedges the repository permanently.
    pub fn claimable(self) -> bool {
        !matches!(self, Self::Indexing)
    }

    /// The state a run in progress lands in.
    pub fn after(self, settled: &Settled) -> Self {
        match settled {
            Settled::Done { .. } => Self::Ready,
            Settled::Failed { .. } => Self::Failed,
        }
    }
}

/// How an indexing run ended, as reported back to the manifest.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "settled", rename_all = "lowercase")]
pub enum Settled {
    /// The run completed, and this is what it leaves on record.
    Done {
        /// The commit the index now reflects.
        revision: Option<String>,
        /// How many chunks the repository has.
        chunks: u64,
        /// What the run spent, to be added to the repository's running total.
        usage: EmbedUsage,
    },
    /// The run gave up.
    Failed {
        /// Why, for a human reading the repository's status.
        message: String,
    },
}

/// A repository's index record.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RepoIndex {
    /// The repository, as `owner/name`.
    pub repo_id: String,
    /// The embedding signature this record is about.
    ///
    /// Part of the identity, not a detail: swapping the embedding model gives a
    /// repository a *different* record that starts at
    /// [`IndexState::Absent`], which is the correct answer — none of the old
    /// vectors are usable.
    pub signature: String,
    /// Where it stands.
    pub state: IndexState,
    /// The commit the index reflects, when it reflects one.
    pub revision: Option<String>,
    /// How many chunks are on record.
    pub chunks: u64,
    /// Why the last run failed, when it did.
    pub message: Option<String>,
    /// What indexing this repository has cost so far.
    pub usage: EmbedUsage,
}

impl RepoIndex {
    /// The record of a repository that has never been indexed.
    pub fn absent(repo_id: impl Into<String>, signature: impl Into<String>) -> Self {
        Self {
            repo_id: repo_id.into(),
            signature: signature.into(),
            state: IndexState::Absent,
            revision: None,
            chunks: 0,
            message: None,
            usage: EmbedUsage::default(),
        }
    }

    /// Whether the index already reflects `revision`.
    ///
    /// Used to skip work outright. Deliberately strict about the state as well
    /// as the revision: an index that failed at this revision is not fresh,
    /// however far it got.
    pub fn is_fresh(&self, revision: &str) -> bool {
        self.state == IndexState::Ready && self.revision.as_deref() == Some(revision)
    }
}

/// The claim a worker holds while it indexes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexLease {
    /// The repository being indexed.
    pub repo_id: String,
    /// The signature being indexed under.
    pub signature: String,
    /// Who holds it, for the log line that explains a requeue.
    pub holder: String,
    /// A unique fence for this particular claim acquisition.
    ///
    /// A holder name identifies a worker, not one lifetime of its claim. The
    /// fence makes a worker that outlived the claim TTL unable to settle or
    /// delete a newer claim acquired under the same name.
    pub fence: String,
}

impl IndexLease {
    /// Build a distinct lease for one successful claim acquisition.
    pub fn new(
        repo_id: impl Into<String>,
        signature: impl Into<String>,
        holder: impl Into<String>,
    ) -> Self {
        static NEXT_FENCE: AtomicU64 = AtomicU64::new(0);
        let tick = NEXT_FENCE.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        Self {
            repo_id: repo_id.into(),
            signature: signature.into(),
            holder: holder.into(),
            fence: format!("{}-{nanos}-{tick}", std::process::id()),
        }
    }
}

/// The result of trying to take the claim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Claim {
    /// The claim is yours.
    Granted(IndexLease),
    /// Somebody else has it. Requeue; do not wait.
    Busy {
        /// Who holds it, when the store could say.
        holder: Option<String>,
    },
}

impl Claim {
    /// The lease, when the claim was granted.
    pub fn lease(&self) -> Option<&IndexLease> {
        match self {
            Self::Granted(lease) => Some(lease),
            Self::Busy { .. } => None,
        }
    }
}

/// One file's contribution to the index, as recorded in the manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexedFile {
    /// The repo-relative path.
    pub path: String,
    /// The ids of the chunks this file produced, in order.
    ///
    /// Ids rather than hashes because they are what the stale-chunk delete
    /// takes, and the hash is already inside the id — storing both would let
    /// them disagree.
    ///
    /// **Confirmed**: every id here is in the index. This is the set the
    /// indexer diffs against to decide what to embed, so an id that is merely
    /// intended must never appear in it.
    pub chunks: Vec<String>,
    /// Ids a run is *about* to write, recorded before the write.
    ///
    /// The crash-safety half of the two-phase write. A run that dies between
    /// embedding and recording would otherwise leave chunks in the index that
    /// no manifest entry mentions: the next run would neither reuse them nor
    /// ever delete them, and stale code would sit in retrieval forever.
    /// Writing the intent first means the next run's stale sweep still knows
    /// about them. They are deliberately *not* counted as indexed — that would
    /// skip embedding a chunk that was never written.
    #[serde(default)]
    pub pending: Vec<String>,
}

impl IndexedFile {
    /// A confirmed record.
    pub fn confirmed(path: impl Into<String>, chunks: Vec<String>) -> Self {
        Self {
            path: path.into(),
            chunks,
            pending: Vec::new(),
        }
    }

    /// Every id this file has ever been associated with, confirmed or intended.
    ///
    /// The candidate set for the stale sweep.
    pub fn every_id(&self) -> Vec<String> {
        let mut ids = self.chunks.clone();
        ids.extend(self.pending.iter().cloned());
        ids.sort();
        ids.dedup();
        ids
    }
}

/// What one indexing run did.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct IndexReport {
    /// Files that were chunked this run.
    pub files: u64,
    /// Chunks written.
    pub upserted: u64,
    /// Stale chunks removed, after the writes.
    pub deleted: u64,
    /// Chunks that were already indexed and cost nothing.
    ///
    /// The number that says whether incremental re-indexing is working: on a
    /// push touching one file of a thousand, this is nearly every chunk.
    pub reused: u64,
    /// Files whose content actually changed this run.
    ///
    /// Not the same as [`IndexReport::files`], which counts every file the run
    /// looked at. This is the subset whose chunk ids differ from what the
    /// manifest already had — the same content-hash comparison that decides
    /// whether to call the embedder, reported so that the code graph can
    /// re-parse the same subset instead of the whole tree.
    ///
    /// Empty after a run that found nothing to do, and *complete* after the
    /// first index of a repository, because a file with no manifest entry
    /// differs from it in every chunk.
    pub changed: Vec<String>,
    /// Paths whose chunks were removed: deleted, newly ignored, or too large.
    pub removed: Vec<String>,
    /// What the embeddings cost.
    pub usage: EmbedUsage,
    /// Every file left out, with a reason.
    pub skipped: Vec<SkippedFile>,
    /// Whether the run stopped early against its spend ceiling.
    ///
    /// A flag rather than an error: what was written before the ceiling is
    /// valid and worth keeping, and the caller needs to know the index is
    /// partial without losing it.
    pub budget_exhausted: bool,
}

impl IndexReport {
    /// A human-readable line, and the surprising skips beneath it.
    pub fn summary(&self) -> String {
        let mut text = format!(
            "{} file(s): {} chunk(s) written, {} reused, {} removed, ${:.4} spent",
            self.files, self.upserted, self.reused, self.deleted, self.usage.cost_usd
        );
        if self.budget_exhausted {
            text.push_str(" (stopped at the spend ceiling; the index is partial)");
        }
        if let Some(report) = crate::chunk::types::report_skips(&self.skipped) {
            text.push('\n');
            text.push_str(&report);
        }
        text
    }
}

/// What a call to the indexer did, at the level the caller cares about.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum IndexOutcome {
    /// The run happened.
    Indexed(IndexReport),
    /// The index already reflected the requested revision; nothing was done.
    AlreadyFresh,
    /// Another worker holds the claim. **Requeue the job.**
    Requeue {
        /// Who holds it, when the store could say.
        holder: Option<String>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_index_in_progress_cannot_be_claimed_and_every_other_state_can() {
        assert!(!IndexState::Indexing.claimable());
        for state in [IndexState::Absent, IndexState::Ready, IndexState::Failed] {
            assert!(state.claimable(), "{state:?} must be retryable");
        }
    }

    #[test]
    fn a_failed_run_leaves_the_repository_retryable_rather_than_wedged() {
        let settled = Settled::Failed {
            message: "provider down".into(),
        };
        let after = IndexState::Indexing.after(&settled);
        assert_eq!(after, IndexState::Failed);
        assert!(after.claimable());
    }

    #[test]
    fn freshness_needs_both_the_revision_and_a_ready_state() {
        let mut record = RepoIndex::absent("o/r", "mock:hash-bag:64");
        record.revision = Some("abc".into());
        record.state = IndexState::Failed;
        assert!(
            !record.is_fresh("abc"),
            "a failed run at this revision is not a fresh index"
        );

        record.state = IndexState::Ready;
        assert!(record.is_fresh("abc"));
        assert!(!record.is_fresh("def"));
    }

    #[test]
    fn a_busy_claim_yields_no_lease() {
        assert!(Claim::Busy { holder: None }.lease().is_none());
        let lease = IndexLease::new("o/r", "s", "worker-1");
        assert_eq!(Claim::Granted(lease.clone()).lease(), Some(&lease));
    }

    #[test]
    fn a_partial_run_says_so_in_its_summary() {
        let report = IndexReport {
            files: 3,
            upserted: 10,
            budget_exhausted: true,
            ..IndexReport::default()
        };
        assert!(report.summary().contains("the index is partial"));
    }
}
