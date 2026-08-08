//! The MongoDB adapter for the four retrieval ports. Requires `serve`.
//!
//! Gated on `serve` rather than a feature of its own because it needs nothing
//! the server does not already link: `mongodb` and `bson` are declared there,
//! and the retrieval stores live in the same database as `server::store`. A new
//! feature flag would have bought a second way to build a broken deployment.
//!
//! # Why MongoDB, and what it has to be
//!
//! MongoDB **Community 8.2+** serves `$vectorSearch`, `$search` and
//! `$rankFusion` natively, through the separate `mongot` process. That is new —
//! it used to be Atlas-only — and it is the whole reason this is one database
//! rather than Mongo plus a vector store plus an inverted index.
//!
//! Two consequences are load-bearing:
//!
//! - **`$rankFusion` does the fusion.** Weighted reciprocal rank fusion over
//!   sub-pipelines is a server stage. Hand-rolling RRF in Rust would mean
//!   over-fetching both arms, and hand-rolling a TF vector for the lexical arm
//!   would throw away real BM25 with corpus IDF — the part that makes a rare
//!   identifier outrank a common word.
//! - **A stock `mongo:` image cannot do any of it.** It has no `mongot`, and an
//!   unsupported aggregation stage fails when the query runs, which is to say
//!   on a contributor's pull request. [`MongoIndex::prepare`] therefore probes
//!   for real and refuses to start. See [`VECTOR_SEARCH_UNAVAILABLE`].
//!
//! # Storage
//!
//! Vectors are BSON `binData` of subtype `vector`, packed float32: four bytes
//! per dimension against eight plus per-element type overhead for an array of
//! doubles. At 1024 dimensions that is the difference between a ~4 KiB and a
//! ~9 KiB document, on the field that dominates the collection.

use std::collections::{BTreeMap, BTreeSet};

use async_trait::async_trait;
use bson::binary::Vector;
use bson::{Binary, Bson, Document, doc};
use futures::StreamExt;
use mongodb::options::IndexOptions;
use mongodb::{Client, Collection, Database, IndexModel, SearchIndexModel, SearchIndexType};
use sha2::{Digest, Sha256};

use crate::error::{Error, Result};
use crate::index::types::{
    Chunk, ChunkMethod, EdgeKind, EmbedSignature, EmbeddedChunk, GraphEdge, GraphNode, HybridQuery,
    KnowledgeDoc, KnowledgeScope, Neighbourhood, NodeKind, ScoredChunk,
};
use crate::ports::graph::GraphStore;
use crate::ports::index::ChunkIndex;
use crate::ports::knowledge::KnowledgeStore;

/// What the operator is told when hybrid search is not actually available.
///
/// Spelled out rather than left as a driver error because the driver's message
/// — "Unrecognized pipeline stage name: '$vectorSearch'" — reads like a bug in
/// tinysweeper, and the fix is entirely in the deployment.
pub const VECTOR_SEARCH_UNAVAILABLE: &str = concat!(
    "this MongoDB deployment cannot serve $vectorSearch/$search. ",
    "tinysweeper needs MongoDB Community (or Enterprise) 8.2 or newer with the ",
    "`mongot` search process attached and the server running as a replica set. ",
    "The stock `mongo:` Docker image ships no mongot and will fail here. ",
    "See docker-compose.yml for a working pair of images."
);

/// How many upserts are in flight at once.
///
/// Indexing a repository is thousands of documents and the driver pipelines
/// them over one connection pool; unbounded concurrency here just converts the
/// pool into a queue with worse error messages.
const UPSERT_CONCURRENCY: usize = 32;

/// How long [`MongoIndex::prepare`] waits for a freshly created search index to
/// become queryable before giving up.
///
/// mongot builds indexes asynchronously, so a boot straight after a first
/// deploy legitimately has to wait. Bounded, because waiting forever is how a
/// broken deployment presents as a hung one.
const INDEX_READY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

/// Name of the lexical (BM25) search index. One per collection: unlike the
/// vector index it does not depend on the embedding model.
const TEXT_INDEX: &str = "tinysweeper_text";

/// The vector the boot probe searches with.
///
/// A unit vector, not a zero one. Cosine similarity against the zero vector is
/// undefined and the server rejects the whole aggregation for it, so a zero
/// probe would fail against a perfectly good deployment — a boot assertion that
/// cries wolf is worse than no boot assertion.
fn probe_vector(dims: usize) -> Vec<f32> {
    vec![1.0 / (dims.max(1) as f32).sqrt(); dims]
}

