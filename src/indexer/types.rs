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
        /// How many chunks the repository has *now*, when the run changed
        /// that before it failed. A deletion that happened is a deletion
        /// whether or not the embedding after it did; leaving the old count
        /// on record would report chunks that are not there, and a run that
        /// deleted the last of them would look `Ready` rather than cold.
        /// `None` leaves the count as it was — and is what a record written
        /// before this field existed reads as.
        #[serde(default)]
        chunks: Option<u64>,
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
    /// What `pending` holds. `false` at the intent: ids about to be written,
    /// not yet in the index, never counted. `true` at the confirmation: the
    /// *old* ids a replacement is about to delete — still in the index, and
    /// still in the repository's chunk count until that delete lands. A
    /// removal that counts what it subtracts needs the difference.
    #[serde(default)]
    pub pending_is_stale: bool,
}

impl IndexedFile {
    /// A confirmed record.
    pub fn confirmed(path: impl Into<String>, chunks: Vec<String>) -> Self {
        Self {
            path: path.into(),
            chunks,
            pending: Vec::new(),
            pending_is_stale: false,
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
    /// Submodule directories the checkout was missing through no decision of
    /// the operator's — a fetch that failed this time. Their rows were kept,
    /// and the revision is not claimed, so the next delivery tries again.
    #[serde(default)]
    pub unfetched: Vec<String>,
    /// Whether the code graph must be rebuilt whole after this run rather
    /// than incrementally from `changed`: the record this run started from
    /// was one that never completed, and its graph was never synced from —
    /// or was synced from a tree that lacked something.
    #[serde(default)]
    pub rebuild_graph: bool,
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
        if !self.unfetched.is_empty() {
            text.push_str(&format!(
                " (submodule(s) not fetched, kept as indexed: {})",
                self.unfetched.join(", ")
            ));
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

/// What the manifest records as the revision an index reflects.
///
/// The commit alone is not enough: which submodules were fetched into the
/// checkout is part of what got indexed, and that is decided by
/// `retrieval.submodules`, not by the commit. An operator who removes a
/// repository from the list at an unchanged head would otherwise be told the
/// index is fresh — and `Retriever::retrieve` applies no allow-list of its
/// own, so the chunks policy says may no longer be read would keep reaching
/// prompts until the next push. Folding the list into the recorded revision
/// makes a policy change a stale index. An empty list records the bare
/// commit, so the manifests written before this existed stay fresh.
///
/// One function for both sides on purpose: the server records it, and
/// `Retriever::freshness` compares against it. Two spellings would report
/// every allow-listed repository as permanently stale.
pub fn indexed_revision(revision: &str, submodules: &[String]) -> String {
    if submodules.is_empty() {
        return revision.to_string();
    }
    let mut allowed: Vec<&str> = submodules.iter().map(String::as_str).collect();
    allowed.sort_unstable();
    allowed.dedup();
    let mut hasher = <sha2::Sha256 as sha2::Digest>::new();
    for repo in allowed {
        sha2::Digest::update(&mut hasher, repo.as_bytes());
        sha2::Digest::update(&mut hasher, b"\0");
    }
    let digest = sha2::Digest::finalize(hasher);
    format!(
        "{revision}+submodules:{:016x}",
        u64::from_be_bytes(digest[..8].try_into().unwrap())
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_recorded_revision_moves_with_the_submodule_policy() {
        // No policy: the bare commit, so manifests written before the list
        // existed are still fresh.
        assert_eq!(indexed_revision("abc", &[]), "abc");

        let one = indexed_revision("abc", &["o/lib".into()]);
        let two = indexed_revision("abc", &["o/lib".into(), "o/core".into()]);
        assert!(one.starts_with("abc+submodules:"));
        assert_ne!(
            one, two,
            "changing the allow-list must make the index stale"
        );
        assert_ne!(one, "abc", "a policy is not the bare commit");

        // Order and repeats are not policy.
        let reordered = indexed_revision("abc", &["o/core".into(), "o/lib".into(), "o/lib".into()]);
        assert_eq!(two, reordered);
    }

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
            chunks: None,
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
