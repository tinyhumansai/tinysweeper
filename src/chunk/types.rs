//! The domain types the chunker speaks in.
//!
//! Always compiled, and free of tree-sitter: the fallback splitter produces the
//! same [`SourceChunk`] the grammar path does, so nothing downstream needs to
//! know whether the `treesitter` feature was on — it only needs to read
//! [`SourceChunk::method`].

use serde::{Deserialize, Serialize};

use crate::index::types::ChunkMethod;

/// A span chosen out of one file, before it is hashed and embedded.
///
/// Line numbers are 1-based and inclusive at both ends, matching
/// [`Chunk`](crate::index::Chunk) and the evidence module, so a retrieval hit is
/// quotable at a diff line with no translation step in between.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceChunk {
    /// First line of the span, 1-based inclusive.
    pub start_line: u32,
    /// Last line of the span, 1-based inclusive.
    pub end_line: u32,
    /// The span's text, verbatim from the file.
    pub text: String,
    /// The definition this span is, when it is exactly one named definition.
    ///
    /// Deliberately `None` for a span that merged several small definitions:
    /// naming one of three functions would read as "this chunk is that
    /// function", which is the kind of confident-but-wrong metadata the module
    /// exists to avoid.
    pub symbol: Option<String>,
    /// Whether a grammar or the line splitter chose the boundaries.
    pub method: ChunkMethod,
}

/// How big a chunk may get, and when a definition stops being kept whole.
///
/// Three limits, because they answer three different questions.
///
/// `target_chars` is the size a chunk is aimed at — small enough that a hit is
/// specific, large enough to carry a function and its doc comment.
///
/// `max_chars` is the point past which keeping a definition whole stops being
/// worth it: a definition larger than this is split on lines and *says* so,
/// rather than being stored as one enormous parsed chunk.
///
/// `max_embed_bytes` is not a preference at all — it is the provider's own
/// per-input limit, and exceeding it fails the call. It is separate from
/// `max_chars` because the two are answerable by different people: `max_chars`
/// is a retrieval-quality judgement a deployment may tune, while this one is
/// dictated by whichever embedding model is configured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChunkOptions {
    /// The size a chunk is aimed at, in characters.
    pub target_chars: usize,
    /// The hard ceiling past which even a single definition is split on lines.
    pub max_chars: usize,
    /// Hard ceiling on one chunk's bytes, from the embedder's per-input limit.
    ///
    /// See [`DEFAULT_MAX_EMBED_BYTES`].
    pub max_embed_bytes: usize,
}

/// Hard ceiling on one chunk's bytes, from the embedding provider's per-input
/// token limit.
///
/// An earlier version of this module assumed an embedder "truncates its input
/// silently". It does not: OpenAI rejects the whole request with
/// `Invalid 'input[19]': maximum input length is 8192 tokens`, which fails the
/// entire batch and leaves the repository unindexed. That is how the largest
/// repository in the fleet kept reviewing from the diff alone after the
/// batch-level token ceiling was already fixed.
///
/// The provider's limit is 8,192 tokens **per input**. Dense code measured at
/// roughly 1.8 bytes per token — the same measurement behind
/// [`DEFAULT_MAX_BATCH_TOKENS`](crate::indexer::run::DEFAULT_MAX_BATCH_TOKENS)
/// — putting 8,192 tokens at about 14,700 bytes. 12,000 leaves room for source
/// denser still, which is exactly the kind of file that trips this: minified
/// bundles, generated code, and long embedded literals.
pub const DEFAULT_MAX_EMBED_BYTES: usize = 12_000;

impl Default for ChunkOptions {
    fn default() -> Self {
        Self {
            // Roughly 400–500 tokens: comfortably inside every embedding
            // model's window, and small enough that a hit points at one thing.
            target_chars: 1_800,
            // Eight times the target. A definition that big is rare and
            // pathological; below it, keeping the body whole always wins.
            max_chars: 14_400,
            max_embed_bytes: DEFAULT_MAX_EMBED_BYTES,
        }
    }
}

impl ChunkOptions {
    /// Options with a caller-chosen target, keeping the ceiling proportional.
    ///
    /// `max_embed_bytes` is deliberately *not* scaled with the target: it
    /// describes the provider, not the caller's taste, so a caller asking for
    /// bigger chunks does not get permission to exceed the provider's limit.
    pub fn with_target(target_chars: usize) -> Self {
        let target_chars = target_chars.max(1);
        Self {
            target_chars,
            max_chars: target_chars.saturating_mul(8),
            max_embed_bytes: DEFAULT_MAX_EMBED_BYTES,
        }
    }

    /// The size past which a span must be broken up, whatever the reason.
    ///
    /// Whichever ceiling is lower wins: a deployment that raises `max_chars`
    /// above the provider's per-input limit must not thereby produce chunks
    /// the provider will reject.
    pub fn split_ceiling(&self) -> usize {
        self.max_chars.min(self.max_embed_bytes)
    }
}