/// Encode a vector for storage or for a query.
///
/// float32 `binData`, not an array of doubles — see the module docs.
fn encode_vector(values: &[f32]) -> Binary {
    Binary::from(Vector::Float32(values.to_vec()))
}

/// The vector index name for a signature.
///
/// Derived from a hash because the index carries `numDimensions` in its
/// definition and cannot be shared across signatures, while index names have a
/// restricted character set that a raw model id does not respect.
fn vector_index_name(signature: &EmbedSignature) -> String {
    // Hand-rolled hex for the same reason as `Finding::fingerprint`: sha2 0.11
    // returns an `Array` with no `LowerHex`, and a hex crate is not worth a
    // dependency for sixteen characters.
    let hex: String = Sha256::digest(signature.key().as_bytes())
        .iter()
        .take(8)
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("tinysweeper_vec_{hex}")
}

/// Apply a batch of upserts, bounded in flight.
///
/// Takes owned `(filter, update)` pairs rather than borrowing the caller's
/// slice: inside an `async_trait` future the borrow cannot be proven to outlive
/// the boxed future, and materialising the documents first is both cheaper to
/// reason about and no more allocation than the driver does anyway.
async fn upsert_all(
    collection: &Collection<Document>,
    writes: Vec<(Document, Document)>,
) -> Result<u64> {
    let total = writes.len() as u64;
    let results = futures::stream::iter(writes.into_iter().map(|(filter, update)| {
        let collection = collection.clone();
        async move {
            collection
                .update_one(filter, doc! { "$set": update })
                .upsert(true)
                .await
                .map_err(mongo)
        }
    }))
    .buffer_unordered(UPSERT_CONCURRENCY)
    .collect::<Vec<_>>()
    .await;
    for result in results {
        result?;
    }
    Ok(total)
}

/// Whether a driver error is "this server does not know that stage".
///
/// Matched on rather than propagated because it is the one failure that means
/// *the deployment is wrong*, and it deserves [`VECTOR_SEARCH_UNAVAILABLE`]
/// instead of a stack of Mongo error codes. Matched on the message rather than
/// the code because the server reports it as a plain `CommandNotSupported` /
/// unrecognised-stage parse failure, indistinguishable by code from a genuine
/// pipeline mistake.
pub(crate) fn is_unsupported_stage(message: &str) -> bool {
    message.contains("Unrecognized pipeline stage")
        || message.contains("Atlas")
        || message.contains("mongot")
        || message.contains("not supported")
        || message.contains("Unsupported")
}

fn unavailable(err: &mongodb::error::Error) -> Error {
    Error::Forge(format!("{VECTOR_SEARCH_UNAVAILABLE} (server said: {err})"))
}

fn mongo(err: mongodb::error::Error) -> Error {
    Error::Forge(err.to_string())
}

/// Every retrieval store, opened against one database.
///
/// Held together because they share a connection and are prepared as a unit:
/// starting with a working chunk index and a graph store that never got its
/// indexes is a deployment that degrades under load rather than failing.
#[derive(Debug, Clone)]
pub struct MongoIndex {
    /// Embedded chunks of repository source.
    pub code: MongoChunkIndex,
    /// Embedded chunks of knowledge documents.
    pub knowledge_chunks: MongoChunkIndex,
    /// Embedded chunks of past review conversations.
    pub reviews: MongoChunkIndex,
    /// The code graph.
    pub graph: MongoGraphStore,
    /// Curated org- and repo-scoped documents.
    pub knowledge: MongoKnowledgeStore,
}

impl MongoIndex {
    /// Open every store against `database`.
    pub fn new(database: &Database) -> Self {
        Self {
            code: MongoChunkIndex::new(database, "code_chunks"),
            knowledge_chunks: MongoChunkIndex::new(database, "knowledge_chunks"),
            reviews: MongoChunkIndex::new(database, "review_chunks"),
            graph: MongoGraphStore::new(database),
            knowledge: MongoKnowledgeStore::new(database),
        }
    }

    /// Connect to `uri` and open every store in database `name`.
    pub async fn connect(uri: &str, name: &str) -> Result<Self> {
        let client = Client::with_uri_str(uri)
            .await
            .map_err(|err| Error::Forge(format!("could not reach MongoDB: {err}")))?;
        Ok(Self::new(&client.database(name)))
    }

