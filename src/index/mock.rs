//! In-memory implementations of the four retrieval ports.
//!
//! Always compiled, and not stubs: these are what the test suite runs against
//! and what `local-review` uses when no database is configured. They keep
//! `cargo test` offline, which is the invariant the whole crate is arranged
//! around.
//!
//! [`MockEmbedder`] is deterministic — the same text always yields the same
//! vector, and different texts reliably yield different ones — so retrieval
//! tests can assert on an exact ordering instead of on a tolerance.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use crate::error::{Error, Result};
use crate::index::types::{
    Chunk, EdgeKind, EmbedSignature, EmbeddedChunk, GraphEdge, GraphNode, HybridQuery,
    KnowledgeDoc, KnowledgeScope, Neighbourhood, ScoredChunk,
};
use crate::ports::embed::Embedder;
use crate::ports::graph::GraphStore;
use crate::ports::index::ChunkIndex;
use crate::ports::knowledge::KnowledgeStore;

/// A deterministic offline embedder.
///
/// The vector is a bag-of-tokens hash projected onto `dims` axes and then
/// normalised. That is not a good embedding — it has no semantics at all — but
/// it has the two properties a test needs: identical text embeds identically,
/// and text sharing tokens embeds nearby. Anything cleverer would make the
/// tests depend on the quality of a toy model.
#[derive(Debug, Clone)]
pub struct MockEmbedder {
    signature: EmbedSignature,
}

impl Default for MockEmbedder {
    fn default() -> Self {
        Self::new(64)
    }
}

impl MockEmbedder {
    /// An embedder producing `dims`-wide vectors.
    pub fn new(dims: usize) -> Self {
        Self {
            signature: EmbedSignature::new("mock", "hash-bag", dims),
        }
    }

    /// An embedder that reports a caller-chosen signature.
    ///
    /// Used by the tests that prove a signature change makes previously
    /// indexed rows invisible.
    pub fn with_signature(signature: EmbedSignature) -> Self {
        Self { signature }
    }

    fn vector(&self, text: &str) -> Vec<f32> {
        let dims = self.signature.dims.max(1);
        let mut vector = vec![0.0_f32; dims];
        for token in text
            .split(|c: char| !c.is_alphanumeric())
            .filter(|t| !t.is_empty())
        {
            // FNV-1a: stable across runs and platforms, which a Rust `Hasher`
            // is explicitly not guaranteed to be. A golden test that depends on
            // the ordering of hits needs the former.
            let mut hash = 0xcbf2_9ce4_8422_2325_u64;
            for byte in token.as_bytes() {
                hash ^= u64::from(byte.to_ascii_lowercase());
                hash = hash.wrapping_mul(0x100_0000_01b3);
            }
            let axis = (hash % dims as u64) as usize;
            let sign = if hash & (1 << 63) == 0 { 1.0 } else { -1.0 };
            vector[axis] += sign;
        }
        let norm = vector.iter().map(|v| v * v).sum::<f32>().sqrt();
        if norm > 0.0 {
            for value in &mut vector {
                *value /= norm;
            }
        }
        vector
    }
}

#[async_trait]
impl Embedder for MockEmbedder {
    fn signature(&self) -> EmbedSignature {
        self.signature.clone()
    }

    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        Ok(texts.iter().map(|text| self.vector(text)).collect())
    }
}

/// An in-memory chunk index.
#[derive(Debug, Clone, Default)]
pub struct MockChunkIndex {
    // Keyed by document id, exactly like the real adapter, so upsert
    // idempotence is exercised rather than assumed.
    rows: Arc<Mutex<BTreeMap<String, Row>>>,
    prepared: Arc<Mutex<Vec<EmbedSignature>>>,
}

#[derive(Debug, Clone)]
struct Row {
    signature: String,
    chunk: Chunk,
    vector: Vec<f32>,
}

impl MockChunkIndex {
    /// An empty index.
    pub fn new() -> Self {
        Self::default()
    }

    /// How many chunks are stored.
    pub fn len(&self) -> usize {
        self.rows.lock().expect("index lock").len()
    }

    /// Whether the index holds nothing.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The signatures [`ChunkIndex::prepare`] was called with, in order.
    ///
    /// Recorded so a test can assert the boot path actually prepared the index
    /// rather than relying on lazy creation.
    pub fn prepared(&self) -> Vec<EmbedSignature> {
        self.prepared.lock().expect("index lock").clone()
    }
}

/// Cosine similarity of two equal-length vectors, or `0.0` if they disagree.
///
/// Disagreeing lengths cannot happen within one signature, and across
/// signatures the query filter has already excluded the row — returning zero
/// rather than panicking keeps a mismatch from taking the process down.
fn cosine(a: &[f32], b: &[f32]) -> f64 {
    if a.len() != b.len() {
        return 0.0;
    }
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let na: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let nb: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if na == 0.0 || nb == 0.0 {
        return 0.0;
    }
    f64::from(dot / (na * nb))
}

