//! The indexing run: claim, chunk, embed what changed, sweep what did not.
//!
//! Always compiled — it is written against the three ports, so the whole thing
//! runs offline against the mocks.
//!
//! The ordering here is the module's entire point, and it is the opposite of
//! the obvious one. The obvious order is *delete this repository's chunks, then
//! embed and write the new ones*, because it is one line and it is trivially
//! correct when it finishes. It is also catastrophic when it does not: a
//! provider timeout, a killed process, a network blip anywhere in the embedding
//! loop leaves the repository with **zero** chunks and a failed status, and it
//! stays that way until somebody notices that reviews stopped citing anything.
//!
//! So: write first, delete afterwards, and delete only what is provably
//! superseded.
//!
//! 1. Chunk the changed files and diff their chunk ids against the manifest.
//! 2. Record the ids about to be written, as *pending*.
//! 3. Embed and upsert the ids that are new.
//! 4. Confirm those ids in the manifest.
//! 5. Delete the ids the previous pass had and this one does not.
//!
//! An interruption at any step leaves the index a superset of the truth, which
//! degrades retrieval slightly. The reverse order leaves it a subset — usually
//! the empty set — which breaks it silently.

use std::collections::BTreeSet;
use std::path::{Component, Path};

use crate::chunk::types::{SkipReason, SkippedFile};
use crate::chunk::{Chunker, Selector};
use crate::error::{Error, Result};
use crate::index::types::{Chunk, EmbedSignature, EmbeddedChunk};
use crate::indexer::cost::{EmbedUsage, estimate_tokens};
use crate::indexer::types::{Claim, IndexLease, IndexOutcome, IndexReport, IndexedFile, Settled};
use crate::ports::embed::Embedder;
use crate::ports::index::ChunkIndex;
use crate::ports::manifest::IndexManifest;

/// How many texts go in one embedding call by default.
///
/// Large enough that per-call latency stops dominating a full index, small
/// enough that a provider's per-request size limit is not the thing that
/// discovers the number for us. It is a ceiling on *count* only — see
/// [`DEFAULT_MAX_BATCH_TOKENS`], which is the ceiling that actually binds.
pub const DEFAULT_BATCH: usize = 64;

/// Estimated-token ceiling on one embedding call, by default.
///
/// A count alone does not bound a request, and assuming it did is what broke
/// indexing in production: 64 chunks of real source were rejected with
/// `max_tokens_per_request` — 467,846 tokens against OpenAI's ceiling of
/// 300,000 — so every large repository silently degraded to a diff-only
/// review while small ones indexed fine.
///
/// The number carries two corrections on top of that 300,000.
///
/// [`estimate_tokens`](crate::indexer::cost::estimate_tokens) assumes four
/// bytes to a token, which holds for prose and is roughly **half** the true
/// rate for code, where punctuation is dense. The rejected batch is the
/// measurement: 64 chunks capped at 14,400 chars is at most 230,400 estimated
/// tokens, and the provider counted 467,846 — a little over 2x. So a budget
/// expressed in estimated tokens must be halved before it means anything to a
/// provider counting real ones.
///
/// 120,000 estimated tokens is therefore ~240,000 real ones at that ratio,
/// leaving room under 300,000 for source denser than the sample. Deployments
/// on a provider with a different ceiling set `embeddings.max_request_tokens`
/// rather than editing this.
pub const DEFAULT_MAX_BATCH_TOKENS: u64 = 120_000;

/// How many files are carried in memory at once.
///
/// Chunking holds every chunk's text until it is embedded, so a full index of a
/// monorepo would otherwise be the whole repository in memory at peak. Grouping
/// bounds that, and the write-then-delete ordering holds within each group, so
/// a failure part-way through still leaves every completed group correct.
pub const DEFAULT_GROUP: usize = 200;

/// Runs indexing against the three retrieval ports.
pub struct Indexer<'a> {
    embedder: &'a dyn Embedder,
    index: &'a dyn ChunkIndex,
    manifest: &'a dyn IndexManifest,
    chunker: Chunker,
    selector: Selector,
    batch: usize,
    max_batch_tokens: u64,
    group: usize,
    budget_usd: Option<f64>,
    holder: String,
    /// Directories whose absent paths are deleted before anything is
    /// embedded: the submodules this checkout did not fetch. See
    /// [`Indexer::revoking`].
    revoked: Vec<String>,
    /// Directories whose absent paths are *kept*: submodules the checkout
    /// should have but a fetch failed to bring. See [`Indexer::missing`].
    missing: Vec<String>,
}