    /// Connect using the same environment the server's store uses.
    pub async fn from_env() -> Result<Self> {
        let uri = std::env::var("TINYSWEEPER_MONGODB_URI")
            .map_err(|_| Error::Forge("TINYSWEEPER_MONGODB_URI is not set".into()))?;
        let name =
            std::env::var("TINYSWEEPER_MONGODB_DB").unwrap_or_else(|_| "tinysweeper".to_string());
        Self::connect(&uri, &name).await
    }

    /// The embedding signature the server was configured with, if any.
    ///
    /// Retrieval is opt-in: a deployment with no embedding provider runs the
    /// lanes exactly as before. But a *partly* configured one — a provider with
    /// no dimension count — is a mistake rather than a choice, so it is an
    /// error instead of a silent fall back to disabled.
    pub fn signature_from_env() -> Result<Option<EmbedSignature>> {
        let provider = std::env::var("TINYSWEEPER_EMBED_PROVIDER").ok();
        let model = std::env::var("TINYSWEEPER_EMBED_MODEL").ok();
        let dims = std::env::var("TINYSWEEPER_EMBED_DIMS").ok();
        match (provider, model, dims) {
            (None, None, None) => Ok(None),
            (Some(provider), Some(model), Some(dims)) => {
                let dims = dims.parse::<usize>().map_err(|_| {
                    Error::config("TINYSWEEPER_EMBED_DIMS must be a positive integer")
                })?;
                if dims == 0 {
                    return Err(Error::config("TINYSWEEPER_EMBED_DIMS must not be zero"));
                }
                Ok(Some(EmbedSignature::new(provider, model, dims)))
            }
            _ => Err(Error::config(
                "retrieval needs all of TINYSWEEPER_EMBED_PROVIDER, TINYSWEEPER_EMBED_MODEL \
                 and TINYSWEEPER_EMBED_DIMS, or none of them",
            )),
        }
    }

    /// Create every index and **prove hybrid search works**, or fail.
    ///
    /// This is the boot assertion. It is deliberately not lazy and deliberately
    /// not tolerant: the failure it catches — a deployment with no `mongot` —
    /// is otherwise invisible until the first review query, at which point it
    /// surfaces as a failed check run on somebody's pull request.
    pub async fn prepare(&self, signature: &EmbedSignature) -> Result<()> {
        self.graph.prepare().await?;
        self.knowledge.prepare().await?;
        for index in [&self.code, &self.knowledge_chunks, &self.reviews] {
            index.prepare(signature).await?;
        }
        // One end-to-end probe, on the collection that actually gets queried.
        // Creating the indexes alone does not prove the stages run: mongot can
        // accept a definition and still be unreachable from mongod.
        self.code.probe_hybrid_search(signature).await
    }
}

/// A [`ChunkIndex`] over one MongoDB collection.
///
/// Parameterised by collection rather than hard-wired so `code_chunks`,
/// `knowledge_chunks` and `review_chunks` are one implementation. They differ
/// in what is written into them, not in how they are searched.
#[derive(Debug, Clone)]
pub struct MongoChunkIndex {
    collection: Collection<Document>,
}

impl MongoChunkIndex {
    /// Open the index over `name` in `database`.
    pub fn new(database: &Database, name: &str) -> Self {
        Self {
            collection: database.collection(name),
        }
    }

    /// The definition of the vector index for `signature`.
    fn vector_definition(signature: &EmbedSignature) -> Document {
        doc! {
            "fields": [
                {
                    "type": "vector",
                    "path": "vector",
                    "numDimensions": signature.dims as i64,
                    // Cosine, because every provider worth using returns
                    // normalised vectors and cosine is what their published
                    // benchmarks are measured with.
                    "similarity": "cosine",
                },
                // Both filters must be declared here or `$vectorSearch` refuses
                // to filter on them. `sig` is the partition key that makes an
                // embedding-model swap invalidate the index.
                { "type": "filter", "path": "sig" },
                { "type": "filter", "path": "repo_id" },
            ]
        }
    }

