//! The domain types the retrieval pipeline speaks in.
//!
//! Always compiled. Nothing here touches a port: assembling, budgeting and
//! rendering retrieved context are ordinary functions over ordinary values, so
//! the parts most likely to be wrong are the parts tested offline.
//!
//! Two of these types exist purely so a review can be *honest about what it
//! saw*. [`RetrievalStatus`] records why a review had less context than the
//! operator believes it had, and [`RetrievedContext::dropped`] records what the
//! budget threw away. A reviewer running blind while reporting nothing unusual
//! is a worse outcome than one that says the index was cold.

use std::fmt::Write as _;

use crate::index::types::Chunk;

/// Where a retrieved chunk came from.
///
/// Recorded rather than inferred because the two are answers to different
/// questions and a reader of the prompt should be able to tell them apart:
/// similarity says *this looks like the change*, the graph says *this is what
/// the change reaches*. The second is routinely the one that matters and it
/// never scores well on the first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Provenance {
    /// Returned by the hybrid dense + lexical query.
    Search,
    /// Reached by walking the code graph out from the diff.
    Graph,
}

impl Provenance {
    /// The word written next to the chunk in the prompt.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Search => "similar code",
            Self::Graph => "reached from the diff",
        }
    }
}

/// One chunk that survived ranking, dedupe and the token budget.
#[derive(Debug, Clone, PartialEq)]
pub struct RetrievedChunk {
    /// The chunk itself.
    pub chunk: Chunk,
    /// Its fused search score, or `0.0` for a chunk the graph supplied.
    pub score: f64,
    /// How it was found.
    pub provenance: Provenance,
}

impl RetrievedChunk {
    /// Whether this chunk's line span overlaps `other`'s in the same file.
    ///
    /// The dedupe predicate. Chunk boundaries move when a file is re-chunked
    /// and the search and graph arms routinely return overlapping spans of the
    /// same function, so equality on `(path, start, end)` would let the same
    /// code through twice under two different line ranges.
    pub fn overlaps(&self, other: &Self) -> bool {
        self.chunk.path == other.chunk.path
            && self.chunk.start_line <= other.chunk.end_line
            && other.chunk.start_line <= self.chunk.end_line
    }

    /// How many tokens this chunk costs once rendered.
    pub fn tokens(&self) -> usize {
        crate::harness::pricing::estimate_tokens(&self.render()) as usize
    }

    /// The chunk as it appears in the prompt.
    fn render(&self) -> String {
        let mut out = String::with_capacity(self.chunk.text.len() + 128);
        let _ = write!(
            out,
            "// {}:{}-{}",
            self.chunk.path, self.chunk.start_line, self.chunk.end_line
        );
        if let Some(symbol) = &self.chunk.symbol {
            let _ = write!(out, " ({symbol})");
        }
        let _ = writeln!(out, " — {}", self.provenance.as_str());
        out.push_str(self.chunk.text.trim_end());
        out.push('\n');
        out
    }
}

/// Why a review saw less than the full index.
///
/// Deliberately an enum rather than a boolean. "Retrieval was degraded" is not
/// actionable; "the index is cold" tells an operator to run the indexer and
/// "hybrid search is unavailable" tells them to look at the deployment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RetrievalStatus {
    /// Nobody asked for retrieval: no index wired in, or `retrieval.enabled`
    /// is false. Not reported to the pull request — an operator who turned it
    /// off does not need telling on every review.
    Off,
    /// Retrieval ran against an index that reflects the code under review.
    Ready,
    /// This repository has never been indexed under the current embedding
    /// signature.
    Cold,
    /// The index exists but does not reflect the commit under review.
    Stale {
        /// The revision the index does reflect, when it names one.
        indexed: Option<String>,
    },
    /// The index or the search engine could not be reached.
    Unavailable {
        /// What went wrong, for the operator reading the check run.
        reason: String,
    },
}

impl RetrievalStatus {
    /// Whether a review under this status saw the context it should have.
    pub fn is_degraded(&self) -> bool {
        !matches!(self, Self::Off | Self::Ready)
    }
}

/// Everything retrieval contributed to one review.
#[derive(Debug, Clone, PartialEq)]
pub struct RetrievedContext {
    /// Why this is or is not a full-context review.
    pub status: RetrievalStatus,
    /// The chunks that reached the prompt, best first.
    pub chunks: Vec<RetrievedChunk>,
    /// Candidates the budget, the chunk cap or dedupe removed.
    ///
    /// Reported rather than discarded silently, for the same reason the review
    /// reports deduped findings: a filter whose effect nobody can see is a
    /// filter nobody can tell is misconfigured.
    pub dropped: usize,
    /// Tokens the retained chunks are estimated to cost.
    pub tokens: usize,
    /// Graph nodes the walk reached, before chunks were fetched for them.
    pub graph_nodes: usize,
    /// What existing code depends on the change, and what no test reaches.
    ///
    /// Not chunks, and deliberately not budgeted like them: this is a list of
    /// names, a line each, and it is the one part of retrieval that stays
    /// useful when the index holds no chunk for any of them.
    pub impact: crate::graph::impact::Impact,
}

impl Default for RetrievedContext {
    fn default() -> Self {
        Self::off()
    }
}

impl RetrievedContext {
    /// The context of a review that retrieved nothing because nothing asked it
    /// to.
    pub fn off() -> Self {
        Self {
            status: RetrievalStatus::Off,
            chunks: Vec::new(),
            dropped: 0,
            tokens: 0,
            graph_nodes: 0,
            impact: Default::default(),
        }
    }

    /// The context of a review whose retrieval failed or had nothing to read.
    pub fn degraded(status: RetrievalStatus) -> Self {
        Self {
            status,
            ..Self::off()
        }
    }