impl<'a> Indexer<'a> {
    /// An indexer with the default chunker, selector and batch sizes.
    pub fn new(
        embedder: &'a dyn Embedder,
        index: &'a dyn ChunkIndex,
        manifest: &'a dyn IndexManifest,
    ) -> Result<Self> {
        Ok(Self {
            embedder,
            index,
            manifest,
            chunker: Chunker::new(),
            selector: Selector::new(&[])?,
            batch: DEFAULT_BATCH,
            max_batch_tokens: DEFAULT_MAX_BATCH_TOKENS,
            group: DEFAULT_GROUP,
            budget_usd: None,
            holder: format!("pid-{}", std::process::id()),
            revoked: Vec::new(),
            missing: Vec::new(),
        })
    }

    /// Name the submodule directories a fetch failed to bring this time.
    ///
    /// The opposite of [`Indexer::revoking`]. These are allowed, and absent
    /// only because the network or the token failed today; their paths are
    /// left out of the run's removal step so the index keeps serving what it
    /// had, and the run does not claim the revision, so the next delivery at
    /// the same head fetches and indexes them rather than believing the index
    /// is fresh without them.
    pub fn missing(mut self, submodule_dirs: Vec<String>) -> Self {
        self.missing = dirs_of(submodule_dirs);
        self
    }

    /// Name the submodule directories this checkout did not fetch.
    ///
    /// A path the manifest knows under one of these is deleted *before* the
    /// embedding pass rather than after it. Ordinary removals keep their
    /// place at the end — write before delete, so a rename whose new path
    /// cannot be embedded still has its old one — but a path under an
    /// unfetched submodule has no replacement coming, and the reason it is
    /// unfetched is usually that the operator took the repository off
    /// `retrieval.submodules`. That is a revocation, and a review querying
    /// this index while the rebuild is still embedding, or after it stops on
    /// budget, must not be handed that repository's code. Only paths absent
    /// from the checkout are touched: a `.gitmodules` entry naming a
    /// directory that is really on disk names nothing that is gone.
    pub fn revoking(mut self, submodule_dirs: Vec<String>) -> Self {
        self.revoked = dirs_of(submodule_dirs);
        self
    }

    /// Use a caller-configured file selector — normally one built from
    /// `config.paths.ignore`.
    pub fn with_selector(mut self, selector: Selector) -> Self {
        self.selector = selector;
        self
    }

    /// Use a caller-configured chunker.
    pub fn with_chunker(mut self, chunker: Chunker) -> Self {
        self.chunker = chunker;
        self
    }

    /// Set the embedding batch size, in texts per call.
    pub fn with_batch(mut self, batch: usize) -> Self {
        self.batch = batch.max(1);
        self
    }

    /// Set the estimated-token ceiling on one embedding call.
    ///
    /// Zero is read as [`DEFAULT_MAX_BATCH_TOKENS`] rather than as "no limit":
    /// an unbounded batch is the bug this ceiling exists to prevent, so it is
    /// not something a config typo should be able to switch back on.
    pub fn with_max_batch_tokens(mut self, max_batch_tokens: u64) -> Self {
        self.max_batch_tokens = match max_batch_tokens {
            0 => DEFAULT_MAX_BATCH_TOKENS,
            n => n,
        };
        self
    }

    /// Set how many files are chunked before being written.
    pub fn with_group(mut self, group: usize) -> Self {
        self.group = group.max(1);
        self
    }

    /// Stop embedding once a run has spent this much.
    ///
    /// The run stops at a batch boundary and reports
    /// [`IndexReport::budget_exhausted`]; it does not fail, and it does not
    /// discard what it wrote. A partial index that says it is partial is worth
    /// far more than a failed one.
    pub fn with_budget(mut self, budget_usd: f64) -> Self {
        self.budget_usd = Some(budget_usd);
        self
    }

    /// Name this worker, for the log line that explains a requeue.
    pub fn as_holder(mut self, holder: impl Into<String>) -> Self {
        self.holder = holder.into();
        self
    }

    /// Index a whole checkout.
    ///
    /// Returns [`IndexOutcome::AlreadyFresh`] when the manifest already has this
    /// revision, and [`IndexOutcome::Requeue`] when another worker holds the
    /// claim — never a wait.
    pub async fn index_repo(
        &self,
        repo_id: &str,
        revision: &str,
        root: &Path,
    ) -> Result<IndexOutcome> {
        let signature = self.embedder.signature();
        if self
            .manifest
            .state(repo_id, &signature)
            .await?
            .is_fresh(revision)
        {
            return Ok(IndexOutcome::AlreadyFresh);
        }

        let selection = self.selector.walk(root)?;
        // Anything the manifest knows about that the walk no longer sees has
        // been deleted or newly ignored, and its chunks have to go. Which
        // paths the manifest knows is read under the claim — see `Removed`.
        self.guarded(
            repo_id,
            revision,
            root,
            selection.selected,
            selection.skipped,
            Removed::NotSeenByTheWalk,
        )
        .await
    }

