//! The domain types the memory layer speaks in.
//!
//! Always compiled. Nothing here touches a port: what a memory item *is*, how a
//! scope is named, and what a recollection or a grounded answer looks like are
//! ordinary values, so the parts most likely to be wrong are the parts tested
//! offline.
//!
//! The one design decision worth stating up front is [`MemorySection`]. A
//! repository's memory is not one bag: what the code *is*, what the maintainers
//! *said* about it, and what happened to the reviewer's *own earlier findings*
//! are three different kinds of fact, asked for by three different questions.
//! Keeping them in separate sections of one scope means "did the maintainers
//! reject a finding like this before?" can be answered without the answer
//! being drowned by a thousand similar-looking code chunks. The fourth
//! section, discussions, is everything *else* that was said on the
//! repository's issues and pull requests — by maintainers, contributors and
//! other review bots alike — which is where the reasons behind the code live.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Which section of a repository's memory an item belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemorySection {
    /// The repository's source, chunked by symbol where a grammar allows it.
    Code,
    /// What the repository says about itself: instruction files, contributor
    /// guides, module READMEs — the *pointers* a reviewer should hold.
    Conventions,
    /// What the reviewer said before and what the maintainers did with it.
    Reviews,
    /// What was said on the repository's issues and pull requests: the
    /// issues themselves, open and closed, and every comment, inline review
    /// comment and review anybody — human or another agent — left on them.
    Discussions,
}

impl MemorySection {
    /// Every section, in the order a full ingest writes them.
    pub const ALL: [Self; 4] = [
        Self::Code,
        Self::Conventions,
        Self::Reviews,
        Self::Discussions,
    ];

    /// The stable, lowercase name used in scope ids and configuration.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Code => "code",
            Self::Conventions => "conventions",
            Self::Reviews => "reviews",
            Self::Discussions => "discussions",
        }
    }

    /// The inverse of [`Self::as_str`], for configuration and the CLI.
    pub fn parse(name: &str) -> Option<Self> {
        match name.trim() {
            "code" => Some(Self::Code),
            "conventions" => Some(Self::Conventions),
            "reviews" => Some(Self::Reviews),
            "discussions" => Some(Self::Discussions),
            _ => None,
        }
    }
}

/// Where in memory a set of items lives: one repository, one section.
///
/// A scope is the unit of isolation. Nothing recalled for one repository may
/// come from another, which is why the repository id is part of the scope
/// rather than a label on the item — a label is a filter that can be forgotten,
/// a scope is an address that cannot.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct MemoryScope {
    /// The repository, as `owner/name`.
    pub repo: String,
    /// The section within it. `None` addresses the whole repository, which is
    /// what a grounded question is asked against.
    pub section: Option<MemorySection>,
}

impl MemoryScope {
    /// The whole of one repository's memory.
    pub fn repo(repo: impl Into<String>) -> Self {
        Self {
            repo: repo.into(),
            section: None,
        }
    }

    /// One section of one repository's memory.
    pub fn section(repo: impl Into<String>, section: MemorySection) -> Self {
        Self {
            repo: repo.into(),
            section: Some(section),
        }
    }

    /// Whether an item filed under `other` is visible from this scope.
    ///
    /// A repository scope sees every section; a section scope sees only
    /// itself. Different repositories never see each other.
    pub fn covers(&self, other: &Self) -> bool {
        self.repo == other.repo && self.section.is_none_or(|s| other.section == Some(s))
    }
}

impl std::fmt::Display for MemoryScope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.section {
            Some(section) => write!(f, "{}/{}", self.repo, section.as_str()),
            None => f.write_str(&self.repo),
        }
    }
}

/// What kind of thing a memory item records.
///
/// Recorded rather than inferred so the prompt can say which is which. A
/// convention and a rejected finding are both short prose, and a reviewer told
/// only "here is something remembered" cannot tell an instruction it should
/// apply from an outcome it should learn from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryKind {
    /// One chunk of source, usually a whole symbol.
    CodeChunk,
    /// One section of an instruction file or contributor guide.
    Convention,
    /// A finding the reviewer published on a pull request.
    ReviewFinding,
    /// What happened to a published finding: fixed, rejected, or left open.
    ReviewOutcome,
    /// An issue: its title, body and how it ended.
    Issue,
    /// A pull request: its title, body and how it ended.
    PullRequest,
    /// One thing somebody said on an issue or a pull request — a comment,
    /// an inline review comment, or a review — whoever said it.
    Remark,
}