    /// The definition of the lexical index.
    ///
    /// `dynamic: false` on purpose: indexing every field would index the stored
    /// vector binary, which is large and meaningless to BM25.
    fn text_definition() -> Document {
        doc! {
            "mappings": {
                "dynamic": false,
                "fields": {
                    "text": { "type": "string" },
                    "symbol": { "type": "string" },
                    "path": { "type": "string" },
                    "sig": { "type": "token" },
                    "repo_id": { "type": "token" },
                }
            }
        }
    }

    async fn ensure_btree_indexes(&self) -> Result<()> {
        // The compound (repo_id, path) index is not an optimisation. Every
        // incremental re-index deletes the chunks of the files a push touched;
        // without a path in the index that delete is a full collection scan,
        // once per push, over every repository's chunks.
        self.collection
            .create_index(
                IndexModel::builder()
                    .keys(doc! { "repo_id": 1, "path": 1 })
                    .build(),
            )
            .await
            .map_err(|err| Error::Forge(format!("could not index chunks by path: {err}")))?;
        self.collection
            .create_index(IndexModel::builder().keys(doc! { "sig": 1 }).build())
            .await
            .map_err(|err| Error::Forge(format!("could not index chunks by signature: {err}")))?;
        Ok(())
    }

    /// Create the search indexes if they are absent, and wait until queryable.
    async fn ensure_search_indexes(&self, signature: &EmbedSignature) -> Result<()> {
        let vector_name = vector_index_name(signature);
        let existing = self.search_index_names().await?;

        if !existing.contains(&vector_name) {
            self.collection
                .create_search_index(
                    SearchIndexModel::builder()
                        .name(Some(vector_name.clone()))
                        .index_type(Some(SearchIndexType::VectorSearch))
                        .definition(Self::vector_definition(signature))
                        .build(),
                )
                .await
                .map_err(|err| unavailable(&err))?;
        }
        if !existing.contains(&TEXT_INDEX.to_string()) {
            self.collection
                .create_search_index(
                    SearchIndexModel::builder()
                        .name(Some(TEXT_INDEX.to_string()))
                        .index_type(Some(SearchIndexType::Search))
                        .definition(Self::text_definition())
                        .build(),
                )
                .await
                .map_err(|err| unavailable(&err))?;
        }

        self.await_queryable(&[vector_name, TEXT_INDEX.to_string()])
            .await
    }

    async fn search_index_names(&self) -> Result<Vec<String>> {
        // `$listSearchIndexes` is the cheapest thing that fails outright on a
        // deployment with no mongot, so it doubles as the first half of the
        // boot assertion.
        let mut cursor = self.collection.list_search_indexes().await.map_err(|err| {
            if is_unsupported_stage(&err.to_string()) {
                unavailable(&err)
            } else {
                mongo(err)
            }
        })?;
        let mut names = Vec::new();
        while let Some(next) = cursor.next().await {
            let document = next.map_err(mongo)?;
            if let Ok(name) = document.get_str("name") {
                names.push(name.to_string());
            }
        }
        Ok(names)
    }

    async fn await_queryable(&self, names: &[String]) -> Result<()> {
        let deadline = std::time::Instant::now() + INDEX_READY_TIMEOUT;
        loop {
            let mut cursor = self.collection.list_search_indexes().await.map_err(mongo)?;
            let mut ready = BTreeSet::new();
            while let Some(next) = cursor.next().await {
                let document = next.map_err(mongo)?;
                let queryable = document.get_bool("queryable").unwrap_or(false);
                if let (Ok(name), true) = (document.get_str("name"), queryable) {
                    ready.insert(name.to_string());
                }
            }
            if names.iter().all(|name| ready.contains(name)) {
                return Ok(());
            }
            if std::time::Instant::now() >= deadline {
                return Err(Error::Forge(format!(
                    "search indexes {names:?} were still not queryable after {}s; \
                     mongot may be unreachable from mongod. {VECTOR_SEARCH_UNAVAILABLE}",
                    INDEX_READY_TIMEOUT.as_secs()
                )));
            }
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        }
    }

    /// Run the real hybrid pipeline once, on a zero vector, and require it to
    /// execute.
    ///
    /// An empty result set is a pass — the collection may legitimately be
    /// empty. What is being asserted is that the *stages* run.
    pub async fn probe_hybrid_search(&self, signature: &EmbedSignature) -> Result<()> {
        let query = HybridQuery {
            signature: signature.clone(),
            repo_id: None,
            text: "tinysweeper boot probe".to_string(),
            vector: probe_vector(signature.dims),
            limit: 1,
            dense_weight: 1.0,
            text_weight: 1.0,
        };
        match self.collection.aggregate(self.pipeline(&query)).await {
            Ok(_) => Ok(()),
            Err(err) if is_unsupported_stage(&err.to_string()) => Err(unavailable(&err)),
            Err(err) => Err(mongo(err)),
        }
    }

