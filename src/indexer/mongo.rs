//! The MongoDB index manifest. Requires `serve`.
//!
//! Gated on `serve` for the same reason as `index::mongo`: it needs nothing the
//! server does not already link, and it lives in the same database as
//! `server::store`.
//!
//! The claim is written exactly the way `server::store` writes a delivery claim
//! and a review lease — a single `insert_one` against a **unique index**, with
//! the duplicate-key error as the answer rather than an exception. That shape is
//! deliberate and worth keeping consistent: it is atomic without a transaction,
//! it needs no read-then-write, and it cannot be lost to a race between two
//! workers on different machines. A find-then-update would be all three of the
//! opposite things.
//!
//! Claims also expire on a TTL. An explicit release cannot run if the process
//! is killed, and a stranded claim on this collection means that repository can
//! never be re-indexed again — the same failure that once held four review
//! leases forever.

use bson::{Document, doc};
use mongodb::options::IndexOptions;
use mongodb::{Client, Collection, Database, IndexModel};

use async_trait::async_trait;

use crate::error::{Error, Result};
use crate::index::types::EmbedSignature;
use crate::indexer::cost::EmbedUsage;
use crate::indexer::types::{Claim, IndexLease, IndexState, IndexedFile, RepoIndex, Settled};
use crate::ports::manifest::IndexManifest;

/// How long an indexing claim survives without being released.
///
/// Longer than the review lease because a cold full index of a large repository
/// is thousands of embedding calls, and cutting one short would leave it
/// permanently half-done. Short enough that a crashed worker frees the
/// repository the same day.
pub const CLAIM_TTL: std::time::Duration = std::time::Duration::from_secs(2 * 60 * 60);

/// The manifest, over two collections.
#[derive(Clone, Debug)]
pub struct MongoManifest {
    // Claims are separate from records because only they carry the unique index
    // and the TTL. Putting the claim on the record document would mean the TTL
    // expiring the record — the repository's whole index history — rather than
    // just the claim.
    claims: Collection<Document>,
    records: Collection<Document>,
    files: Collection<Document>,
}

impl MongoManifest {
    /// Open the manifest against an existing database handle.
    pub fn new(database: &Database) -> Self {
        Self {
            claims: database.collection("index_claims"),
            records: database.collection("index_records"),
            files: database.collection("index_files"),
        }
    }

    /// Connect to `uri` and use database `name`.
    pub async fn connect(uri: &str, name: &str) -> Result<Self> {
        let client = Client::with_uri_str(uri)
            .await
            .map_err(|err| Error::Forge(format!("could not reach MongoDB: {err}")))?;
        let manifest = Self::new(&client.database(name));
        manifest.prepare().await?;
        Ok(manifest)
    }

    /// Create the indexes the manifest depends on.
    ///
    /// The unique index is not an optimisation: without it two workers both
    /// insert a claim and both believe they own the repository.
    pub async fn prepare(&self) -> Result<()> {
        self.claims
            .create_index(
                IndexModel::builder()
                    .keys(doc! { "key": 1 })
                    .options(IndexOptions::builder().unique(true).build())
                    .build(),
            )
            .await
            .map_err(mongo)?;

        self.claims
            .create_index(
                IndexModel::builder()
                    .keys(doc! { "taken": 1 })
                    .options(IndexOptions::builder().expire_after(CLAIM_TTL).build())
                    .build(),
            )
            .await
            .map_err(mongo)?;

        // Every manifest read is "this repository, this signature, these
        // paths"; without this index that is a collection scan on the hottest
        // path in the module.
        self.files
            .create_index(
                IndexModel::builder()
                    .keys(doc! { "repo_id": 1, "signature": 1, "path": 1 })
                    .options(IndexOptions::builder().unique(true).build())
                    .build(),
            )
            .await
            .map_err(mongo)?;
        Ok(())
    }
}

/// The `_id` of a repository's record, and the claim key: one per signature.
fn record_id(repo_id: &str, signature: &str) -> String {
    format!("{repo_id}\u{1f}{signature}")
}

fn mongo(err: mongodb::error::Error) -> Error {
    Error::Forge(err.to_string())
}

/// Whether an error is a unique-index violation.
///
/// The *expected* outcome of a contended claim, which is why it is matched
/// rather than propagated. Kept identical to `server::store`'s copy: both are
/// three lines, and sharing them across a feature boundary would couple the
/// server to the indexer for no gain.
fn is_duplicate_key(err: &mongodb::error::Error) -> bool {
    matches!(
        *err.kind,
        mongodb::error::ErrorKind::Write(mongodb::error::WriteFailure::WriteError(
            mongodb::error::WriteError { code: 11000, .. }
        ))
    ) || err.to_string().contains("E11000")
}

fn record_from_document(repo_id: &str, signature: &str, document: Option<Document>) -> RepoIndex {
    let mut record = RepoIndex::absent(repo_id, signature);
    let Some(document) = document else {
        return record;
    };
    if let Ok(state) = document.get_str("state") {
        record.state = match state {
            "indexing" => IndexState::Indexing,
            "ready" => IndexState::Ready,
            "failed" => IndexState::Failed,
            _ => IndexState::Absent,
        };
    }
    record.revision = document.get_str("revision").ok().map(str::to_string);
    record.message = document.get_str("message").ok().map(str::to_string);
    record.chunks = document.get_i64("chunks").unwrap_or_default().max(0) as u64;
    record.usage = EmbedUsage {
        calls: document.get_i64("calls").unwrap_or_default().max(0) as u64,
        tokens: document.get_i64("tokens").unwrap_or_default().max(0) as u64,
        cost_usd: document.get_f64("cost_usd").unwrap_or_default(),
    };
    record
}

