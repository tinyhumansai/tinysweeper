//! The domain types the retrieval ports speak in.
//!
//! Always compiled. Like `forge/types.rs`, these are hand-written shapes rather
//! than a database's wire types: that is what lets the offline mocks be
//! first-class implementations instead of stubs, and what keeps the MongoDB
//! driver out of the default build.

use serde::{Deserialize, Serialize};

/// Which embedding model produced a vector.
///
/// This is the index's **partition key**, not decoration. Vectors from two
/// different models are not comparable — cosine distance between them is a
/// number, but a meaningless one — so an index that mixed them would return
/// confident nonsense rather than fail. Writing the signature onto every
/// document and filtering every query by it means swapping the embedding model
/// invalidates the index by construction: the old rows are still there, they
/// simply stop matching.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct EmbedSignature {
    /// The provider that served the embedding, e.g. `voyage`.
    pub provider: String,
    /// The model id, e.g. `voyage-code-3`.
    pub model: String,
    /// How many dimensions the vectors have.
    ///
    /// Carried separately from the model id because the vector-search index
    /// declares it at creation time, and a mismatch is rejected by the server
    /// rather than silently truncated.
    pub dims: usize,
}

impl EmbedSignature {
    /// Build a signature.
    pub fn new(provider: impl Into<String>, model: impl Into<String>, dims: usize) -> Self {
        Self {
            provider: provider.into(),
            model: model.into(),
            dims,
        }
    }

    /// The stable string written onto every indexed document.
    ///
    /// Stored as one scalar rather than three fields so the partition filter is
    /// a single equality match, which is what an index can serve cheaply.
    pub fn key(&self) -> String {
        format!("{}:{}:{}", self.provider, self.model, self.dims)
    }
}

impl std::fmt::Display for EmbedSignature {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.key())
    }
}

/// How a chunk's boundaries were chosen.
///
/// Recorded on every chunk, and this is a correctness signal rather than
/// telemetry. A [`ChunkMethod::Parsed`] chunk is guaranteed to contain whole
/// definitions, so quoting it back at a reviewer quotes something that compiles
/// in isolation; a [`ChunkMethod::Lines`] chunk carries no such promise and may
/// begin halfway through a function. Retrieval that treated the two alike would
/// present a half-function with the same confidence as a whole one, so the
/// distinction is stored rather than inferred from the language.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default, Serialize, Deserialize,
)]
#[serde(rename_all = "lowercase")]
pub enum ChunkMethod {
    /// A grammar named the boundaries: the span is one or more whole
    /// definitions and never cuts a body in half.
    Parsed,
    /// No grammar was available, or the definition was too large to keep whole,
    /// so the span was cut on line boundaries. The default, because a chunk
    /// that has not said otherwise has not earned the stronger claim.
    #[default]
    Lines,
}

impl ChunkMethod {
    /// Whether a grammar chose these boundaries.
    pub fn is_parsed(self) -> bool {
        matches!(self, Self::Parsed)
    }

    /// The stable string written onto an indexed document.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Parsed => "parsed",
            Self::Lines => "lines",
        }
    }
}

/// One indexed span of a file.
///
/// Line numbers are 1-based and inclusive at both ends, matching how the
/// evidence module reports hunks — a retrieval hit has to be quotable back at a
/// diff line without an off-by-one translation step in between.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Chunk {
    /// The repository this chunk belongs to, as `owner/name`.
    pub repo_id: String,
    /// Path within the repository, forward-slashed and repo-relative.
    pub path: String,
    /// First line of the span, 1-based inclusive.
    pub start_line: u32,
    /// Last line of the span, 1-based inclusive.
    pub end_line: u32,
    /// The text that was embedded.
    pub text: String,
    /// Detected language, when known.
    pub lang: Option<String>,
    /// The enclosing symbol, when the chunker could name one.
    pub symbol: Option<String>,
    /// How the span's boundaries were chosen.
    ///
    /// `#[serde(default)]` so documents written before this field existed read
    /// back as [`ChunkMethod::Lines`] — the weaker claim, which is the safe way
    /// for a missing field to resolve.
    #[serde(default)]
    pub chunked_by: ChunkMethod,
    /// Hash of `text`.
    ///
    /// Re-indexing is incremental: a chunk whose hash is unchanged does not
    /// need re-embedding, which is the difference between re-indexing a
    /// repository on every push and re-indexing the files that moved.
    pub content_hash: String,
}