impl MemoryKind {
    /// The section this kind is filed under.
    pub fn section(self) -> MemorySection {
        match self {
            Self::CodeChunk => MemorySection::Code,
            Self::Convention => MemorySection::Conventions,
            Self::ReviewFinding | Self::ReviewOutcome => MemorySection::Reviews,
            Self::Issue | Self::PullRequest | Self::Remark => MemorySection::Discussions,
        }
    }

    /// The stable, lowercase name used in labels and prompts.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::CodeChunk => "code",
            Self::Convention => "convention",
            Self::ReviewFinding => "finding",
            Self::ReviewOutcome => "outcome",
            Self::Issue => "issue",
            Self::PullRequest => "pull-request",
            Self::Remark => "remark",
        }
    }
}

/// One thing to remember.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryItem {
    /// A stable identity within its scope: the same key with the same body is
    /// the same memory and is written once, however many times it is offered.
    pub key: String,
    /// What this is.
    pub kind: MemoryKind,
    /// The repository path this is about, when it is about one.
    pub path: Option<String>,
    /// The symbol this is about, when a grammar named one.
    pub symbol: Option<String>,
    /// A one-line heading: a symbol name, a section heading, a finding title.
    pub title: String,
    /// The content itself.
    pub body: String,
    /// Free-form labels an engine may index: lane ids, severities, outcomes.
    pub labels: Vec<String>,
    /// When this was observed, as an RFC 3339 timestamp, when the caller knows.
    ///
    /// An outcome observed a month ago and one observed today are different
    /// evidence, and an engine that scores freshness needs to be told which is
    /// which rather than dating both to the ingest.
    pub observed_at: Option<String>,
}

impl MemoryItem {
    /// A new item under `key`, with nothing but its kind, title and body set.
    pub fn new(
        key: impl Into<String>,
        kind: MemoryKind,
        title: impl Into<String>,
        body: impl Into<String>,
    ) -> Self {
        Self {
            key: key.into(),
            kind,
            path: None,
            symbol: None,
            title: title.into(),
            body: body.into(),
            labels: Vec::new(),
            observed_at: None,
        }
    }

    /// Set the path this item is about.
    pub fn at_path(mut self, path: impl Into<String>) -> Self {
        self.path = Some(path.into());
        self
    }

    /// Set the symbol this item is about.
    pub fn at_symbol(mut self, symbol: impl Into<String>) -> Self {
        self.symbol = Some(symbol.into());
        self
    }

    /// Add a label.
    pub fn labelled(mut self, label: impl Into<String>) -> Self {
        self.labels.push(label.into());
        self
    }

    /// Set when this was observed.
    pub fn observed(mut self, at: impl Into<String>) -> Self {
        self.observed_at = Some(at.into());
        self
    }

    /// The section this item is filed under.
    pub fn section(&self) -> MemorySection {
        self.kind.section()
    }

    /// A content-addressed identity for idempotent writes.
    ///
    /// Hashes the key *and* the body: the same key offered with a different
    /// body is a new version, not a replay, and an engine that refuses to
    /// overwrite must be able to tell the two apart.
    pub fn content_id(&self) -> String {
        let mut digest = Sha256::new();
        digest.update(self.key.as_bytes());
        digest.update([0]);
        digest.update(self.body.as_bytes());
        hex(&digest.finalize())
    }
}

/// What a batch write reported.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RememberReport {
    /// Items the engine had not seen before.
    pub written: usize,
    /// Items the engine already held, by content identity.
    pub replayed: usize,
}

impl RememberReport {
    /// Fold another batch's report into this one.
    pub fn merge(&mut self, other: Self) {
        self.written += other.written;
        self.replayed += other.replayed;
    }
}

/// One item that came back from recall.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Recollection {
    /// The item, as it was remembered.
    pub item: MemoryItem,
    /// The engine's score, when it reports one. CortexDB does not, so the
    /// order of the returned list is the ranking and this is `None`.
    pub score: Option<f64>,
}

/// One source a grounded answer cited.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Citation {
    /// The engine's identifier for the cited memory.
    pub id: String,
    /// The path the cited memory was about, when it was about one.
    pub path: Option<String>,
    /// The cited text, when the engine returned it.
    pub excerpt: Option<String>,
}