    /// The `$rankFusion` pipeline for `query`.
    ///
    /// Split out so the boot probe runs *exactly* what a review will run. A
    /// probe that exercised a simpler pipeline would pass on a deployment where
    /// the real one fails.
    fn pipeline(&self, query: &HybridQuery) -> Vec<Document> {
        let signature = query.signature.key();

        let mut vector_filter = doc! { "sig": { "$eq": &signature } };
        let mut text_filter = vec![doc! { "equals": { "path": "sig", "value": &signature } }];
        if let Some(repo) = &query.repo_id {
            vector_filter.insert("repo_id", doc! { "$eq": repo });
            text_filter.push(doc! { "equals": { "path": "repo_id", "value": repo } });
        }

        // `numCandidates` is the approximate-nearest-neighbour beam. Ten times
        // the limit is the vendor's own guidance and the point where recall
        // stops improving noticeably.
        let candidates = (query.limit * 10).max(100) as i64;

        vec![
            doc! {
                "$rankFusion": {
                    "input": {
                        "pipelines": {
                            // Reciprocal rank fusion is done by the server. The
                            // two arms are ranked independently and combined by
                            // rank, not by score, which is what makes a BM25
                            // score and a cosine distance comparable at all.
                            "dense": [
                                {
                                    "$vectorSearch": {
                                        "index": vector_index_name(&query.signature),
                                        "path": "vector",
                                        "queryVector": encode_vector(&query.vector),
                                        "numCandidates": candidates,
                                        "limit": query.limit as i64,
                                        "filter": vector_filter,
                                    }
                                }
                            ],
                            "lexical": [
                                {
                                    "$search": {
                                        "index": TEXT_INDEX,
                                        "compound": {
                                            "must": [{
                                                "text": {
                                                    "query": &query.text,
                                                    "path": ["text", "symbol", "path"],
                                                }
                                            }],
                                            "filter": text_filter,
                                        }
                                    }
                                },
                                { "$limit": query.limit as i64 },
                            ],
                        }
                    },
                    "combination": {
                        "weights": {
                            "dense": query.dense_weight,
                            "lexical": query.text_weight,
                        }
                    },
                }
            },
            doc! { "$addFields": { "score": { "$meta": "score" } } },
            doc! { "$limit": query.limit as i64 },
        ]
    }
}

fn chunk_document(signature: &EmbedSignature, embedded: &EmbeddedChunk) -> Document {
    let chunk = &embedded.chunk;
    doc! {
        "sig": signature.key(),
        "repo_id": &chunk.repo_id,
        "path": &chunk.path,
        "start_line": chunk.start_line as i64,
        "end_line": chunk.end_line as i64,
        "text": &chunk.text,
        "lang": chunk.lang.as_deref(),
        "symbol": chunk.symbol.as_deref(),
        "content_hash": &chunk.content_hash,
        "chunked_by": chunk.chunked_by.as_str(),
        "vector": encode_vector(&embedded.vector),
    }
}

fn chunk_from_document(document: &Document) -> Chunk {
    Chunk {
        repo_id: document.get_str("repo_id").unwrap_or_default().to_string(),
        path: document.get_str("path").unwrap_or_default().to_string(),
        start_line: document.get_i64("start_line").unwrap_or_default() as u32,
        end_line: document.get_i64("end_line").unwrap_or_default() as u32,
        text: document.get_str("text").unwrap_or_default().to_string(),
        lang: document.get_str("lang").ok().map(str::to_string),
        symbol: document.get_str("symbol").ok().map(str::to_string),
        content_hash: document
            .get_str("content_hash")
            .unwrap_or_default()
            .to_string(),
        // A document written before the field existed reads back as the weaker
        // claim, which is the safe direction for a missing value to resolve.
        chunked_by: match document.get_str("chunked_by") {
            Ok("parsed") => ChunkMethod::Parsed,
            _ => ChunkMethod::Lines,
        },
    }
}

#[async_trait]
impl ChunkIndex for MongoChunkIndex {
    async fn prepare(&self, signature: &EmbedSignature) -> Result<()> {
        self.ensure_btree_indexes().await?;
        self.ensure_search_indexes(signature).await
    }