/// A crude lexical score: the share of query tokens the text contains.
///
/// Not BM25 — the real adapter delegates that to the search engine. This exists
/// so the mock's hybrid query genuinely has two arms, and so a test can prove
/// an exact-identifier match outranks a merely similar one.
fn lexical(text: &str, query: &str) -> f64 {
    let haystack = text.to_lowercase();
    let terms: Vec<String> = query
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(|t| t.to_lowercase())
        .collect();
    if terms.is_empty() {
        return 0.0;
    }
    let hits = terms.iter().filter(|t| haystack.contains(*t)).count();
    hits as f64 / terms.len() as f64
}

#[async_trait]
impl ChunkIndex for MockChunkIndex {
    async fn prepare(&self, signature: &EmbedSignature) -> Result<()> {
        self.prepared
            .lock()
            .expect("index lock")
            .push(signature.clone());
        Ok(())
    }

    async fn upsert(&self, signature: &EmbedSignature, chunks: &[EmbeddedChunk]) -> Result<u64> {
        let mut rows = self.rows.lock().expect("index lock");
        for embedded in chunks {
            if embedded.vector.len() != signature.dims {
                return Err(Error::Config(format!(
                    "chunk {} has {} dimensions but {signature} declares {}",
                    embedded.chunk.id(),
                    embedded.vector.len(),
                    signature.dims
                )));
            }
            rows.insert(
                embedded.chunk.id(),
                Row {
                    signature: signature.key(),
                    chunk: embedded.chunk.clone(),
                    vector: embedded.vector.clone(),
                },
            );
        }
        Ok(chunks.len() as u64)
    }

    async fn delete_repo(&self, repo_id: &str) -> Result<u64> {
        let mut rows = self.rows.lock().expect("index lock");
        let before = rows.len();
        rows.retain(|_, row| row.chunk.repo_id != repo_id);
        Ok((before - rows.len()) as u64)
    }

    async fn delete_paths(&self, repo_id: &str, paths: &[String]) -> Result<u64> {
        if paths.is_empty() {
            return Ok(0);
        }
        let mut rows = self.rows.lock().expect("index lock");
        let before = rows.len();
        rows.retain(|_, row| !(row.chunk.repo_id == repo_id && paths.contains(&row.chunk.path)));
        Ok((before - rows.len()) as u64)
    }

    async fn delete_chunks(&self, repo_id: &str, ids: &[String]) -> Result<u64> {
        if ids.is_empty() {
            return Ok(0);
        }
        let mut rows = self.rows.lock().expect("index lock");
        let before = rows.len();
        // The repo filter is not redundant: an id embeds its repository, but a
        // caller that got one wrong must not be able to delete another
        // repository's chunk through this method.
        rows.retain(|id, row| !(row.chunk.repo_id == repo_id && ids.contains(id)));
        Ok((before - rows.len()) as u64)
    }

    async fn query(&self, query: &HybridQuery) -> Result<Vec<ScoredChunk>> {
        let signature = query.signature.key();
        let rows = self.rows.lock().expect("index lock");
        let mut scored: Vec<ScoredChunk> = rows
            .values()
            .filter(|row| row.signature == signature)
            .filter(|row| match &query.repo_id {
                Some(repo) => row.chunk.repo_id == *repo,
                None => true,
            })
            .map(|row| ScoredChunk {
                score: query.dense_weight * cosine(&row.vector, &query.vector)
                    + query.text_weight * lexical(&row.chunk.text, &query.text),
                chunk: row.chunk.clone(),
            })
            .collect();
        // Ties break on the chunk id so the ordering is total and the golden
        // tests do not flake on map iteration order.
        scored.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.chunk.id().cmp(&b.chunk.id()))
        });
        scored.truncate(query.limit);
        Ok(scored)
    }
}

/// An in-memory code graph.
#[derive(Debug, Clone, Default)]
pub struct MockGraphStore {
    nodes: Arc<Mutex<BTreeMap<String, GraphNode>>>,
    edges: Arc<Mutex<BTreeMap<String, GraphEdge>>>,
}

impl MockGraphStore {
    /// An empty graph.
    pub fn new() -> Self {
        Self::default()
    }

    /// How many nodes are stored.
    pub fn node_count(&self) -> usize {
        self.nodes.lock().expect("graph lock").len()
    }

    /// How many edges are stored.
    pub fn edge_count(&self) -> usize {
        self.edges.lock().expect("graph lock").len()
    }
}

#[async_trait]
impl GraphStore for MockGraphStore {
    async fn prepare(&self) -> Result<()> {
        Ok(())
    }

    async fn upsert_nodes(&self, nodes: &[GraphNode]) -> Result<u64> {
        let mut store = self.nodes.lock().expect("graph lock");
        for node in nodes {
            store.insert(format!("{}\u{1f}{}", node.repo_id, node.id), node.clone());
        }
        Ok(nodes.len() as u64)
    }

    async fn upsert_edges(&self, edges: &[GraphEdge]) -> Result<u64> {
        let mut store = self.edges.lock().expect("graph lock");
        for edge in edges {
            store.insert(edge.id(), edge.clone());
        }
        Ok(edges.len() as u64)
    }

