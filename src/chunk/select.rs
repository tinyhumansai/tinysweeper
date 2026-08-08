//! Deciding which files to index, and reporting the ones left out.
//!
//! Always compiled.
//!
//! The reporting half is not politeness. A chunker that silently drops
//! everything over some size cap makes a large hand-written service file
//! invisible to review permanently, with no warning anywhere: the reviewer
//! simply never sees that code and nobody can tell the difference between
//! "there was nothing to say" and "we never looked". So a skip is a value —
//! [`SkippedFile`] — carried out of the selector, and
//! [`Selection::report`](crate::chunk::types::Selection::report) turns the
//! surprising ones into text a human is shown.

use std::path::Path;

use globset::{Glob, GlobSet, GlobSetBuilder};

use crate::chunk::lang;
use crate::chunk::types::{Selection, SkipReason, SkippedFile};
use crate::error::{Error, Result};

/// The default per-file ceiling, in bytes.
///
/// A megabyte is far above any hand-written source file and far below a
/// minified bundle or a generated client, which is the line the cap is trying
/// to draw. It is deliberately much larger than the 100 KB that a
/// silently-skipping chunker gets away with, because here every skip is
/// reported and a cap that fires often is a cap that produces noise.
pub const DEFAULT_MAX_BYTES: u64 = 1_048_576;

/// Build a matcher from gitignore-style globs.
///
/// Shared with the review path so a repository's `paths.ignore` means exactly
/// the same thing to the indexer as it does to the lanes — two spellings of
/// "ignored" would be a config that behaves differently depending on which half
/// of the product is reading it.
pub fn ignore_globs(patterns: &[String]) -> Result<GlobSet> {
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        builder.add(
            Glob::new(pattern)
                .map_err(|err| Error::config(format!("invalid ignore glob `{pattern}`: {err}")))?,
        );
    }
    builder
        .build()
        .map_err(|err| Error::config(format!("could not build the ignore set: {err}")))
}

/// Decides which files are worth indexing.
#[derive(Debug, Clone)]
pub struct Selector {
    ignored: GlobSet,
    // Kept alongside the compiled set so a skip can name the glob that caused
    // it. `GlobSet` reports matching indices, not patterns, and "ignored by
    // some rule" is not an actionable thing to tell someone.
    patterns: Vec<String>,
    max_bytes: u64,
}

impl Selector {
    /// A selector honouring `patterns` with the default size cap.
    pub fn new(patterns: &[String]) -> Result<Self> {
        Ok(Self {
            ignored: ignore_globs(patterns)?,
            patterns: patterns.to_vec(),
            max_bytes: DEFAULT_MAX_BYTES,
        })
    }

    /// Override the per-file size cap.
    pub fn max_bytes(mut self, max_bytes: u64) -> Self {
        self.max_bytes = max_bytes;
        self
    }

    /// Why `path` at `bytes` bytes would not be indexed, if it would not be.
    ///
    /// Ordered cheapest-first, and the order is also the most useful one to
    /// report: being ignored explains a file better than its extension does.
    pub fn reject(&self, path: &str, bytes: u64) -> Option<SkipReason> {
        if let Some(index) = self.ignored.matches(path).first() {
            return Some(SkipReason::Ignored {
                glob: self
                    .patterns
                    .get(*index)
                    .cloned()
                    .unwrap_or_else(|| "?".to_string()),
            });
        }
        if !lang::is_indexable(path) {
            return Some(SkipReason::UnsupportedExtension {
                extension: lang::extension(path),
            });
        }
        if bytes == 0 {
            return Some(SkipReason::Empty);
        }
        if bytes > self.max_bytes {
            return Some(SkipReason::TooLarge {
                bytes,
                cap: self.max_bytes,
            });
        }
        None
    }

    /// Sort `files` — path and size in bytes — into kept and skipped.
    pub fn select<I>(&self, files: I) -> Selection
    where
        I: IntoIterator<Item = (String, u64)>,
    {
        let mut selection = Selection::default();
        for (path, bytes) in files {
            match self.reject(&path, bytes) {
                Some(reason) => selection.skipped.push(SkippedFile { path, reason }),
                None => selection.selected.push(path),
            }
        }
        selection
    }

    /// Walk a checkout and select from it.
    ///
    /// Paths come back repo-relative and forward-slashed, matching
    /// [`Chunk::path`](crate::index::Chunk), so the same string is what the diff
    /// and the index both talk about. An unreadable entry is *reported*, not
    /// skipped and not fatal: one bad symlink must not abort indexing a
    /// repository, and must not vanish either.
    pub fn walk(&self, root: &Path) -> Result<Selection> {
        let mut files = Vec::new();
        let mut unreadable = Vec::new();
        gather(root, root, &mut files, &mut unreadable)?;
        // Sorted so a re-index of an unchanged tree produces an identical plan,
        // which is what makes the incremental path's diffing meaningful.
        files.sort();

        let mut selection = self.select(files);
        selection.skipped.extend(unreadable);
        Ok(selection)
    }
}