    async fn upsert(&self, signature: &EmbedSignature, chunks: &[EmbeddedChunk]) -> Result<u64> {
        for embedded in chunks {
            if embedded.vector.len() != signature.dims {
                return Err(Error::Config(format!(
                    "chunk {} has {} dimensions but {signature} declares {}",
                    embedded.chunk.id(),
                    embedded.vector.len(),
                    signature.dims
                )));
            }
        }
        let writes = chunks
            .iter()
            .map(|embedded| {
                (
                    doc! { "_id": embedded.chunk.id() },
                    chunk_document(signature, embedded),
                )
            })
            .collect();
        upsert_all(&self.collection, writes).await
    }

    async fn delete_repo(&self, repo_id: &str) -> Result<u64> {
        Ok(self
            .collection
            .delete_many(doc! { "repo_id": repo_id })
            .await
            .map_err(mongo)?
            .deleted_count)
    }

    async fn delete_paths(&self, repo_id: &str, paths: &[String]) -> Result<u64> {
        if paths.is_empty() {
            return Ok(0);
        }
        Ok(self
            .collection
            .delete_many(doc! { "repo_id": repo_id, "path": { "$in": paths } })
            .await
            .map_err(mongo)?
            .deleted_count)
    }

    async fn delete_chunks(&self, repo_id: &str, ids: &[String]) -> Result<u64> {
        if ids.is_empty() {
            return Ok(0);
        }
        // Filtered on `repo_id` as well as `_id` so a mistaken id cannot reach
        // across repositories, exactly as the mock does.
        Ok(self
            .collection
            .delete_many(doc! { "repo_id": repo_id, "_id": { "$in": ids } })
            .await
            .map_err(mongo)?
            .deleted_count)
    }

    async fn query(&self, query: &HybridQuery) -> Result<Vec<ScoredChunk>> {
        let mut cursor = match self.collection.aggregate(self.pipeline(query)).await {
            Ok(cursor) => cursor,
            Err(err) if is_unsupported_stage(&err.to_string()) => return Err(unavailable(&err)),
            Err(err) => return Err(mongo(err)),
        };
        let mut hits = Vec::new();
        while let Some(next) = cursor.next().await {
            let document = next.map_err(mongo)?;
            hits.push(ScoredChunk {
                score: document.get_f64("score").unwrap_or_default(),
                chunk: chunk_from_document(&document),
            });
        }
        Ok(hits)
    }
}

/// A [`GraphStore`] over two MongoDB collections.
#[derive(Debug, Clone)]
pub struct MongoGraphStore {
    nodes: Collection<Document>,
    edges: Collection<Document>,
}

impl MongoGraphStore {
    /// Open the graph in `database`.
    pub fn new(database: &Database) -> Self {
        Self {
            nodes: database.collection("graph_nodes"),
            edges: database.collection("graph_edges"),
        }
    }
}

fn node_document(node: &GraphNode) -> Document {
    doc! {
        "repo_id": &node.repo_id,
        "node_id": &node.id,
        "kind": bson::to_bson(&node.kind).unwrap_or(Bson::Null),
        "path": &node.path,
        "symbol": node.symbol.as_deref(),
        "lang": node.lang.as_deref(),
    }
}

fn node_from_document(document: &Document) -> GraphNode {
    GraphNode {
        id: document.get_str("node_id").unwrap_or_default().to_string(),
        repo_id: document.get_str("repo_id").unwrap_or_default().to_string(),
        kind: document
            .get("kind")
            .and_then(|kind| bson::from_bson(kind.clone()).ok())
            .unwrap_or(NodeKind::File),
        path: document.get_str("path").unwrap_or_default().to_string(),
        symbol: document.get_str("symbol").ok().map(str::to_string),
        lang: document.get_str("lang").ok().map(str::to_string),
    }
}

fn edge_document(edge: &GraphEdge) -> Document {
    doc! {
        "repo_id": &edge.repo_id,
        "from": &edge.from,
        "to": &edge.to,
        "kind": bson::to_bson(&edge.kind).unwrap_or(Bson::Null),
        "path": &edge.path,
    }
}