impl Chunk {
    /// The document id for this chunk.
    ///
    /// Deterministic, so an upsert of the same span twice is one row rather
    /// than two. The content hash is part of it so an edited span becomes a
    /// *new* document — the stale one is removed by the path delete that
    /// precedes every incremental re-index, and never lingers as a silent
    /// half-match.
    pub fn id(&self) -> String {
        format!(
            "{}\u{1f}{}\u{1f}{}\u{1f}{}",
            self.repo_id, self.path, self.start_line, self.content_hash
        )
    }
}

/// A chunk together with the vector that was computed for it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EmbeddedChunk {
    /// The chunk itself, stored as the hit's payload.
    pub chunk: Chunk,
    /// Its embedding, in the signature's dimensionality.
    pub vector: Vec<f32>,
}

/// A retrieval hit.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScoredChunk {
    /// The chunk that matched.
    pub chunk: Chunk,
    /// Its fused score. Higher is better; the scale is not comparable across
    /// backends, so this orders results and nothing else.
    pub score: f64,
}

/// A hybrid dense + lexical query.
///
/// Both arms are required rather than optional. Dense-only retrieval misses
/// exact identifiers — the thing a code reviewer searches for most — and
/// lexical-only retrieval misses paraphrase. Running one arm is a degraded
/// query, so the type does not offer it as a choice.
#[derive(Debug, Clone, PartialEq)]
pub struct HybridQuery {
    /// The embedding signature to search within. Rows written by any other
    /// model are invisible to this query.
    pub signature: EmbedSignature,
    /// Restrict to one repository. `None` searches every indexed repository,
    /// which is what a cross-repo convention lookup wants.
    pub repo_id: Option<String>,
    /// The query text, for the lexical arm.
    pub text: String,
    /// The query embedding, for the dense arm.
    pub vector: Vec<f32>,
    /// How many hits to return.
    pub limit: usize,
    /// Weight of the dense arm in the rank fusion.
    pub dense_weight: f64,
    /// Weight of the lexical arm in the rank fusion.
    pub text_weight: f64,
}

impl HybridQuery {
    /// A query with the default weights.
    ///
    /// The dense arm is weighted slightly higher because the lexical arm
    /// already wins outright whenever the query is an exact identifier; the
    /// balance only matters for the queries in between.
    pub fn new(signature: EmbedSignature, text: impl Into<String>, vector: Vec<f32>) -> Self {
        Self {
            signature,
            repo_id: None,
            text: text.into(),
            vector,
            limit: 20,
            dense_weight: 0.6,
            text_weight: 0.4,
        }
    }

    /// Restrict the query to one repository.
    pub fn in_repo(mut self, repo_id: impl Into<String>) -> Self {
        self.repo_id = Some(repo_id.into());
        self
    }

    /// Cap the number of hits.
    pub fn limit(mut self, limit: usize) -> Self {
        self.limit = limit;
        self
    }
}

/// What a graph node stands for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NodeKind {
    /// A whole file.
    File,
    /// A named definition within a file.
    Symbol,
}

/// How two nodes relate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EdgeKind {
    /// The source file imports the target file or module.
    Imports,
    /// The source file defines the target symbol.
    Defines,
    /// The source symbol calls the target symbol.
    Calls,
    /// The source mentions the target without calling it.
    References,
}

impl EdgeKind {
    /// Every edge kind, for traversals that do not want to filter.
    pub const ALL: [EdgeKind; 4] = [
        EdgeKind::Imports,
        EdgeKind::Defines,
        EdgeKind::Calls,
        EdgeKind::References,
    ];
}