/// One question put to the engine, and how its evidence is gathered.
///
/// A question and the query that finds its evidence are two different
/// texts, and conflating them is what made asking fragile. A question is a
/// sentence — "which rules apply to changes under these paths?" — and an
/// engine's recall over a sentence is at the mercy of every word in it:
/// measured against a live CortexDB, a handful of words that name hub
/// entities (`src`, `README.md`, the envelope header) turned a one-second
/// recall into its two-minute deadline, with the same words a second
/// earlier answering in under a second on their own. A short bag of the
/// change's own identifiers never did that. So the evidence is gathered by
/// keywords, and the question is asked over what they found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ask<'a> {
    /// The question, in whatever words the operator wrote it.
    pub question: &'a str,
    /// Keywords the evidence is gathered by, in place of the question's own
    /// words. `None` asks in the question's words — the CLI's raw form.
    pub evidence: Option<&'a str>,
    /// How the answer should be shaped — "one paragraph, name paths". Never
    /// carries repository text.
    pub instructions: Option<&'a str>,
}

impl<'a> Ask<'a> {
    /// A question asked in its own words.
    pub fn new(question: &'a str) -> Self {
        Self {
            question,
            evidence: None,
            instructions: None,
        }
    }

    /// The same question, with its evidence gathered by `keywords`.
    pub fn with_evidence(mut self, keywords: &'a str) -> Self {
        self.evidence = Some(keywords);
        self
    }

    /// The same question, with `instructions` on the answer's shape.
    pub fn shaped(mut self, instructions: &'a str) -> Self {
        self.instructions = Some(instructions);
        self
    }

    /// What recall is run on to gather the evidence.
    pub fn evidence_query(&self) -> &'a str {
        self.evidence
            .filter(|keywords| !keywords.trim().is_empty())
            .unwrap_or(self.question)
    }
}

/// A grounded answer to a question about the repository.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryAnswer {
    /// The question, as asked.
    pub question: String,
    /// The answer. Empty when the engine had nothing to ground one on.
    pub answer: String,
    /// What it was grounded on.
    pub citations: Vec<Citation>,
    /// The model that wrote it, when the engine names one.
    pub model: Option<String>,
}

impl MemoryAnswer {
    /// Whether the engine actually answered rather than declining.
    pub fn is_grounded(&self) -> bool {
        !self.answer.trim().is_empty() && !self.citations.is_empty()
    }
}

/// Lowercase hex of a digest.
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_repo_scope_covers_every_section_and_a_section_scope_only_itself() {
        let repo = MemoryScope::repo("o/r");
        let code = MemoryScope::section("o/r", MemorySection::Code);
        let reviews = MemoryScope::section("o/r", MemorySection::Reviews);
        assert!(repo.covers(&code));
        assert!(repo.covers(&reviews));
        assert!(code.covers(&code));
        assert!(!code.covers(&reviews));
        assert!(!MemoryScope::repo("o/other").covers(&code));
        // A section scope does not cover the whole repository.
        assert!(!code.covers(&repo));
    }

    #[test]
    fn scopes_render_as_repo_then_section() {
        assert_eq!(MemoryScope::repo("o/r").to_string(), "o/r");
        assert_eq!(
            MemoryScope::section("o/r", MemorySection::Conventions).to_string(),
            "o/r/conventions"
        );
    }

    #[test]
    fn every_kind_files_under_the_section_it_names() {
        assert_eq!(MemoryKind::CodeChunk.section(), MemorySection::Code);
        assert_eq!(MemoryKind::Convention.section(), MemorySection::Conventions);
        assert_eq!(MemoryKind::ReviewFinding.section(), MemorySection::Reviews);
        assert_eq!(MemoryKind::ReviewOutcome.section(), MemorySection::Reviews);
    }

    #[test]
    fn content_id_changes_with_the_body_and_the_key_but_not_the_labels() {
        let a = MemoryItem::new("k", MemoryKind::Convention, "t", "body");
        let same = a.clone().labelled("extra");
        let other_body = MemoryItem::new("k", MemoryKind::Convention, "t", "body2");
        let other_key = MemoryItem::new("k2", MemoryKind::Convention, "t", "body");
        assert_eq!(a.content_id(), same.content_id());
        assert_ne!(a.content_id(), other_body.content_id());
        assert_ne!(a.content_id(), other_key.content_id());
        assert_eq!(a.content_id().len(), 64);
    }

    #[test]
    fn an_answer_is_grounded_only_with_text_and_a_citation() {
        let mut answer = MemoryAnswer {
            question: "q".into(),
            answer: "a".into(),
            citations: vec![],
            model: None,
        };
        assert!(!answer.is_grounded());
        answer.citations.push(Citation {
            id: "1".into(),
            path: None,
            excerpt: None,
        });
        assert!(answer.is_grounded());
        answer.answer = "  ".into();
        assert!(!answer.is_grounded());
    }
}