fn edge_from_document(document: &Document) -> Option<GraphEdge> {
    Some(GraphEdge {
        repo_id: document.get_str("repo_id").ok()?.to_string(),
        from: document.get_str("from").ok()?.to_string(),
        to: document.get_str("to").ok()?.to_string(),
        kind: bson::from_bson(document.get("kind")?.clone()).ok()?,
        path: document.get_str("path").unwrap_or_default().to_string(),
    })
}

#[async_trait]
impl GraphStore for MongoGraphStore {
    async fn prepare(&self) -> Result<()> {
        // Same reasoning as the chunk index: incremental re-index deletes by
        // path, and an unindexed path field turns every push into two
        // collection scans.
        for (collection, label) in [(&self.nodes, "nodes"), (&self.edges, "edges")] {
            collection
                .create_index(
                    IndexModel::builder()
                        .keys(doc! { "repo_id": 1, "path": 1 })
                        .build(),
                )
                .await
                .map_err(|err| Error::Forge(format!("could not index graph {label}: {err}")))?;
        }
        // Traversal looks edges up by endpoint, one hop at a time.
        for keys in [
            doc! { "repo_id": 1, "from": 1 },
            doc! { "repo_id": 1, "to": 1 },
        ] {
            self.edges
                .create_index(IndexModel::builder().keys(keys).build())
                .await
                .map_err(|err| Error::Forge(format!("could not index graph edges: {err}")))?;
        }
        let unique = IndexOptions::builder().unique(true).build();
        self.nodes
            .create_index(
                IndexModel::builder()
                    .keys(doc! { "repo_id": 1, "node_id": 1 })
                    .options(unique)
                    .build(),
            )
            .await
            .map_err(|err| Error::Forge(format!("could not index graph nodes: {err}")))?;
        Ok(())
    }

    async fn upsert_nodes(&self, nodes: &[GraphNode]) -> Result<u64> {
        let writes = nodes
            .iter()
            .map(|node| {
                (
                    doc! { "_id": format!("{}\u{1f}{}", node.repo_id, node.id) },
                    node_document(node),
                )
            })
            .collect();
        upsert_all(&self.nodes, writes).await
    }

    async fn upsert_edges(&self, edges: &[GraphEdge]) -> Result<u64> {
        let writes = edges
            .iter()
            .map(|edge| (doc! { "_id": edge.id() }, edge_document(edge)))
            .collect();
        upsert_all(&self.edges, writes).await
    }

    async fn delete_repo(&self, repo_id: &str) -> Result<u64> {
        let filter = doc! { "repo_id": repo_id };
        let nodes = self
            .nodes
            .delete_many(filter.clone())
            .await
            .map_err(mongo)?
            .deleted_count;
        let edges = self
            .edges
            .delete_many(filter)
            .await
            .map_err(mongo)?
            .deleted_count;
        Ok(nodes + edges)
    }

    async fn delete_paths(&self, repo_id: &str, paths: &[String]) -> Result<u64> {
        if paths.is_empty() {
            return Ok(0);
        }
        let filter = doc! { "repo_id": repo_id, "path": { "$in": paths } };
        let nodes = self
            .nodes
            .delete_many(filter.clone())
            .await
            .map_err(mongo)?
            .deleted_count;
        let edges = self
            .edges
            .delete_many(filter)
            .await
            .map_err(mongo)?
            .deleted_count;
        Ok(nodes + edges)
    }