    /// Re-index only the paths a push touched.
    ///
    /// A path that no longer exists on disk is treated as a deletion, which is
    /// what makes this usable straight from a diff's changed-file list without
    /// the caller having to classify anything first.
    pub async fn index_paths(
        &self,
        repo_id: &str,
        revision: &str,
        root: &Path,
        paths: &[String],
    ) -> Result<IndexOutcome> {
        let signature = self.embedder.signature();
        let state = self.manifest.state(repo_id, &signature).await?;
        // A changed-path list cannot establish a baseline: it omits every
        // untouched file. Fall back to a full walk until one has completed.
        if state.revision.is_none() {
            return self.index_repo(repo_id, revision, root).await;
        }
        let mut sized = Vec::new();
        let mut removed = Vec::new();
        for path in paths {
            let relative = Path::new(path);
            let unsafe_path = relative.is_absolute()
                || relative
                    .components()
                    .any(|part| matches!(part, Component::ParentDir));
            match (!unsafe_path).then(|| std::fs::symlink_metadata(root.join(relative))) {
                Some(Ok(metadata)) if metadata.file_type().is_file() => {
                    sized.push((path.clone(), metadata.len()))
                }
                _ => removed.push(path.clone()),
            }
        }

        let selection = self.selector.select(sized);
        // A file that is still on disk but no longer selected — it grew past
        // the cap, or an ignore glob now covers it — must lose its chunks too,
        // or retrieval keeps quoting a file we have stopped tracking.
        removed.extend(selection.skipped.iter().map(|s| s.path.clone()));

        self.guarded(
            repo_id,
            revision,
            root,
            selection.selected,
            selection.skipped,
            Removed::These(removed),
        )
        .await
    }

    /// Take the claim, run, and release it whatever happens.
    async fn guarded(
        &self,
        repo_id: &str,
        revision: &str,
        root: &Path,
        selected: Vec<String>,
        skipped: Vec<SkippedFile>,
        removed: Removed,
    ) -> Result<IndexOutcome> {
        let signature = self.embedder.signature();
        let lease = match self
            .manifest
            .claim(repo_id, &signature, &self.holder)
            .await?
        {
            Claim::Granted(lease) => lease,
            Claim::Busy { holder } => {
                tracing::info!(
                    repo = repo_id,
                    holder = ?holder,
                    "another worker is indexing this repository; requeueing"
                );
                return Ok(IndexOutcome::Requeue { holder });
            }
        };

        // The count this run starts from, read under the claim: read before
        // it, a worker finishing at that moment could settle a newer count
        // that this run would then overwrite with its stale baseline plus
        // its own deltas. Read after the writes it would be the same number
        // — nothing but `release` moves it — but under the claim, before the
        // run, is the reading nobody has to think about. A read that fails
        // releases the claim, or every later delivery requeues against a
        // lease nobody holds until its TTL.
        let state = match self.manifest.state(repo_id, &signature).await {
            Ok(state) => state,
            Err(err) => return Err(self.release_failed(&lease, err).await),
        };
        let before = state.chunks;
        // Read under the claim for the same reason: a worker under the old
        // policy confirming a submodule's files between a read before the
        // claim and the claim itself would leave those paths out of the
        // removal set, and the policy that revoked them settled as fresh.
        let removed = match removed {
            Removed::These(paths) => paths,
            Removed::NotSeenByTheWalk => {
                let seen: BTreeSet<&String> = selected.iter().collect();
                match self.manifest.paths(repo_id, &signature).await {
                    Ok(known) => known
                        .into_iter()
                        .filter(|path| !seen.contains(path))
                        .collect(),
                    Err(err) => return Err(self.release_failed(&lease, err).await),
                }
            }
        };

        let mut report = IndexReport {
            skipped,
            // Decided under the claim, from the record this run starts from:
            // a run that never completed — cold, budget, a missing
            // submodule, or one that failed part-way — leaves either no
            // revision or a failed state, and either means the graph is
            // owed a whole rebuild rather than an incremental one keyed on
            // a `changed` list that the incomplete run already confirmed.
            // Not the state: `claim` has just set it to `Indexing`. A record
            // that never completed has no revision; one whose last run failed
            // still carries that run's message (a completed run clears it).
            rebuild_graph: state.revision.is_none() || state.message.is_some(),
            // A previous run that could not account for what it confirmed
            // said so; this run settles from a recount, not from deltas.
            recount: state
                .message
                .as_deref()
                .is_some_and(|message| message.contains(crate::indexer::types::COUNT_UNCERTAIN)),
            ..IndexReport::default()
        };
        let outcome = self
            .run(repo_id, &signature, root, selected, removed, &mut report)
            .await;

        // Released on both paths. A claim only released on success is a claim a
        // crashed run holds until its TTL expires, and every push in between is
        // requeued for nothing.
        match outcome {
            Ok(()) => {
                let chunks = if report.recount {
                    match self.recount(repo_id, &signature).await {
                        Ok(chunks) => chunks,
                        // The recount is the one read that decides the count,
                        // so a failure there keeps the marker for the next
                        // claimant — and releases the claim, or nobody is.
                        Err(err) => {
                            let settled = Settled::Failed {
                                message: format!(
                                    "{err} {}",
                                    crate::indexer::types::COUNT_UNCERTAIN
                                ),
                                chunks: None,
                            };
                            if let Err(nested) = self.manifest.release(&lease, &settled).await {
                                tracing::warn!(error = %nested, "could not release the index claim");
                            }
                            return Err(err);
                        }
                    }
                } else {
                    before
                        .saturating_add(report.upserted)
                        .saturating_sub(report.deleted)
                };
                self.settle(&lease, chunks, revision, &report).await?;
                Ok(IndexOutcome::Indexed(report))
            }
            Err(err) => {
                // What the run wrote and deleted before it failed is on disk
                // whatever the error says; the count on record must say so
                // too, or the next run inherits a total for chunks that are
                // not there. A run that could not tell what it confirmed
                // says so in its message, and the next run recounts.
                let chunks = (report.upserted != 0 || report.deleted != 0).then(|| {
                    before
                        .saturating_add(report.upserted)
                        .saturating_sub(report.deleted)
                });
                let message = if report.recount {
                    format!("{err} {}", crate::indexer::types::COUNT_UNCERTAIN)
                } else {
                    err.to_string()
                };
                let settled = Settled::Failed { message, chunks };
                // A release failure must not mask the error that caused it.
                if let Err(nested) = self.manifest.release(&lease, &settled).await {
                    tracing::warn!(error = %nested, "could not release the index claim");
                }
                Err(err)
            }
        }
    }