/// A file or symbol in the code graph.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphNode {
    /// Stable node id, unique within the repository.
    pub id: String,
    /// The repository, as `owner/name`.
    pub repo_id: String,
    /// Whether this is a file or a symbol.
    pub kind: NodeKind,
    /// The file this node lives in. Present for symbols too, because deleting
    /// a file has to delete its symbols and a path filter is the only cheap
    /// way to find them.
    pub path: String,
    /// The symbol name, for [`NodeKind::Symbol`].
    pub symbol: Option<String>,
    /// Detected language, when known.
    pub lang: Option<String>,
}

impl GraphNode {
    /// A file node.
    pub fn file(repo_id: impl Into<String>, path: impl Into<String>) -> Self {
        let path = path.into();
        Self {
            id: path.clone(),
            repo_id: repo_id.into(),
            kind: NodeKind::File,
            path,
            symbol: None,
            lang: None,
        }
    }

    /// A symbol node within `path`.
    pub fn symbol(
        repo_id: impl Into<String>,
        path: impl Into<String>,
        symbol: impl Into<String>,
    ) -> Self {
        let path = path.into();
        let symbol = symbol.into();
        Self {
            id: format!("{path}#{symbol}"),
            repo_id: repo_id.into(),
            kind: NodeKind::Symbol,
            path,
            symbol: Some(symbol),
            lang: None,
        }
    }
}

/// A directed relationship between two nodes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphEdge {
    /// The repository, as `owner/name`. Edges never cross repositories.
    pub repo_id: String,
    /// Source node id.
    pub from: String,
    /// Target node id.
    pub to: String,
    /// What the edge means.
    pub kind: EdgeKind,
    /// The file the edge was observed in.
    ///
    /// Denormalised from the source node on purpose: an incremental re-index
    /// deletes by path, and without this field that delete would have to
    /// resolve node ids first — an extra round trip per changed file.
    pub path: String,
}

impl GraphEdge {
    /// Build an edge observed in `path`.
    pub fn new(
        repo_id: impl Into<String>,
        from: impl Into<String>,
        to: impl Into<String>,
        kind: EdgeKind,
        path: impl Into<String>,
    ) -> Self {
        Self {
            repo_id: repo_id.into(),
            from: from.into(),
            to: to.into(),
            kind,
            path: path.into(),
        }
    }

    /// The document id for this edge, deterministic so upserts are idempotent.
    pub fn id(&self) -> String {
        format!(
            "{}\u{1f}{}\u{1f}{}\u{1f}{:?}",
            self.repo_id, self.from, self.to, self.kind
        )
    }
}

/// The result of a k-hop traversal.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Neighbourhood {
    /// Every node reached, including the seeds that existed.
    pub nodes: Vec<GraphNode>,
    /// Every edge walked.
    pub edges: Vec<GraphEdge>,
}

/// Who a knowledge document applies to.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum KnowledgeScope {
    /// Applies to every repository in an organisation.
    Org {
        /// The organisation login.
        org: String,
    },
    /// Applies to one repository only.
    Repo {
        /// The repository, as `owner/name`.
        repo_id: String,
    },
}

impl KnowledgeScope {
    /// Scope to an organisation.
    pub fn org(org: impl Into<String>) -> Self {
        Self::Org { org: org.into() }
    }

    /// Scope to one repository.
    pub fn repo(repo_id: impl Into<String>) -> Self {
        Self::Repo {
            repo_id: repo_id.into(),
        }
    }

    /// The organisation a repo-scoped document inherits from.
    ///
    /// Used to widen a lookup: a review of `tinyhumansai/tinysweeper` must see
    /// both the repository's own conventions and the org-wide ones.
    pub fn owner(&self) -> &str {
        match self {
            Self::Org { org } => org,
            Self::Repo { repo_id } => repo_id.split('/').next().unwrap_or(repo_id),
        }
    }
}