/// Directories never worth descending into.
///
/// `.git` is not source, and walking it on a large repository costs more than
/// everything else here put together. The rest are conventional build output;
/// a repository that wants them indexed can say so by not ignoring them, but
/// nobody ever has.
const SKIPPED_DIRS: &[&str] = &[".git", "node_modules", "target", "vendor", ".venv", "dist"];

fn gather(
    root: &Path,
    directory: &Path,
    files: &mut Vec<(String, u64)>,
    unreadable: &mut Vec<SkippedFile>,
) -> Result<()> {
    let entries = match std::fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(err) => {
            unreadable.push(SkippedFile {
                path: relative(root, directory),
                reason: SkipReason::Unreadable {
                    message: err.to_string(),
                },
            });
            return Ok(());
        }
    };

    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        // `symlink_metadata`, so a symlink is judged as itself rather than as
        // whatever it points at: following one out of the checkout would index
        // files from outside the repository.
        let metadata = match std::fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(err) => {
                unreadable.push(SkippedFile {
                    path: relative(root, &path),
                    reason: SkipReason::Unreadable {
                        message: err.to_string(),
                    },
                });
                continue;
            }
        };

        if metadata.is_dir() {
            let name = entry.file_name();
            if SKIPPED_DIRS.iter().any(|d| name == *d) {
                continue;
            }
            gather(root, &path, files, unreadable)?;
        } else if metadata.is_file() {
            files.push((relative(root, &path), metadata.len()));
        }
    }
    Ok(())
}

fn relative(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn selector() -> Selector {
        Selector::new(&["docs/**".to_string(), "*.lock".to_string()]).expect("valid globs")
    }

    #[test]
    fn an_oversized_file_is_skipped_with_its_size_and_the_cap() {
        let selector = selector().max_bytes(1_000);
        let selection = selector.select([
            ("src/small.rs".to_string(), 900_u64),
            ("src/huge.rs".to_string(), 4_000_u64),
        ]);

        assert_eq!(selection.selected, vec!["src/small.rs".to_string()]);
        assert_eq!(
            selection.skipped,
            vec![SkippedFile {
                path: "src/huge.rs".into(),
                reason: SkipReason::TooLarge {
                    bytes: 4_000,
                    cap: 1_000,
                },
            }]
        );
        let report = selection.report().expect("reported, never silent");
        assert!(report.contains("src/huge.rs"), "{report}");
    }

    #[test]
    fn an_ignored_path_names_the_glob_that_ignored_it() {
        let selection = selector().select([("docs/guide.md".to_string(), 10_u64)]);
        assert_eq!(
            selection.skipped[0].reason,
            SkipReason::Ignored {
                glob: "docs/**".into()
            }
        );
        // The operator asked for this one, so it is not reported back at them.
        assert!(selection.report().is_none());
    }

    #[test]
    fn ignoring_beats_the_extension_allowlist() {
        // Both apply to `Cargo.lock`; the ignore glob is the more useful answer.
        let selection = selector().select([("Cargo.lock".to_string(), 10_u64)]);
        assert!(matches!(
            selection.skipped[0].reason,
            SkipReason::Ignored { .. }
        ));
    }

    #[test]
    fn unsupported_extensions_and_empty_files_are_skipped() {
        let selection = selector().select([
            ("assets/logo.png".to_string(), 10_u64),
            ("src/empty.rs".to_string(), 0_u64),
        ]);
        assert_eq!(selection.selected.len(), 0);
        assert!(matches!(
            selection.skipped[0].reason,
            SkipReason::UnsupportedExtension { .. }
        ));
        assert_eq!(selection.skipped[1].reason, SkipReason::Empty);
    }

    #[test]
    fn an_invalid_glob_is_a_config_error_naming_the_pattern() {
        let err = Selector::new(&["src/[".to_string()])
            .expect_err("rejected")
            .to_string();
        assert!(err.contains("src/["), "{err}");
    }

    #[test]
    fn walking_a_checkout_returns_forward_slashed_relative_paths() {
        let root = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(root.path().join("src/deep")).expect("dirs");
        std::fs::create_dir_all(root.path().join(".git")).expect("dirs");
        std::fs::write(root.path().join("src/deep/a.rs"), "fn a() {}\n").expect("write");
        std::fs::write(root.path().join(".git/config"), "x").expect("write");

        let selection = Selector::new(&[]).expect("globs").walk(root.path()).expect("walks");
        assert_eq!(selection.selected, vec!["src/deep/a.rs".to_string()]);
    }
}
