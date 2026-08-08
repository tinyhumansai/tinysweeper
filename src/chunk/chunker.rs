//! The chunker: pick spans, hash them, hand back indexable chunks.
//!
//! Always compiled. With `treesitter` on it tries the grammar first and falls
//! back to the line splitter; with it off every file takes the fallback. Either
//! way the resulting chunk says which happened, so nothing downstream has to
//! infer it from the build flags.

use sha2::{Digest, Sha256};

use crate::chunk::lang::{self, Language};
use crate::chunk::lines;
use crate::chunk::types::{ChunkOptions, SkipReason, SourceChunk};
use crate::index::types::Chunk;

/// Turns a file's text into chunks.
#[derive(Debug, Clone, Default)]
pub struct Chunker {
    options: ChunkOptions,
}

impl Chunker {
    /// A chunker with the default sizes.
    pub fn new() -> Self {
        Self::default()
    }

    /// A chunker with caller-chosen sizes.
    pub fn with_options(options: ChunkOptions) -> Self {
        Self { options }
    }

    /// The sizes this chunker was built with.
    pub fn options(&self) -> ChunkOptions {
        self.options
    }

    /// Choose the spans of one file.
    ///
    /// The grammar is tried first and the line splitter is the fallback, not a
    /// competitor: a file the grammar declines is still worth indexing, it just
    /// does not get to claim its chunks contain whole definitions.
    pub fn spans(&self, path: &str, source: &str) -> Vec<SourceChunk> {
        #[cfg(feature = "treesitter")]
        if let Some(language) = Language::from_path(path)
            && let Some(chunks) = crate::chunk::tree::split(source, language, &self.options)
        {
            return chunks;
        }

        // Referenced so the `treesitter`-off build does not warn about an
        // unused import, and so the two builds read the same.
        let _ = Language::from_path(path);
        lines::split(source, 1, &self.options)
    }

    /// Chunk one file into index-ready values.
    pub fn chunk(&self, repo_id: &str, path: &str, source: &str) -> Vec<Chunk> {
        let language = Language::from_path(path).map(|l| l.as_str().to_string());
        // A file no grammar claims still gets a language label from its
        // extension. Reporting `None` would make every `.sql` hit look like a
        // file of unknown type rather than one we chose not to parse.
        let label = language.or_else(|| lang::fallback_label(path));

        self.spans(path, source)
            .into_iter()
            .map(|span| Chunk {
                repo_id: repo_id.to_string(),
                path: path.to_string(),
                start_line: span.start_line,
                end_line: span.end_line,
                content_hash: content_hash(&span.text),
                text: span.text,
                lang: label.clone(),
                symbol: span.symbol,
                chunked_by: span.method,
            })
            .collect()
    }

    /// Chunk a file's raw bytes, or say why it could not be.
    ///
    /// The UTF-8 check lives here rather than in the selector because it needs
    /// the content, and the selector deliberately works from paths and sizes so
    /// it can run against a diff without a checkout.
    pub fn chunk_bytes(
        &self,
        repo_id: &str,
        path: &str,
        bytes: &[u8],
    ) -> std::result::Result<Vec<Chunk>, SkipReason> {
        let source = std::str::from_utf8(bytes).map_err(|_| SkipReason::NotText)?;
        if source.trim().is_empty() {
            return Err(SkipReason::Empty);
        }
        Ok(self.chunk(repo_id, path, source))
    }
}

/// The hash stored on a chunk, and the thing incremental re-indexing turns on.
///
/// Over the chunk text alone: the same span at a new line number in an
/// otherwise untouched file is the same content and must not be re-embedded.
/// The path and line number are already in
/// [`Chunk::id`](crate::index::Chunk::id), so identity stays distinct without
/// making the hash sensitive to a change that costs nothing to keep.
///
/// Sixteen bytes of SHA-256, hand-hexed for the same reason as
/// `Finding::fingerprint`: sha2 0.11 returns an array with no `LowerHex`, and a
/// hex crate is not worth a dependency in the offline default build.
pub fn content_hash(text: &str) -> String {
    Sha256::digest(text.as_bytes())
        .iter()
        .take(16)
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::types::ChunkMethod;

    #[test]
    fn identical_text_hashes_identically_and_different_text_does_not() {
        // The whole incremental path rests on this: an unchanged chunk must
        // hash to the same value on the next push, or every re-index re-embeds
        // the repository.
        assert_eq!(content_hash("fn a() {}"), content_hash("fn a() {}"));
        assert_ne!(content_hash("fn a() {}"), content_hash("fn b() {}"));
        assert_eq!(content_hash("x").len(), 32);
    }

    #[test]
    fn a_chunk_carries_its_repo_path_lines_and_hash() {
        let chunks = Chunker::new().chunk("o/r", "src/a.rs", "fn a() {}\nfn b() {}\n");
        assert!(!chunks.is_empty());
        for chunk in &chunks {
            assert_eq!(chunk.repo_id, "o/r");
            assert_eq!(chunk.path, "src/a.rs");
            assert_eq!(chunk.lang.as_deref(), Some("rust"));
            assert!(chunk.start_line >= 1 && chunk.end_line >= chunk.start_line);
            assert_eq!(chunk.content_hash, content_hash(&chunk.text));
        }
    }

    #[test]
    fn a_file_no_grammar_claims_is_labelled_by_extension_and_marked_unparsed() {
        let chunks = Chunker::new().chunk("o/r", "db/schema.sql", "SELECT 1;\n");
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].lang.as_deref(), Some("sql"));
        assert_eq!(
            chunks[0].chunked_by,
            ChunkMethod::Lines,
            "an unparsed chunk must not present itself as parsed"
        );
        assert_eq!(chunks[0].symbol, None);
    }

    #[test]
    fn non_utf8_bytes_are_a_reported_skip_rather_than_a_panic() {
        let err = Chunker::new()
            .chunk_bytes("o/r", "a.rs", &[0xff, 0xfe, 0x00])
            .expect_err("not text");
        assert_eq!(err, SkipReason::NotText);
    }

    #[test]
    fn a_whitespace_only_file_is_skipped_as_empty() {
        let err = Chunker::new()
            .chunk_bytes("o/r", "a.rs", b"\n\n   \n")
            .expect_err("empty");
        assert_eq!(err, SkipReason::Empty);
    }

    #[test]
    fn chunking_is_deterministic() {
        // Re-indexing the same tree twice must produce byte-identical ids, or
        // the "unchanged file costs nothing" property is unprovable.
        let source = (1..=200).map(|i| format!("fn f{i}() {{}}\n")).collect::<String>();
        let first = Chunker::new().chunk("o/r", "src/a.rs", &source);
        let second = Chunker::new().chunk("o/r", "src/a.rs", &source);
        assert_eq!(first, second);
    }

    #[cfg(feature = "treesitter")]
    #[test]
    fn a_parsed_file_reports_parsed_chunks() {
        let chunks = Chunker::new().chunk("o/r", "src/a.rs", "fn only() {\n    1\n}\n");
        assert_eq!(chunks[0].chunked_by, ChunkMethod::Parsed);
        assert_eq!(chunks[0].symbol.as_deref(), Some("only"));
    }
}
