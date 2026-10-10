//! Optional repository exploration beyond file ranges and literal search.

use super::{Found, Hit, Lookup, MAX_SEARCH_HITS, TreeReader};
use crate::error::Result;

/// Additional bounded, read-only repository operations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TreeQuery {
    /// List file paths at the reviewed snapshot, under a relative directory.
    List {
        /// Relative directory, or `.` for the root.
        path: String,
        /// Maximum paths, from 1 to 200.
        limit: u32,
    },
    /// Find literal symbol occurrences or host-owned graph evidence.
    Symbol {
        /// Nonempty literal symbol.
        symbol: String,
        /// Maximum hits, from 1 to 200.
        limit: u32,
    },
    /// Read an inclusive file range at a full immutable commit ID.
    History {
        /// Full SHA-1 or SHA-256 hexadecimal commit ID.
        commit: String,
        /// Normalized relative file path.
        path: String,
        /// First line, at least one.
        start: u32,
        /// Last line; bounded to the normal reader cap.
        end: u32,
    },
}
impl TreeQuery {
    pub(crate) fn valid(&self) -> bool {
        match self {
            Self::List { path, limit } => valid_path(path, true) && (1..=200).contains(limit),
            Self::Symbol { symbol, limit } => {
                !symbol.trim().is_empty()
                    && symbol.len() <= 1024
                    && !symbol.chars().any(char::is_control)
                    && (1..=200).contains(limit)
            }
            Self::History {
                commit,
                path,
                start,
                end,
            } => {
                matches!(commit.len(), 40 | 64)
                    && commit.bytes().all(|b| b.is_ascii_hexdigit())
                    && valid_path(path, false)
                    && *start > 0
                    && end >= start
                    && end - start < super::MAX_READ_LINES
            }
        }
    }
    pub(crate) fn key(&self) -> String {
        match self {
            Self::List { path, limit } => format!("list:{path}:{limit}"),
            Self::Symbol { symbol, limit } => format!("symbol:{symbol}:{limit}"),
            Self::History {
                commit,
                path,
                start,
                end,
            } => format!("history:{commit}:{path}:{start}:{end}"),
        }
    }
}
pub(crate) fn valid_path(path: &str, root: bool) -> bool {
    (root && path == ".")
        || (!path.is_empty()
            && path.len() <= 4096
            && !path.starts_with('/')
            && !path
                .chars()
                .any(|c| c.is_control() || matches!(c, '\\' | ':' | '~'))
            && path.split('/').all(|part| {
                !part.is_empty()
                    && part != "."
                    && part != ".."
                    && !part.eq_ignore_ascii_case(".git")
            }))
}
pub(crate) fn visible_path(path: &str) -> bool {
    valid_path(path, false)
        && !crate::scan::is_sensitive_path(path)
        && crate::scan::redact_line(path) == path
}
pub(crate) fn unavailable() -> Found {
    Found::Unavailable {
        reason: "this repository operation is unavailable on this snapshot".into(),
    }
}
/// List only safe file metadata, never contents or sensitive filenames.
pub(crate) fn paths(
    mut paths: Vec<String>,
    path: &str,
    limit: u32,
    already_truncated: bool,
) -> Found {
    paths.retain(|candidate| {
        visible_path(candidate)
            && (path == "." || candidate == path || candidate.starts_with(&format!("{path}/")))
    });
    paths.sort();
    paths.dedup();
    let truncated = already_truncated || paths.len() > limit as usize;
    paths.truncate(limit as usize);
    Found::Hits {
        hits: paths
            .into_iter()
            .map(|path| Hit {
                path,
                line: 1,
                text: "repository file (listing metadata, not source)".into(),
            })
            .collect(),
        truncated,
        skipped: Vec::new(),
    }
}
pub(crate) async fn symbol(tree: &dyn TreeReader, symbol: &str, limit: u32) -> Result<Found> {
    let found = tree
        .lookup(&Lookup::Search {
            pattern: symbol.into(),
            glob: None,
        })
        .await?;
    Ok(bound(found, limit.min(MAX_SEARCH_HITS as u32)))
}
pub(crate) fn bound(found: Found, limit: u32) -> Found {
    match found {
        Found::Hits {
            mut hits,
            mut truncated,
            skipped,
        } => {
            hits.retain(|hit| visible_path(&hit.path));
            truncated |= hits.len() > limit as usize;
            hits.truncate(limit as usize);
            Found::Hits {
                hits,
                truncated,
                skipped: skipped
                    .into_iter()
                    .filter(|path| visible_path(path))
                    .collect(),
            }
        }
        other => other,
    }
}