    /// Release a claim for a run that failed before it wrote anything,
    /// handing the error back to return.
    async fn release_failed(&self, lease: &IndexLease, err: Error) -> Error {
        let settled = Settled::Failed {
            message: err.to_string(),
            chunks: None,
        };
        if let Err(nested) = self.manifest.release(lease, &settled).await {
            tracing::warn!(error = %nested, "could not release the index claim");
        }
        err
    }

    /// The repository's counted rows, from the manifest rather than from a
    /// running total: every confirmed id, plus every confirmation's pending
    /// set. One repository-wide read, spent only after a run that could not
    /// account for itself.
    async fn recount(&self, repo_id: &str, signature: &EmbedSignature) -> Result<u64> {
        let paths = self.manifest.paths(repo_id, signature).await?;
        let files = self.manifest.indexed(repo_id, signature, &paths).await?;
        Ok(files
            .iter()
            .map(|file| {
                file.chunks.len() as u64
                    + if file.pending_is_stale {
                        file.pending.len() as u64
                    } else {
                        0
                    }
            })
            .sum())
    }

    async fn settle(
        &self,
        lease: &IndexLease,
        chunks: u64,
        revision: &str,
        report: &IndexReport,
    ) -> Result<()> {
        self.manifest
            .release(
                lease,
                &Settled::Done {
                    // A partial run must not claim the revision: saying so would
                    // make the next push skip the work that was never finished.
                    revision: (!report.budget_exhausted && report.unfetched.is_empty())
                        .then(|| revision.to_string()),
                    chunks,
                    usage: report.usage,
                },
            )
            .await
    }

