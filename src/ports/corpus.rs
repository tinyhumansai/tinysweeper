//! Read-only access to the tree a pull request is changing.
//!
//! Always compiled. This is the port a reviewer's tools read through, and it is
//! deliberately the *narrowest* thing that answers the questions a reviewer
//! actually has: show me this file, and show me where else this appears.
//!
//! # Why this is not `Forge`
//!
//! [`crate::ports::forge::Forge`] can already read a file at a revision, and the
//! server path uses exactly that to implement this trait. But `Forge` is also
//! how a review comments, labels, approves and merges. Handing that trait to
//! the thing that executes model-chosen tool calls would put every write method
//! one slug away from a prompt injection, and "the invoker only matches two
//! slugs" is a property of today's `match` arm rather than of the type.
//!
//! This trait has no write method to reach. That is the entire reason it exists
//! as a separate port rather than a borrow of the forge.
//!
//! # Reading is not executing
//!
//! The security boundary allows this and nothing more: "we read the diff and
//! the tree; we do not build, install dependencies, or run the target
//! repository's scripts." Every implementation here reads. None spawns a
//! process against contributor code — the git-backed one runs `git`, which is
//! the operator's binary reading the operator's checkout, through the hardened
//! invocation in `crate::evidence::git`.

use async_trait::async_trait;

use crate::error::Result;

/// One line matched by [`Corpus::search`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    /// Repository-relative path of the file the line is in.
    pub path: String,
    /// 1-indexed line number.
    pub line: usize,
    /// The matching line, trimmed of trailing whitespace.
    pub text: String,
}

/// Read-only access to the tree under review.
#[async_trait]
pub trait Corpus: Send + Sync {
    /// Read `path` as it stands at the revision under review.
    ///
    /// `Ok(None)` for a path that does not exist. A reviewer guessing at a
    /// filename is the common case, not a failure, and an error would tell it
    /// to stop looking rather than to guess again.
    async fn read(&self, path: &str) -> Result<Option<String>>;

    /// Find lines matching `pattern`, as a literal substring.
    ///
    /// Literal rather than a regular expression on purpose: the pattern comes
    /// from a model, and a model-authored regex is one catastrophic backtrack
    /// away from hanging a review. Returns at most `limit` hits.
    ///
    /// A corpus that cannot search returns `Ok(None)` — distinct from
    /// `Ok(Some(vec![]))`, which means it searched and found nothing. A reviewer
    /// told "no matches" concludes something; one told "I cannot search here"
    /// concludes nothing, and those must not look the same.
    async fn search(&self, pattern: &str, limit: usize) -> Result<Option<Vec<Hit>>>;
}

/// A corpus over an in-memory map of path to content.
///
/// The offline mock every test uses, and also the real implementation on a path
/// that already holds file contents. Search is a literal scan, which is what
/// the trait promises rather than a degraded version of it.
#[derive(Debug, Default, Clone)]
pub struct MapCorpus {
    files: std::collections::BTreeMap<String, String>,
}

impl MapCorpus {
    /// Build a corpus over `files`.
    pub fn new(files: std::collections::BTreeMap<String, String>) -> Self {
        Self { files }
    }

    /// Add one file, for tests that build a corpus a line at a time.
    pub fn with(mut self, path: &str, content: &str) -> Self {
        self.files.insert(path.to_string(), content.to_string());
        self
    }

    /// Whether there is anything at all to read.
    ///
    /// A lane checks this before offering tools: a reviewer told it may read
    /// files, given a corpus with none, spends a round discovering that.
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }
}

#[async_trait]
impl Corpus for MapCorpus {
    async fn read(&self, path: &str) -> Result<Option<String>> {
        Ok(self.files.get(path).cloned())
    }

    async fn search(&self, pattern: &str, limit: usize) -> Result<Option<Vec<Hit>>> {
        // An empty pattern matches every line of every file, which is the whole
        // corpus pasted into a prompt. Treated as "found nothing" rather than
        // as an error: it is a malformed question, and the reviewer's next move
        // should be to ask a better one.
        if pattern.is_empty() {
            return Ok(Some(Vec::new()));
        }

        let mut hits = Vec::new();
        for (path, content) in &self.files {
            for (index, text) in content.lines().enumerate() {
                if hits.len() >= limit {
                    return Ok(Some(hits));
                }
                if text.contains(pattern) {
                    hits.push(Hit {
                        path: path.clone(),
                        line: index + 1,
                        text: text.trim_end().to_string(),
                    });
                }
            }
        }
        Ok(Some(hits))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn corpus() -> MapCorpus {
        MapCorpus::default()
            .with("src/a.rs", "fn one() {}\nfn two() {}\n")
            .with("src/b.rs", "fn three() {}\n")
    }

    #[tokio::test]
    async fn a_missing_path_reads_as_absent_rather_than_an_error() {
        assert_eq!(corpus().read("nope.rs").await.unwrap(), None);
    }

    #[tokio::test]
    async fn search_reports_the_path_and_the_line_number() {
        let hits = corpus().search("fn two", 10).await.unwrap().unwrap();

        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].path, "src/a.rs");
        assert_eq!(hits[0].line, 2, "1-indexed, as an editor counts");
    }

    #[tokio::test]
    async fn the_limit_is_honoured_across_files_not_per_file() {
        // Per-file would make the cap meaningless on a large repository, which
        // is the case it exists for.
        let hits = corpus().search("fn ", 2).await.unwrap().unwrap();
        assert_eq!(hits.len(), 2);
    }

    #[tokio::test]
    async fn an_empty_pattern_matches_nothing_rather_than_everything() {
        let hits = corpus().search("", 10).await.unwrap().unwrap();
        assert!(hits.is_empty(), "an empty pattern would return the corpus");
    }

    #[tokio::test]
    async fn searching_and_finding_nothing_differs_from_being_unable_to_search() {
        // `Some(vec![])` lets a reviewer conclude "this appears nowhere else".
        // `None` must not, and the types are what keep those apart.
        assert_eq!(corpus().search("zzz", 10).await.unwrap(), Some(Vec::new()));
    }
}