/// A curated piece of context: a convention, a runbook, a past decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KnowledgeDoc {
    /// Stable document id, chosen by whoever wrote it.
    ///
    /// Renamed to `_id` on the wire, matching `server::store::Contributor`: the
    /// id *is* the primary key, and letting MongoDB generate a second one would
    /// make an upsert insert a duplicate instead of replacing.
    #[serde(rename = "_id")]
    pub id: String,
    /// Who the document applies to.
    pub scope: KnowledgeScope,
    /// A short human title.
    pub title: String,
    /// The document body. Untrusted input as far as prompts are concerned:
    /// anyone with write access to the knowledge base can put text here, so it
    /// is fenced and labelled as data exactly like a pull request body.
    pub body: String,
    /// Whether this document is *always* included in a prompt.
    ///
    /// Pinned documents bypass retrieval entirely. That is the point: "we never
    /// use `unwrap` in library code" must not depend on the query happening to
    /// embed near it. Pinning is deliberately expensive — it spends context on
    /// every single review — so the port keeps the two sets separate rather
    /// than making it a relevance boost that can be tuned away by accident.
    pub pinned: bool,
    /// Free-form tags for filtering.
    #[serde(default)]
    pub tags: Vec<String>,
}

impl KnowledgeDoc {
    /// A retrievable (not pinned) document.
    pub fn new(
        id: impl Into<String>,
        scope: KnowledgeScope,
        title: impl Into<String>,
        body: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            scope,
            title: title.into(),
            body: body.into(),
            pinned: false,
            tags: Vec::new(),
        }
    }

    /// Mark the document as always-included.
    pub fn pinned(mut self) -> Self {
        self.pinned = true;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_signature_key_separates_models_of_equal_width() {
        // The failure this guards against is silent: two 1024-dimension models
        // whose vectors are shaped alike but mean nothing to each other.
        let a = EmbedSignature::new("voyage", "voyage-code-3", 1024);
        let b = EmbedSignature::new("voyage", "voyage-3-large", 1024);
        assert_ne!(a.key(), b.key());
        assert_eq!(a.key(), "voyage:voyage-code-3:1024");
    }

    #[test]
    fn a_chunk_id_changes_when_its_content_changes() {
        let mut chunk = Chunk {
            repo_id: "tinyhumansai/tinysweeper".into(),
            path: "src/lib.rs".into(),
            start_line: 1,
            end_line: 20,
            content_hash: "aaa".into(),
            ..Chunk::default()
        };
        let before = chunk.id();
        chunk.content_hash = "bbb".into();
        assert_ne!(before, chunk.id());
    }

    #[test]
    fn a_chunk_id_is_stable_across_identical_upserts() {
        let chunk = Chunk {
            repo_id: "o/r".into(),
            path: "a.rs".into(),
            start_line: 3,
            end_line: 9,
            content_hash: "h".into(),
            ..Chunk::default()
        };
        assert_eq!(chunk.id(), chunk.clone().id());
    }

    #[test]
    fn a_symbol_node_id_is_scoped_by_its_file() {
        // Two files may define `main`; the ids must not collide.
        let one = GraphNode::symbol("o/r", "src/a.rs", "main");
        let two = GraphNode::symbol("o/r", "src/b.rs", "main");
        assert_ne!(one.id, two.id);
    }

    #[test]
    fn a_repo_scope_reports_the_org_it_inherits_from() {
        assert_eq!(
            KnowledgeScope::repo("tinyhumansai/x").owner(),
            "tinyhumansai"
        );
        assert_eq!(KnowledgeScope::org("tinyhumansai").owner(), "tinyhumansai");
    }

    #[test]
    fn a_hybrid_query_defaults_to_both_arms_weighted() {
        let query = HybridQuery::new(EmbedSignature::new("p", "m", 4), "hello", vec![0.0; 4]);
        assert!(query.dense_weight > 0.0 && query.text_weight > 0.0);
        assert!(query.repo_id.is_none());
        assert_eq!(query.in_repo("o/r").limit(5).limit, 5);
    }

    #[test]
    fn scopes_round_trip_through_bson_shaped_json() {
        for scope in [KnowledgeScope::org("o"), KnowledgeScope::repo("o/r")] {
            let encoded = serde_json::to_value(&scope).expect("encodes");
            let decoded: KnowledgeScope = serde_json::from_value(encoded).expect("decodes");
            assert_eq!(decoded, scope);
        }
    }
}