/// Why a file was left out of the index.
///
/// Every variant carries the numbers behind the decision. A skip that could not
/// explain itself is indistinguishable from a file that was never there, which
/// is how a large hand-written service file ends up invisible to review forever
/// with nobody noticing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum SkipReason {
    /// The path matched one of `config.paths.ignore`.
    Ignored {
        /// The glob that matched, so the fix is a one-line config edit.
        glob: String,
    },
    /// The extension is not on the allowlist.
    UnsupportedExtension {
        /// The extension seen, without the dot. Empty for an extensionless file.
        extension: String,
    },
    /// The file is larger than the cap.
    TooLarge {
        /// Its size in bytes.
        bytes: u64,
        /// The cap it exceeded.
        cap: u64,
    },
    /// The file is not valid UTF-8, so there is no text to embed.
    NotText,
    /// The file has no content worth indexing.
    Empty,
    /// The file could not be read.
    Unreadable {
        /// What the filesystem said.
        message: String,
    },
}

impl SkipReason {
    /// A one-line explanation, for a check-run summary or a log line.
    pub fn explain(&self) -> String {
        match self {
            Self::Ignored { glob } => format!("ignored by `{glob}`"),
            Self::UnsupportedExtension { extension } if extension.is_empty() => {
                "no file extension".to_string()
            }
            Self::UnsupportedExtension { extension } => {
                format!("`.{extension}` is not an indexed extension")
            }
            Self::TooLarge { bytes, cap } => format!("{bytes} bytes, over the {cap}-byte cap"),
            Self::NotText => "not valid UTF-8".to_string(),
            Self::Empty => "empty".to_string(),
            Self::Unreadable { message } => format!("unreadable: {message}"),
        }
    }

    /// Whether this skip is worth telling a human about.
    ///
    /// An ignore glob and an unsupported extension are the operator's own
    /// stated policy, so reporting them is noise. A file that was *meant* to be
    /// indexed and was not — too large, unreadable, not text — is the case that
    /// must never be silent.
    pub fn is_surprising(&self) -> bool {
        matches!(
            self,
            Self::TooLarge { .. } | Self::NotText | Self::Unreadable { .. }
        )
    }
}

/// One file that was not indexed, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkippedFile {
    /// The repo-relative, forward-slashed path.
    pub path: String,
    /// Why it was left out.
    pub reason: SkipReason,
}

/// What a pass of [`Selector`](crate::chunk::Selector) decided.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Selection {
    /// The paths that will be chunked, in the order they were offered.
    pub selected: Vec<String>,
    /// Everything left out, with a reason each.
    pub skipped: Vec<SkippedFile>,
}

impl Selection {
    /// The skips a human should see, in path order.
    ///
    /// See [`SkipReason::is_surprising`] for what "should see" means.
    pub fn surprising(&self) -> Vec<&SkippedFile> {
        self.skipped
            .iter()
            .filter(|s| s.reason.is_surprising())
            .collect()
    }

    /// A short report of the surprising skips, or `None` when there are none.
    ///
    /// Returned rather than logged so the caller can put it where it will be
    /// read — a check-run summary rather than a log nobody opens.
    pub fn report(&self) -> Option<String> {
        report_skips(&self.skipped)
    }
}

/// A short report of the surprising skips in `skipped`, or `None` when there
/// are none.
///
/// Free-standing because the indexer accumulates skips across many selections
/// and needs the same rendering without reassembling a [`Selection`] to get it.
pub fn report_skips(skipped: &[SkippedFile]) -> Option<String> {
    let surprising: Vec<&SkippedFile> = skipped
        .iter()
        .filter(|s| s.reason.is_surprising())
        .collect();
    if surprising.is_empty() {
        return None;
    }
    let mut lines = vec![format!("{} file(s) were not indexed:", surprising.len())];
    lines.extend(
        surprising
            .iter()
            .map(|s| format!("- `{}` — {}", s.path, s.reason.explain())),
    );
    Some(lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_oversized_file_is_reported_and_an_ignored_one_is_not() {
        // The asymmetry is the point: an ignore glob is the operator's own
        // decision, a size cap is one they never made.
        let selection = Selection {
            selected: vec!["src/a.rs".into()],
            skipped: vec![
                SkippedFile {
                    path: "vendor/big.rs".into(),
                    reason: SkipReason::TooLarge {
                        bytes: 400_000,
                        cap: 262_144,
                    },
                },
                SkippedFile {
                    path: "target/x.rs".into(),
                    reason: SkipReason::Ignored {
                        glob: "target/**".into(),
                    },
                },
            ],
        };

        let report = selection.report().expect("a surprising skip is reported");
        assert!(report.contains("vendor/big.rs"), "{report}");
        assert!(report.contains("400000 bytes"), "{report}");
        assert!(!report.contains("target/x.rs"), "{report}");
    }

    #[test]
    fn a_selection_with_nothing_surprising_reports_nothing() {
        let selection = Selection {
            selected: vec!["src/a.rs".into()],
            skipped: vec![SkippedFile {
                path: "README.bin".into(),
                reason: SkipReason::UnsupportedExtension {
                    extension: "bin".into(),
                },
            }],
        };
        assert!(selection.report().is_none());
    }

    #[test]
    fn an_extensionless_skip_does_not_say_dot_nothing() {
        let reason = SkipReason::UnsupportedExtension {
            extension: String::new(),
        };
        assert_eq!(reason.explain(), "no file extension");
    }

    #[test]
    fn the_ceiling_stays_above_the_target_for_any_target() {
        for target in [1, 100, 1_800, 100_000] {
            let options = ChunkOptions::with_target(target);
            assert!(options.max_chars >= options.target_chars, "{target}");
        }
    }
}