    async fn run(
        &self,
        repo_id: &str,
        signature: &EmbedSignature,
        root: &Path,
        selected: Vec<String>,
        removed: Vec<String>,
        report: &mut IndexReport,
    ) -> Result<()> {
        // Revocations first, everything else after the writes. See
        // [`Indexer::revoking`] for why the two kinds of removal are ordered
        // differently.
        //
        // Only the *rows* go early. The manifest keeps the paths until the
        // run's own removal step below, so a run that fails between here and
        // there leaves them discoverable: the next full walk finds them
        // absent again, deletes nothing (already gone), and carries them in
        // `removed` to the graph sync that only an `Indexed` outcome reaches.
        // Forgetting them here would make that graph cleanup unreachable.
        //
        // A path under a submodule that merely failed to fetch is not removed
        // at all: it is absent from this checkout, not from the repository.
        // See [`Indexer::missing`].
        let under =
            |dirs: &[String], path: &String| dirs.iter().any(|dir| path.starts_with(dir.as_str()));
        //
        // Revocation wins a tie. `.gitmodules` is the contributor's, and two
        // entries for one path — one allow-listed whose fetch failed, one not
        // allow-listed — would otherwise let the "missing" reading keep rows
        // the operator revoked.
        let (_missing, removed): (Vec<String>, Vec<String>) = removed
            .into_iter()
            .partition(|path| under(&self.missing, path) && !under(&self.revoked, path));
        // Named whether or not the index held anything under them: a cold
        // repository, or a submodule allow-listed today, has no old rows to
        // keep — and still must not have its head claimed as indexed.
        report.unfetched = self
            .missing
            .iter()
            // A directory the operator revoked — or anything under one — is
            // not one the run is waiting on, whatever a second `.gitmodules`
            // entry says.
            .filter(|dir| {
                !self
                    .revoked
                    .iter()
                    .any(|revoked| dir.starts_with(revoked.as_str()))
            })
            .map(|dir| dir.trim_end_matches('/').to_string())
            .collect();
        let revoked: Vec<String> = removed
            .iter()
            .filter(|path| under(&self.revoked, path))
            .cloned()
            .collect();
        if !revoked.is_empty() {
            self.remove_rows(repo_id, signature, &revoked, report)
                .await?;
        }

        for group in selected.chunks(self.group) {
            if report.budget_exhausted {
                break;
            }
            self.index_group(repo_id, signature, root, group, report)
                .await?;
        }

        // Rows a revocation already deleted are deleted again here for
        // nothing, and forgotten for the first time. The count comes from the
        // manifest, not from the store's tally of rows it deleted: the store
        // also holds rows an earlier attempt wrote and never confirmed, which
        // were never counted and must not be subtracted.
        if !removed.is_empty() {
            self.remove_rows(repo_id, signature, &removed, report)
                .await?;
            self.manifest.forget(repo_id, signature, &removed).await?;
            report.removed = removed;
        }

        Ok(())
    }

    /// Delete every row under `paths`, counting the *counted* ones into the
    /// report.
    ///
    /// Two deletes, for two different questions. The number `RepoIndex::chunks`
    /// tracks is confirmed rows, so the decrement is the confirmed ids the
    /// manifest has on record — deleted by id, so the store answers how many
    /// of *those* it removed, which is zero when an earlier attempt already
    /// removed them and the run that would have forgotten them failed. Then
    /// the path sweep, uncounted, for rows an earlier attempt wrote and never
    /// confirmed: they exist, they were never added to the count, and they
    /// must not be subtracted from it.
    ///
    /// The count lands in the report between the two, so a sweep the store
    /// refuses still leaves the first delete — which happened — on record
    /// for the failed run to settle.
    async fn remove_rows(
        &self,
        repo_id: &str,
        signature: &EmbedSignature,
        paths: &[String],
        report: &mut IndexReport,
    ) -> Result<()> {
        if paths.is_empty() {
            return Ok(());
        }
        // Confirmed ids, and the ids a *confirmation* left pending: the old
        // rows of a replacement whose delete never ran are still in the
        // store and still in the count, and go by id like the rest. An
        // intent's pending ids are the opposite — about to be written, never
        // counted — and are left to the uncounted sweep.
        let confirmed: Vec<String> = self
            .manifest
            .indexed(repo_id, signature, paths)
            .await?
            .into_iter()
            .flat_map(|file| {
                let stale = if file.pending_is_stale {
                    file.pending
                } else {
                    Vec::new()
                };
                file.chunks.into_iter().chain(stale)
            })
            .collect();
        if !confirmed.is_empty() {
            report.deleted += self.index.delete_chunks(repo_id, &confirmed).await?;
        }
        self.index.delete_paths(repo_id, paths).await?;
        Ok(())
    }