    /// Whether any chunk reached the prompt.
    pub fn is_empty(&self) -> bool {
        self.chunks.is_empty()
    }

    /// Whether the lane is shown nothing at all.
    ///
    /// Distinct from [`RetrievedContext::is_empty`] because the blast radius
    /// survives an index that holds no chunk for any dependent: knowing
    /// `Ledger::settle` has fourteen callers and no test is worth saying even
    /// when none of the fourteen can be quoted.
    pub fn renders_nothing(&self) -> bool {
        self.chunks.is_empty() && self.impact.is_empty()
    }

    /// How many chunks each arm contributed.
    ///
    /// The acceptance measurement for graph expansion: a pipeline whose graph
    /// count is always zero has the integration in name only.
    pub fn counts(&self) -> (usize, usize) {
        let graph = self
            .chunks
            .iter()
            .filter(|c| c.provenance == Provenance::Graph)
            .count();
        (self.chunks.len() - graph, graph)
    }

    /// The block handed to a lane, or an empty string when there is nothing.
    ///
    /// This is prompt **suffix** material and nothing else. It varies with the
    /// pull request, so putting it in the cacheable prefix would destroy every
    /// cache hit while looking correct — see `crate::harness::prompt`.
    pub fn render(&self) -> String {
        if self.renders_nothing() {
            return String::new();
        }
        let mut out = String::with_capacity(self.tokens * 4 + 256);
        // The blast radius leads. It is the shortest part and the part that
        // says what to look for; a reviewer that reads only the first lines of
        // the block should get the warning, not the first quoted chunk.
        out.push_str(&self.impact.render());
        for chunk in &self.chunks {
            out.push_str(&chunk.render());
            out.push('\n');
        }
        out
    }

    /// The sentence a check-run summary carries when retrieval was not whole.
    ///
    /// `None` when there is nothing to admit. A review that quietly ran with
    /// less context than the operator believes it had is worse than one that
    /// says so, which is the entire reason this returns a string rather than a
    /// log line.
    pub fn note(&self) -> Option<String> {
        match &self.status {
            RetrievalStatus::Off => None,
            RetrievalStatus::Ready if self.chunks.is_empty() => {
                Some("No related code was found in the index; reviewed from the diff alone.".into())
            }
            RetrievalStatus::Ready => None,
            RetrievalStatus::Cold => Some(
                "The code index for this repository is cold, so this review saw the diff alone."
                    .into(),
            ),
            RetrievalStatus::Stale { indexed } => Some(match indexed {
                Some(revision) => format!(
                    "The code index is behind this pull request (indexed at `{}`), so retrieved \
                     context may be out of date.",
                    &revision[..revision.len().min(12)]
                ),
                None => "The code index does not reflect this commit, so retrieved context may be \
                         out of date."
                    .into(),
            }),
            RetrievalStatus::Unavailable { reason } => Some(format!(
                "Code retrieval was unavailable ({reason}), so this review saw the diff alone."
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(path: &str, start: u32, end: u32) -> RetrievedChunk {
        RetrievedChunk {
            chunk: Chunk {
                repo_id: "acme/app".into(),
                path: path.into(),
                start_line: start,
                end_line: end,
                text: "fn thing() {}".into(),
                ..Chunk::default()
            },
            score: 1.0,
            provenance: Provenance::Search,
        }
    }

    #[test]
    fn spans_of_the_same_file_overlap_when_they_share_a_line() {
        assert!(chunk("a.rs", 1, 10).overlaps(&chunk("a.rs", 10, 20)));
        assert!(!chunk("a.rs", 1, 10).overlaps(&chunk("a.rs", 11, 20)));
        assert!(!chunk("a.rs", 1, 10).overlaps(&chunk("b.rs", 1, 10)));
    }

    #[test]
    fn a_disabled_retrieval_admits_nothing_and_a_cold_one_admits_everything() {
        assert_eq!(RetrievedContext::off().note(), None);
        let cold = RetrievedContext::degraded(RetrievalStatus::Cold);
        assert!(cold.note().expect("says so").contains("cold"));
        assert!(cold.status.is_degraded());
    }

    #[test]
    fn a_stale_index_names_the_revision_it_reflects() {
        let stale = RetrievedContext::degraded(RetrievalStatus::Stale {
            indexed: Some("0123456789abcdef0123".into()),
        });
        let note = stale.note().expect("says so");
        assert!(note.contains("0123456789ab"), "{note}");
        assert!(!note.contains("cdef"), "the sha is abbreviated: {note}");
    }

    #[test]
    fn a_ready_index_with_no_hits_still_says_the_review_was_diff_only() {
        // "Ready and empty" is not a failure, but it is still a review that ran
        // on less than the operator expects, so it is stated.
        let empty = RetrievedContext::degraded(RetrievalStatus::Ready);
        assert!(empty.note().expect("says so").contains("No related code"));
        assert!(!empty.status.is_degraded());
    }

    #[test]
    fn a_rendered_chunk_names_its_file_lines_and_provenance() {
        let mut graph = chunk("src/caller.rs", 4, 9);
        graph.provenance = Provenance::Graph;
        graph.chunk.symbol = Some("call_site".into());
        let context = RetrievedContext {
            status: RetrievalStatus::Ready,
            chunks: vec![graph],
            dropped: 0,
            tokens: 0,
            graph_nodes: 1,
        };
        let rendered = context.render();

        assert!(rendered.contains("src/caller.rs:4-9"));
        assert!(rendered.contains("(call_site)"));
        assert!(rendered.contains("reached from the diff"));
        assert_eq!(context.counts(), (0, 1));
    }

    #[test]
    fn an_empty_context_renders_to_nothing_rather_than_an_empty_heading() {
        assert!(RetrievedContext::off().render().is_empty());
    }
}