    async fn neighbours(
        &self,
        repo_id: &str,
        seeds: &[String],
        hops: u8,
        kinds: &[EdgeKind],
    ) -> Result<Neighbourhood> {
        // Breadth-first with one round trip per hop rather than `$graphLookup`.
        // `$graphLookup` walks in one direction, and the callers of a changed
        // function are as much of the blast radius as its callees; expressing a
        // bidirectional walk there means two lookups and a union per hop, which
        // is no cheaper and considerably harder to read. Hops are capped in the
        // single digits by every caller, so this is a handful of queries.
        let kinds: Vec<Bson> = kinds
            .iter()
            .filter_map(|kind| bson::to_bson(kind).ok())
            .collect();

        let mut reached: BTreeSet<String> = seeds.iter().cloned().collect();
        let mut walked: BTreeMap<String, GraphEdge> = BTreeMap::new();
        let mut frontier: Vec<String> = seeds.to_vec();

        for _ in 0..hops {
            if frontier.is_empty() {
                break;
            }
            let filter = doc! {
                "repo_id": repo_id,
                "kind": { "$in": kinds.clone() },
                "$or": [
                    { "from": { "$in": frontier.clone() } },
                    { "to": { "$in": frontier.clone() } },
                ],
            };
            let mut cursor = self.edges.find(filter).await.map_err(mongo)?;
            let mut next = Vec::new();
            while let Some(document) = cursor.next().await {
                let document = document.map_err(mongo)?;
                let Some(edge) = edge_from_document(&document) else {
                    continue;
                };
                for endpoint in [&edge.from, &edge.to] {
                    if reached.insert(endpoint.clone()) {
                        next.push(endpoint.clone());
                    }
                }
                walked.insert(edge.id(), edge);
            }
            frontier = next;
        }

        let ids: Vec<String> = reached.iter().cloned().collect();
        let mut cursor = self
            .nodes
            .find(doc! { "repo_id": repo_id, "node_id": { "$in": ids } })
            .await
            .map_err(mongo)?;
        let mut nodes = Vec::new();
        while let Some(document) = cursor.next().await {
            nodes.push(node_from_document(&document.map_err(mongo)?));
        }
        nodes.sort_by(|a, b| a.id.cmp(&b.id));

        Ok(Neighbourhood {
            nodes,
            edges: walked.into_values().collect(),
        })
    }
}

/// A [`KnowledgeStore`] over one MongoDB collection.
#[derive(Debug, Clone)]
pub struct MongoKnowledgeStore {
    docs: Collection<KnowledgeDoc>,
}

impl MongoKnowledgeStore {
    /// Open the store in `database`.
    pub fn new(database: &Database) -> Self {
        Self {
            docs: database.collection("knowledge_docs"),
        }
    }

    /// The filter matching every document visible from `scope`.
    ///
    /// A repository sees its own documents *and* its organisation's; an
    /// organisation sees only its own. Expressed as one query rather than two
    /// so a repository lookup is a single round trip.
    fn visible(scope: &KnowledgeScope) -> Document {
        match scope {
            KnowledgeScope::Org { org } => doc! {
                "scope.kind": "org",
                "scope.org": org,
            },
            KnowledgeScope::Repo { repo_id } => doc! {
                "$or": [
                    { "scope.kind": "repo", "scope.repo_id": repo_id },
                    { "scope.kind": "org", "scope.org": scope.owner() },
                ],
            },
        }
    }

    async fn matching(&self, scope: &KnowledgeScope, pinned: bool) -> Result<Vec<KnowledgeDoc>> {
        let mut filter = Self::visible(scope);
        filter.insert("pinned", pinned);
        let mut cursor = self.docs.find(filter).await.map_err(mongo)?;
        let mut found = Vec::new();
        while let Some(document) = cursor.next().await {
            found.push(document.map_err(mongo)?);
        }
        found.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(found)
    }
}

#[async_trait]
impl KnowledgeStore for MongoKnowledgeStore {
    async fn prepare(&self) -> Result<()> {
        for keys in [
            doc! { "scope.kind": 1, "scope.org": 1, "pinned": 1 },
            doc! { "scope.kind": 1, "scope.repo_id": 1, "pinned": 1 },
        ] {
            self.docs
                .create_index(IndexModel::builder().keys(keys).build())
                .await
                .map_err(|err| Error::Forge(format!("could not index knowledge: {err}")))?;
        }
        Ok(())
    }

    async fn put(&self, doc: &KnowledgeDoc) -> Result<()> {
        self.docs
            .replace_one(doc! { "_id": &doc.id }, doc)
            .upsert(true)
            .await
            .map_err(mongo)?;
        Ok(())
    }

    async fn get(&self, id: &str) -> Result<Option<KnowledgeDoc>> {
        self.docs.find_one(doc! { "_id": id }).await.map_err(mongo)
    }

    async fn delete(&self, id: &str) -> Result<bool> {
        Ok(self
            .docs
            .delete_one(doc! { "_id": id })
            .await
            .map_err(mongo)?
            .deleted_count
            > 0)
    }

    async fn pinned(&self, scope: &KnowledgeScope) -> Result<Vec<KnowledgeDoc>> {
        self.matching(scope, true).await
    }

    async fn retrievable(&self, scope: &KnowledgeScope) -> Result<Vec<KnowledgeDoc>> {
        self.matching(scope, false).await
    }
}

#[cfg(test)]
#[path = "mongo_test.rs"]
mod tests;