    async fn index_group(
        &self,
        repo_id: &str,
        signature: &EmbedSignature,
        root: &Path,
        group: &[String],
        report: &mut IndexReport,
    ) -> Result<()> {
        let known = self.manifest.indexed(repo_id, signature, group).await?;

        let mut work: Vec<FileWork> = Vec::new();
        for path in group {
            match std::fs::read(root.join(path)) {
                Ok(bytes) => match self.chunker.chunk_bytes(repo_id, path, &bytes) {
                    Ok(chunks) => {
                        let previous = known.iter().find(|file| file.path == *path);
                        work.push(FileWork::new(path.clone(), chunks, previous));
                    }
                    Err(reason) => report.skipped.push(SkippedFile {
                        path: path.clone(),
                        reason,
                    }),
                },
                Err(err) => report.skipped.push(SkippedFile {
                    path: path.clone(),
                    reason: SkipReason::Unreadable {
                        message: err.to_string(),
                    },
                }),
            }
        }
        if work.is_empty() {
            return Ok(());
        }
        report.files += work.len() as u64;
        report.reused += work.iter().map(|file| file.reused).sum::<u64>();
        // Recorded before the writes rather than after them. A run that hits
        // the spend ceiling mid-group still re-parsed nothing it should not
        // have, and a graph rebuilt over a superset of what changed is correct
        // — where one rebuilt over a subset silently keeps stale edges.
        report.changed.extend(
            work.iter()
                .filter(|file| file.changed())
                .map(|file| file.path.clone()),
        );

        // Step 2: say what is about to be written, before writing it.
        let intents: Vec<IndexedFile> = work.iter().map(FileWork::intent).collect();
        self.manifest.record(repo_id, signature, &intents).await?;

        // Step 3: embed and upsert, in batches, across files.
        let relocations: Vec<(String, Chunk)> = work
            .iter()
            .flat_map(|file| file.to_relocate.iter().cloned())
            .collect();
        let relocated = self
            .index
            .relocate(signature, repo_id, &relocations)
            .await?;
        if relocated != relocations.len() as u64 {
            return Err(Error::Forge(
                "a confirmed chunk disappeared before its vector could be reused".into(),
            ));
        }

        let queue: Vec<(usize, &Chunk)> = work
            .iter()
            .enumerate()
            .flat_map(|(index, file)| file.to_embed.iter().map(move |chunk| (index, chunk)))
            .collect();

        // Where each file's last queued chunk sits, so "did this file finish?"
        // is a comparison rather than a search.
        let mut last_position = vec![None::<usize>; work.len()];
        for (position, (file, _)) in queue.iter().enumerate() {
            last_position[*file] = Some(position);
        }

        // Rows go into the store as they are written, but into the *count*
        // only when their file is confirmed below. An unconfirmed file is
        // re-embedded by the next run, and `upsert` reports a replacement as
        // a write, so counting here would count a chunk once per attempt: a
        // run cut off by the budget, or one that failed after its first
        // batch, would leave the total inflated for good.
        let mut written_per_file = vec![0_u64; work.len()];
        let mut written = 0_usize;
        for (start, end) in batch_bounds(&queue, self.batch, self.max_batch_tokens) {
            let batch = &queue[start..end];
            let texts: Vec<String> = batch.iter().map(|(_, chunk)| chunk.text.clone()).collect();
            let usage = EmbedUsage::of_call(&signature.key(), &texts);

            // Checked *before* spending, so the ceiling is a ceiling rather
            // than a line the run notices it has already crossed.
            if let Some(budget) = self.budget_usd
                && report.usage.cost_usd + usage.cost_usd > budget
            {
                tracing::warn!(
                    repo = repo_id,
                    spent = report.usage.cost_usd,
                    budget,
                    "embedding budget reached; stopping with a partial index"
                );
                report.budget_exhausted = true;
                break;
            }

            let response = self.embedder.embed(&texts).await?;
            report.usage.add(EmbedUsage {
                calls: 1,
                tokens: response.usage.embed_tokens,
                cost_usd: response.usage.cost_usd,
            });

            let embedded: Vec<EmbeddedChunk> = batch
                .iter()
                .zip(response.vectors)
                .map(|((_, chunk), vector)| EmbeddedChunk {
                    chunk: (*chunk).clone(),
                    vector,
                })
                .collect();
            self.index.upsert(signature, &embedded).await?;
            for (file, _) in batch {
                written_per_file[*file] += 1;
            }
            written += batch.len();
        }

        // A file finished if every chunk it queued was written. A file that
        // queued nothing — unchanged content — finished trivially, which is
        // exactly the case that must cost nothing.
        let finished = |index: usize| match last_position[index] {
            Some(position) => position < written,
            None => true,
        };

        // Orphans first — rows an earlier intent may have written and never
        // confirmed, now superseded. Deleted before the confirmation is
        // recorded, because the confirmation does not carry them: a delete
        // that fails here leaves the intent on record, where they are still
        // listed, and the next run finds them again. Never counted, never
        // subtracted.
        let orphans: Vec<String> = work
            .iter()
            .enumerate()
            .filter(|(index, _)| finished(*index))
            .flat_map(|(_, file)| file.orphans.clone())
            .collect();
        if !orphans.is_empty() {
            self.index.delete_chunks(repo_id, &orphans).await?;
        }

        // Step 4: confirm only the files whose every chunk actually landed. A
        // file cut off by the budget keeps its old confirmed set, so the next
        // run embeds what this one did not.
        let complete: Vec<IndexedFile> = work
            .iter()
            .enumerate()
            .filter(|(index, _)| finished(*index))
            .map(|(_, file)| file.confirmation())
            .collect();
        let recorded = self.manifest.record(repo_id, signature, &complete).await;
        // Counted per file that is *on record* as confirmed, not per file this
        // run meant to confirm. A manifest that writes its rows one at a time
        // can fail part-way, and the files it did write are confirmed for
        // good: the retry sees them unchanged and never counts them. So on a
        // failure the manifest is asked which ones landed, those are counted,
        // and only then does the error go up to settle with them.
        let counted: Vec<usize> = match &recorded {
            Ok(()) => work
                .iter()
                .enumerate()
                .filter(|(index, _)| finished(*index))
                .map(|(index, _)| index)
                .collect(),
            Err(_) => {
                let paths: Vec<String> = complete.iter().map(|file| file.path.clone()).collect();
                let landed = match self.manifest.indexed(repo_id, signature, &paths).await {
                    Ok(landed) => landed,
                    Err(read_err) => {
                        // The same incident, twice. Which confirmations
                        // landed is now unknowable here, so the count is
                        // marked as such and the next run recounts from the
                        // manifest instead of trusting a delta.
                        report.recount = true;
                        return Err(read_err);
                    }
                };
                work.iter()
                    .enumerate()
                    .filter(|(index, file)| {
                        finished(*index)
                            && landed.iter().any(|on_record| {
                                on_record.path == file.path && on_record.chunks == file.ids
                            })
                    })
                    .map(|(index, _)| index)
                    .collect()
            }
        };
        report.upserted += counted
            .iter()
            .map(|&index| written_per_file[index] + work[index].to_relocate.len() as u64)
            .sum::<u64>();
        recorded?;

        // Step 5: and only now is anything deleted.
        let stale: Vec<String> = work
            .iter()
            .enumerate()
            .filter(|(index, _)| finished(*index))
            .flat_map(|(_, file)| file.stale.clone())
            .collect();
        if !stale.is_empty() {
            report.deleted += self.index.delete_chunks(repo_id, &stale).await?;
        }
        // Delete succeeded, so stale IDs no longer need crash-recovery
        // protection in the manifest.
        let finalized: Vec<IndexedFile> = work
            .iter()
            .enumerate()
            .filter(|(index, _)| finished(*index))
            .map(|(_, file)| file.finalized())
            .collect();
        self.manifest.record(repo_id, signature, &finalized).await?;
        Ok(())
    }
}