#[async_trait]
impl IndexManifest for MongoManifest {
    async fn claim(
        &self,
        repo_id: &str,
        signature: &EmbedSignature,
        holder: &str,
    ) -> Result<Claim> {
        let key = record_id(repo_id, &signature.key());
        match self
            .claims
            .insert_one(doc! {
                "key": &key,
                "holder": holder,
                "taken": bson::DateTime::now(),
            })
            .await
        {
            Ok(_) => {}
            Err(err) if is_duplicate_key(&err) => {
                let held = self
                    .claims
                    .find_one(doc! { "key": &key })
                    .await
                    .map_err(mongo)?;
                return Ok(Claim::Busy {
                    holder: held
                        .as_ref()
                        .and_then(|d| d.get_str("holder").ok())
                        .map(str::to_string),
                });
            }
            Err(err) => return Err(mongo(err)),
        }

        // The claim is what makes this exclusive, so the state write can follow
        // it rather than race it.
        self.records
            .update_one(
                doc! { "_id": &key },
                doc! { "$set": {
                    "repo_id": repo_id,
                    "signature": signature.key(),
                    "state": "indexing",
                } },
            )
            .upsert(true)
            .await
            .map_err(mongo)?;

        Ok(Claim::Granted(IndexLease {
            repo_id: repo_id.to_string(),
            signature: signature.key(),
            holder: holder.to_string(),
        }))
    }

    async fn release(&self, lease: &IndexLease, settled: &Settled) -> Result<()> {
        let key = record_id(&lease.repo_id, &lease.signature);
        let update = match settled {
            Settled::Done {
                revision,
                chunks,
                usage,
            } => doc! {
                "$set": {
                    "state": "ready",
                    "revision": revision.clone(),
                    "chunks": *chunks as i64,
                    "message": bson::Bson::Null,
                },
                // Spend accumulates across runs: `$inc`, so a re-index adds to
                // the repository's bill rather than replacing it.
                "$inc": {
                    "calls": usage.calls as i64,
                    "tokens": usage.tokens as i64,
                    "cost_usd": usage.cost_usd,
                },
            },
            Settled::Failed { message } => doc! {
                "$set": { "state": "failed", "message": message.clone() },
            },
        };

        self.records
            .update_one(doc! { "_id": &key }, update)
            .upsert(true)
            .await
            .map_err(mongo)?;

        // Dropped last. Releasing the claim before the state is written would
        // let the next worker read `indexing` and conclude the repository is
        // busy when nobody holds it.
        self.claims
            .delete_one(doc! { "key": &key })
            .await
            .map_err(mongo)?;
        Ok(())
    }

    async fn state(&self, repo_id: &str, signature: &EmbedSignature) -> Result<RepoIndex> {
        let key = record_id(repo_id, &signature.key());
        let document = self
            .records
            .find_one(doc! { "_id": key })
            .await
            .map_err(mongo)?;
        Ok(record_from_document(repo_id, &signature.key(), document))
    }

    async fn indexed(
        &self,
        repo_id: &str,
        signature: &EmbedSignature,
        paths: &[String],
    ) -> Result<Vec<IndexedFile>> {
        if paths.is_empty() {
            return Ok(Vec::new());
        }
        let mut cursor = self
            .files
            .find(doc! {
                "repo_id": repo_id,
                "signature": signature.key(),
                "path": { "$in": paths },
            })
            .await
            .map_err(mongo)?;

        let mut files = Vec::new();
        while cursor.advance().await.map_err(mongo)? {
            let document = cursor.deserialize_current().map_err(mongo)?;
            files.push(IndexedFile {
                path: document.get_str("path").unwrap_or_default().to_string(),
                chunks: strings(&document, "chunks"),
                pending: strings(&document, "pending"),
            });
        }
        Ok(files)
    }

    async fn paths(&self, repo_id: &str, signature: &EmbedSignature) -> Result<Vec<String>> {
        let mut cursor = self
            .files
            .find(doc! { "repo_id": repo_id, "signature": signature.key() })
            .projection(doc! { "path": 1 })
            .await
            .map_err(mongo)?;

        let mut paths = Vec::new();
        while cursor.advance().await.map_err(mongo)? {
            let document = cursor.deserialize_current().map_err(mongo)?;
            if let Ok(path) = document.get_str("path") {
                paths.push(path.to_string());
            }
        }
        Ok(paths)
    }

    async fn record(
        &self,
        repo_id: &str,
        signature: &EmbedSignature,
        files: &[IndexedFile],
    ) -> Result<()> {
        for file in files {
            self.files
                .update_one(
                    doc! {
                        "repo_id": repo_id,
                        "signature": signature.key(),
                        "path": &file.path,
                    },
                    doc! { "$set": {
                        "chunks": file.chunks.clone(),
                        "pending": file.pending.clone(),
                    } },
                )
                .upsert(true)
                .await
                .map_err(mongo)?;
        }
        Ok(())
    }

    async fn forget(
        &self,
        repo_id: &str,
        signature: &EmbedSignature,
        paths: &[String],
    ) -> Result<()> {
        if paths.is_empty() {
            return Ok(());
        }
        self.files
            .delete_many(doc! {
                "repo_id": repo_id,
                "signature": signature.key(),
                "path": { "$in": paths },
            })
            .await
            .map_err(mongo)?;
        Ok(())
    }
}

fn strings(document: &Document, field: &str) -> Vec<String> {
    document
        .get_array(field)
        .map(|values| {
            values
                .iter()
                .filter_map(|value| value.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
#[path = "mongo_test.rs"]
mod tests;
