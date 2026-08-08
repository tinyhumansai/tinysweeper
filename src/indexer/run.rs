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
use std::path::Path;

use crate::chunk::types::{SkipReason, SkippedFile};
use crate::chunk::{Chunker, Selector};
use crate::error::Result;
use crate::index::types::{Chunk, EmbedSignature, EmbeddedChunk};
use crate::indexer::cost::EmbedUsage;
use crate::indexer::types::{
    Claim, IndexLease, IndexOutcome, IndexReport, IndexedFile, Settled,
};
use crate::ports::embed::Embedder;
use crate::ports::index::ChunkIndex;
use crate::ports::manifest::IndexManifest;

/// How many texts go in one embedding call by default.
///
/// Large enough that per-call latency stops dominating a full index, small
/// enough that a provider's per-request size limit is not the thing that
/// discovers the number for us.
pub const DEFAULT_BATCH: usize = 64;

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

    /// Set the embedding batch size.
    pub fn with_batch(mut self, batch: usize) -> Self {
        self.batch = batch.max(1);
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
            .manifest_paths(repo_id, &signature)
            .await?
            .into_iter()
            .filter(|path| !seen.contains(path))
            .collect();

        self.guarded(repo_id, revision, root, selection.selected, selection.skipped, removed)
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
        let mut sized = Vec::new();
        let mut removed = Vec::new();
        for path in paths {
            match std::fs::metadata(root.join(path)) {
                Ok(metadata) if metadata.is_file() => sized.push((path.clone(), metadata.len())),
                _ => removed.push(path.clone()),
            }
        }

        let selection = self.selector.select(sized);
        // A file that is still on disk but no longer selected — it grew past
        // the cap, or an ignore glob now covers it — must lose its chunks too,
        // or retrieval keeps quoting a file we have stopped tracking.
        removed.extend(selection.skipped.iter().map(|s| s.path.clone()));

        self.guarded(repo_id, revision, root, selection.selected, selection.skipped, removed)
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

    async fn manifest_paths(
        &self,
        repo_id: &str,
        signature: &EmbedSignature,
    ) -> Result<Vec<String>> {
        // The port answers per path rather than per repository on purpose — a
        // repository-wide listing is a query no incremental push needs. The
        // full-index path is the one caller that does, and it reconstructs it
        // from what the walk saw plus what the store still holds.
        self.manifest
            .indexed(repo_id, signature, &[])
            .await
            .map(|files| files.into_iter().map(|file| file.path).collect())
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

        // Step 2: say what is about to be written, before writing it.
        let intents: Vec<IndexedFile> = work.iter().map(FileWork::intent).collect();
        self.manifest.record(repo_id, signature, &intents).await?;

        // Step 3: embed and upsert, in batches, across files.
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
        for batch in queue.chunks(self.batch) {
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

            let vectors = self.embedder.embed(&texts).await?;
            report.usage.add(usage);

            let embedded: Vec<EmbeddedChunk> = batch
                .iter()
                .zip(vectors)
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
        Ok(())
    }
}

/// One file's plan for this run.
struct FileWork {
    path: String,
    ids: Vec<String>,
    to_embed: Vec<Chunk>,
    stale: Vec<String>,
    previous: Vec<String>,
    reused: u64,
}

impl FileWork {
    fn new(path: String, chunks: Vec<Chunk>, previous: Option<&IndexedFile>) -> Self {
        let confirmed: BTreeSet<String> = previous
            .map(|file| file.chunks.iter().cloned().collect())
            .unwrap_or_default();
        let ids: Vec<String> = chunks.iter().map(Chunk::id).collect();
        let fresh: BTreeSet<&String> = ids.iter().collect();

        // The heart of "unchanged content costs nothing": an id contains the
        // chunk's content hash, so an id already confirmed is content already
        // embedded, and it is skipped without a call.
        let to_embed: Vec<Chunk> = chunks
            .into_iter()
            .filter(|chunk| !confirmed.contains(&chunk.id()))
            .collect();
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
            stale,
            reused,
        }
    }

    /// The pending record: the old confirmed set, plus what is about to land.
    fn intent(&self) -> IndexedFile {
        IndexedFile {
            path: self.path.clone(),
            chunks: self.previous.clone(),
            pending: self.ids.clone(),
        }
    }

    fn confirmation(&self) -> IndexedFile {
        IndexedFile::confirmed(self.path.clone(), self.ids.clone())
    }

    /// Whether every chunk this file needed embedding for is among the first
    /// `written` entries of the run's queue.
    fn finished_by(&self, written: usize, queue: &[(usize, &Chunk)]) -> bool {
        queue
            .iter()
            .skip(written)
            .all(|(_, chunk)| !self.to_embed.iter().any(|mine| mine.id() == chunk.id()))
    }
}

#[cfg(test)]
#[path = "run_test.rs"]
mod tests;