/// Directory prefixes, one per directory, from however they were spelled.
fn dirs_of(dirs: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = dirs
        .into_iter()
        .map(|dir| format!("{}/", dir.trim_matches('/')))
        .filter(|dir| dir != "/")
        .collect();
    out.sort_unstable();
    out.dedup();
    out
}

/// Which paths a run removes.
///
/// A changed-path run names them; a full walk asks the manifest, and asks
/// it under the claim rather than before, so nothing another worker confirms
/// in between is missed.
enum Removed {
    These(Vec<String>),
    NotSeenByTheWalk,
}

/// Split `queue` into `[start, end)` batches that respect both ceilings.
///
/// Two ceilings rather than one because they bound different things.
/// `max_items` bounds how much work a single failure loses and keeps a
/// response small enough to hold in memory; `max_tokens` is the one the
/// provider enforces, and the one whose absence produced
/// `max_tokens_per_request` on every large repository.
///
/// A chunk that exceeds `max_tokens` on its own gets a batch to itself rather
/// than being skipped or wedged. That call will very likely be rejected — but
/// the chunker caps a chunk far below any sane ceiling, so reaching this means
/// the configuration is wrong, and a provider error naming the limit is a much
/// better outcome than a silently missing file or a loop that never advances.
/// The invariant that matters is that every chunk lands in exactly one batch.
fn batch_bounds(
    queue: &[(usize, &Chunk)],
    max_items: usize,
    max_tokens: u64,
) -> Vec<(usize, usize)> {
    let max_items = max_items.max(1);
    let mut bounds = Vec::new();
    let mut start = 0_usize;
    let mut tokens = 0_u64;

    for (position, (_, chunk)) in queue.iter().enumerate() {
        let cost = estimate_tokens(&chunk.text);
        let full = position - start >= max_items;
        // `position > start` keeps an oversized lone chunk from closing an
        // empty batch, which would emit `(start, start)` forever.
        let over = position > start && tokens + cost > max_tokens;
        if full || over {
            bounds.push((start, position));
            start = position;
            tokens = 0;
        }
        tokens += cost;
    }

    if start < queue.len() {
        bounds.push((start, queue.len()));
    }
    bounds
}

