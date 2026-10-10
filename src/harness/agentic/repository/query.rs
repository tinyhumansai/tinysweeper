//! Typed repository requests and lexical validation before host dispatch.

use serde::{Deserialize, Serialize};

pub(super) const MAX_RESULTS: u32 = 200;
pub(super) const MAX_LINES: u32 = 1_000;
pub(super) const MAX_PATH_BYTES: usize = 4_096;
pub(super) const MAX_QUERY_BYTES: usize = 1_024;

/// A read-only repository request; range ends are inclusive and lines are one-based.
///
/// The toolset validates these values before dispatch. Hosts constructing queries
/// themselves remain responsible for their own validation and repository containment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum RepositoryQuery {
    /// List directory or tree entries; `.` denotes the repository root.
    List {
        /// Normalized repository-relative directory path.
        path: String,
        /// Maximum entries, from 1 to 200.
        limit: u32,
    },
    /// Read an inclusive range from the host's current snapshot.
    Read {
        /// Normalized repository-relative file path.
        path: String,
        /// First line, at least 1.
        start_line: u32,
        /// Last line, bounding the inclusive range to 1,000 lines.
        end_line: u32,
    },
    /// Search for literal text beneath a directory or file path.
    Search {
        /// Normalized relative path, or `.` for the repository root.
        path: String,
        /// Nonempty literal search text, at most 1,024 UTF-8 bytes.
        query: String,
        /// Maximum hits, from 1 to 200.
        limit: u32,
    },
    /// Look up a symbol or its graph relationships in a host-owned index.
    Lookup {
        /// Nonempty symbol/graph lookup term, at most 1,024 UTF-8 bytes.
        symbol: String,
        /// Maximum results, from 1 to 200.
        limit: u32,
    },
    /// Read a file range at an immutable commit, without executing git.
    GitShow {
        /// Full 40- or 64-character ASCII hexadecimal object ID.
        commit: String,
        /// Normalized repository-relative file path.
        path: String,
        /// First line, at least 1.
        start_line: u32,
        /// Last line, bounding the inclusive range to 1,000 lines.
        end_line: u32,
    },
}

impl RepositoryQuery {
    pub(super) fn valid(&self) -> bool {
        match self {
            Self::List { path, limit } => valid_path(path, true) && valid_limit(*limit),
            Self::Read {
                path,
                start_line,
                end_line,
            } => valid_path(path, false) && valid_range(*start_line, *end_line),
            Self::Search { path, query, limit } => {
                valid_path(path, true) && valid_term(query) && valid_limit(*limit)
            }
            Self::Lookup { symbol, limit } => valid_term(symbol) && valid_limit(*limit),
            Self::GitShow {
                commit,
                path,
                start_line,
                end_line,
            } => {
                matches!(commit.len(), 40 | 64)
                    && commit.bytes().all(|c| c.is_ascii_hexdigit())
                    && valid_path(path, false)
                    && valid_range(*start_line, *end_line)
            }
        }
    }
}

fn valid_path(path: &str, root_allowed: bool) -> bool {
    if root_allowed && path == "." {
        return true;
    }
    !path.is_empty()
        && path.len() <= MAX_PATH_BYTES
        && !path.starts_with('/')
        && !path
            .chars()
            .any(|c| c.is_control() || matches!(c, '\\' | ':' | '~'))
        && path.split('/').all(|part| {
            !part.is_empty() && part != "." && part != ".." && !part.eq_ignore_ascii_case(".git")
        })
}

fn valid_limit(limit: u32) -> bool {
    (1..=MAX_RESULTS).contains(&limit)
}

fn valid_term(term: &str) -> bool {
    !term.trim().is_empty() && term.len() <= MAX_QUERY_BYTES && !term.chars().any(char::is_control)
}

fn valid_range(start: u32, end: u32) -> bool {
    start > 0 && end >= start && end - start < MAX_LINES
}