    async fn delete_repo(&self, repo_id: &str) -> Result<u64> {
        let mut nodes = self.nodes.lock().expect("graph lock");
        let mut edges = self.edges.lock().expect("graph lock");
        let before = nodes.len() + edges.len();
        nodes.retain(|_, n| n.repo_id != repo_id);
        edges.retain(|_, e| e.repo_id != repo_id);
        Ok((before - nodes.len() - edges.len()) as u64)
    }

    async fn delete_paths(&self, repo_id: &str, paths: &[String]) -> Result<u64> {
        if paths.is_empty() {
            return Ok(0);
        }
        let mut nodes = self.nodes.lock().expect("graph lock");
        let mut edges = self.edges.lock().expect("graph lock");
        let before = nodes.len() + edges.len();
        nodes.retain(|_, n| !(n.repo_id == repo_id && paths.contains(&n.path)));
        edges.retain(|_, e| !(e.repo_id == repo_id && paths.contains(&e.path)));
        Ok((before - nodes.len() - edges.len()) as u64)
    }

    async fn neighbours(
        &self,
        repo_id: &str,
        seeds: &[String],
        hops: u8,
        kinds: &[EdgeKind],
    ) -> Result<Neighbourhood> {
        let nodes = self.nodes.lock().expect("graph lock");
        let edges = self.edges.lock().expect("graph lock");

        let mut reached: std::collections::BTreeSet<String> = seeds.iter().cloned().collect();
        let mut walked: BTreeMap<String, GraphEdge> = BTreeMap::new();
        let mut frontier: Vec<String> = seeds.to_vec();

        for _ in 0..hops {
            let mut next = Vec::new();
            for edge in edges.values() {
                if edge.repo_id != repo_id || !kinds.contains(&edge.kind) {
                    continue;
                }
                // Undirected traversal: whoever calls a changed function is as
                // much of the blast radius as whatever it calls.
                let other = if frontier.contains(&edge.from) {
                    &edge.to
                } else if frontier.contains(&edge.to) {
                    &edge.from
                } else {
                    continue;
                };
                walked.insert(edge.id(), edge.clone());
                if reached.insert(other.clone()) {
                    next.push(other.clone());
                }
            }
            if next.is_empty() {
                break;
            }
            frontier = next;
        }

        Ok(Neighbourhood {
            nodes: reached
                .iter()
                .filter_map(|id| nodes.get(&format!("{repo_id}\u{1f}{id}")).cloned())
                .collect(),
            edges: walked.into_values().collect(),
        })
    }
}

/// An in-memory knowledge store.
#[derive(Debug, Clone, Default)]
pub struct MockKnowledgeStore {
    docs: Arc<Mutex<BTreeMap<String, KnowledgeDoc>>>,
}

impl MockKnowledgeStore {
    /// An empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// How many documents are stored.
    pub fn len(&self) -> usize {
        self.docs.lock().expect("knowledge lock").len()
    }

    /// Whether the store holds nothing.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Whether a document scoped as `doc` is visible when looking up `query`.
///
/// A repository lookup sees its organisation's documents; an organisation
/// lookup does **not** see one repository's, because a convention written for
/// one repository is not evidence about another.
pub(crate) fn visible(doc: &KnowledgeScope, query: &KnowledgeScope) -> bool {
    match (doc, query) {
        (KnowledgeScope::Org { org }, other) => org == other.owner(),
        (KnowledgeScope::Repo { repo_id }, KnowledgeScope::Repo { repo_id: wanted }) => {
            repo_id == wanted
        }
        (KnowledgeScope::Repo { .. }, KnowledgeScope::Org { .. }) => false,
    }
}

#[async_trait]
impl KnowledgeStore for MockKnowledgeStore {
    async fn prepare(&self) -> Result<()> {
        Ok(())
    }

    async fn put(&self, doc: &KnowledgeDoc) -> Result<()> {
        self.docs
            .lock()
            .expect("knowledge lock")
            .insert(doc.id.clone(), doc.clone());
        Ok(())
    }

    async fn get(&self, id: &str) -> Result<Option<KnowledgeDoc>> {
        Ok(self.docs.lock().expect("knowledge lock").get(id).cloned())
    }

    async fn delete(&self, id: &str) -> Result<bool> {
        Ok(self
            .docs
            .lock()
            .expect("knowledge lock")
            .remove(id)
            .is_some())
    }

    async fn pinned(&self, scope: &KnowledgeScope) -> Result<Vec<KnowledgeDoc>> {
        Ok(self
            .docs
            .lock()
            .expect("knowledge lock")
            .values()
            .filter(|d| d.pinned && visible(&d.scope, scope))
            .cloned()
            .collect())
    }

    async fn retrievable(&self, scope: &KnowledgeScope) -> Result<Vec<KnowledgeDoc>> {
        Ok(self
            .docs
            .lock()
            .expect("knowledge lock")
            .values()
            .filter(|d| !d.pinned && visible(&d.scope, scope))
            .cloned()
            .collect())
    }
}

#[cfg(test)]
#[path = "mock_test.rs"]
mod tests;