/// One file's plan for this run.
struct FileWork {
    path: String,
    ids: Vec<String>,
    to_embed: Vec<Chunk>,
    to_relocate: Vec<(String, Chunk)>,
    stale: Vec<String>,
    /// Ids an earlier run *intended* and never confirmed, now superseded:
    /// rows that may or may not exist, and were never counted. Deleted with
    /// the stale ones, but never subtracted.
    orphans: Vec<String>,
    previous: Vec<String>,
    reused: u64,
}

impl FileWork {
    fn new(path: String, chunks: Vec<Chunk>, previous: Option<&IndexedFile>) -> Self {
        // Every counted row on record: the confirmed set, and a
        // confirmation's pending set (old rows of a replacement whose delete
        // never ran — in the index, in the count). An intent's pending set is
        // not here: those rows were never counted.
        let confirmed: BTreeSet<String> = previous
            .map(|file| {
                let mut ids: BTreeSet<String> = file.chunks.iter().cloned().collect();
                if file.pending_is_stale {
                    ids.extend(file.pending.iter().cloned());
                }
                ids
            })
            .unwrap_or_default();
        let by_hash: std::collections::BTreeMap<String, String> = confirmed
            .iter()
            .filter_map(|id| {
                id.rsplit('\u{1f}')
                    .next()
                    .map(|hash| (hash.to_string(), id.clone()))
            })
            .collect();
        let ids: Vec<String> = chunks.iter().map(Chunk::id).collect();
        let fresh: BTreeSet<&String> = ids.iter().collect();

        // The heart of "unchanged content costs nothing": an id contains the
        // chunk's content hash, so an id already confirmed is content already
        // embedded, and it is skipped without a call.
        let mut to_embed = Vec::new();
        let mut to_relocate = Vec::new();
        for chunk in chunks {
            if confirmed.contains(&chunk.id()) {
                continue;
            }
            if let Some(id) = by_hash.get(&chunk.content_hash) {
                to_relocate.push((id.clone(), chunk));
            } else {
                to_embed.push(chunk);
            }
        }
        let reused = ids.len().saturating_sub(to_embed.len()) as u64;

        // Counted rows that this content supersedes: the confirmed set, and
        // a confirmation's pending set (the old rows of a replacement whose
        // delete never ran). An intent's pending set is the other thing —
        // rows that may have been written and were never counted — and goes
        // in `orphans`, to be deleted without being subtracted.
        let uncounted: Vec<String> = match previous {
            Some(file) if !file.pending_is_stale => file.pending.clone(),
            _ => Vec::new(),
        };
        let stale: Vec<String> = confirmed
            .iter()
            .filter(|id| !fresh.contains(id))
            .cloned()
            .collect();
        let orphans: Vec<String> = uncounted
            .into_iter()
            .filter(|id| !fresh.contains(id) && !stale.contains(id))
            .collect();

        Self {
            path,
            previous: confirmed.into_iter().collect(),
            ids,
            to_embed,
            to_relocate,
            stale,
            orphans,
            reused,
        }
    }

    /// Whether this file's content differs from what the manifest confirmed.
    ///
    /// A set comparison, not a length one: a file can gain and lose a chunk in
    /// the same edit and keep its count.
    fn changed(&self) -> bool {
        let fresh: BTreeSet<&String> = self.ids.iter().collect();
        let confirmed: BTreeSet<&String> = self.previous.iter().collect();
        fresh != confirmed
    }

    /// The pending record: the old confirmed set, plus what is about to land.
    fn intent(&self) -> IndexedFile {
        let mut pending = self.ids.clone();
        pending.extend(self.stale.iter().cloned());
        pending.extend(self.orphans.iter().cloned());
        pending.sort();
        pending.dedup();
        IndexedFile {
            path: self.path.clone(),
            chunks: self.previous.clone(),
            pending,
            pending_is_stale: false,
        }
    }

    /// The record after the writes landed: the new ids confirmed, the old
    /// counted ids pending their delete. Orphans are not carried — they were
    /// deleted before this record was written, and carrying them as stale
    /// would have a later removal subtract rows it never added.
    fn confirmation(&self) -> IndexedFile {
        IndexedFile {
            path: self.path.clone(),
            chunks: self.ids.clone(),
            pending: self.stale.clone(),
            pending_is_stale: true,
        }
    }

    fn finalized(&self) -> IndexedFile {
        IndexedFile::confirmed(self.path.clone(), self.ids.clone())
    }
}

#[cfg(test)]
#[path = "run_test.rs"]
mod tests;
