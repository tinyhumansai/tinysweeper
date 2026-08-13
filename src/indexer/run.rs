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
        })
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
        // been deleted or newly ignored, and its chunks have to go.
        let seen: BTreeSet<&String> = selection.selected.iter().collect();
        let removed: Vec<String> = self
            .manifest
            .paths(repo_id, &signature)
            .await?
            .into_iter()
            .filter(|path| !seen.contains(path))
            .collect();

        self.guarded(
            repo_id,
            revision,
            root,
            selection.selected,
            selection.skipped,
            removed,
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
            removed,
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
        removed: Vec<String>,
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

        let outcome = self
            .run(repo_id, &signature, root, selected, skipped, removed)
            .await;

        // Released on both paths. A claim only released on success is a claim a
        // crashed run holds until its TTL expires, and every push in between is
        // requeued for nothing.
        match outcome {
            Ok(report) => {
                self.settle(&lease, &signature, repo_id, revision, &report)
                    .await?;
                Ok(IndexOutcome::Indexed(report))
            }
            Err(err) => {
                let settled = Settled::Failed {
                    message: err.to_string(),
                };
                // A release failure must not mask the error that caused it.
                if let Err(nested) = self.manifest.release(&lease, &settled).await {
                    tracing::warn!(error = %nested, "could not release the index claim");
                }
                Err(err)
            }
        }
    }

    async fn settle(
        &self,
        lease: &IndexLease,
        signature: &EmbedSignature,
        repo_id: &str,
        revision: &str,
        report: &IndexReport,
    ) -> Result<()> {
        let chunks = self
            .manifest
            .state(repo_id, signature)
            .await?
            .chunks
            .saturating_add(report.upserted)
            .saturating_sub(report.deleted);
        self.manifest
            .release(
                lease,
                &Settled::Done {
                    // A partial run must not claim the revision: saying so would
                    // make the next push skip the work that was never finished.
                    revision: (!report.budget_exhausted).then(|| revision.to_string()),
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
        skipped: Vec<SkippedFile>,
        removed: Vec<String>,
    ) -> Result<IndexReport> {
        let mut report = IndexReport {
            skipped,
            ..IndexReport::default()
        };

        for group in selected.chunks(self.group) {
            if report.budget_exhausted {
                break;
            }
            self.index_group(repo_id, signature, root, group, &mut report)
                .await?;
        }

        if !removed.is_empty() {
            report.deleted += self.index.delete_paths(repo_id, &removed).await?;
            self.manifest.forget(repo_id, signature, &removed).await?;
            report.removed = removed;
        }

        Ok(report)
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
        report.upserted += relocated;

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
            report.upserted += self.index.upsert(signature, &embedded).await?;
            written += batch.len();
        }

        // A file finished if every chunk it queued was written. A file that
        // queued nothing — unchanged content — finished trivially, which is
        // exactly the case that must cost nothing.
        let finished = |index: usize| match last_position[index] {
            Some(position) => position < written,
            None => true,
        };

        // Step 4: confirm only the files whose every chunk actually landed. A
        // file cut off by the budget keeps its old confirmed set, so the next
        // run embeds what this one did not.
        let complete: Vec<IndexedFile> = work
            .iter()
            .enumerate()
            .filter(|(index, _)| finished(*index))
            .map(|(_, file)| file.confirmation())
            .collect();
        self.manifest.record(repo_id, signature, &complete).await?;

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
    previous: Vec<String>,
    reused: u64,
}

impl FileWork {
    fn new(path: String, chunks: Vec<Chunk>, previous: Option<&IndexedFile>) -> Self {
        let confirmed: BTreeSet<String> = previous
            .map(|file| file.chunks.iter().cloned().collect())
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

        let stale: Vec<String> = previous
            .map(IndexedFile::every_id)
            .unwrap_or_default()
            .into_iter()
            .filter(|id| !fresh.contains(id))
            .collect();

        Self {
            path,
            previous: confirmed.into_iter().collect(),
            ids,
            to_embed,
            to_relocate,
            stale,
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
        pending.sort();
        pending.dedup();
        IndexedFile {
            path: self.path.clone(),
            chunks: self.previous.clone(),
            pending,
        }
    }

    fn confirmation(&self) -> IndexedFile {
        IndexedFile {
            path: self.path.clone(),
            chunks: self.ids.clone(),
            pending: self.stale.clone(),
        }
    }

    fn finalized(&self) -> IndexedFile {
        IndexedFile::confirmed(self.path.clone(), self.ids.clone())
    }
}

#[cfg(test)]
#[path = "run_test.rs"]
mod tests;
