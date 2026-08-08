//! Mapping a path to a language, and the extension allowlist.
//!
//! Always compiled. [`Language`] names a *grammar* we may have, not a family of
//! files: TSX and TypeScript are separate because tree-sitter ships two
//! grammars and parsing one with the other fails on the first JSX element.
//!
//! The allowlist is separate from the grammar list on purpose. Plenty of files
//! are worth retrieving — Markdown, TOML, SQL, Java — that we cannot parse. They
//! are indexed through the line splitter and say so in their metadata, which is
//! strictly better than being invisible.

use std::path::Path;

/// A language the chunker recognises.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Language {
    /// Rust.
    Rust,
    /// TypeScript without JSX.
    TypeScript,
    /// TypeScript with JSX.
    Tsx,
    /// JavaScript, including JSX — one grammar covers both.
    JavaScript,
    /// Python.
    Python,
    /// Go.
    Go,
}

impl Language {
    /// The language of `path`, when there is a grammar for it.
    ///
    /// `None` means the line splitter handles the file, not that the file is
    /// uninteresting — see [`is_indexable`].
    pub fn from_path(path: &str) -> Option<Self> {
        match extension(path).as_str() {
            "rs" => Some(Self::Rust),
            "ts" | "mts" | "cts" => Some(Self::TypeScript),
            "tsx" => Some(Self::Tsx),
            "js" | "mjs" | "cjs" | "jsx" => Some(Self::JavaScript),
            "py" | "pyi" => Some(Self::Python),
            "go" => Some(Self::Go),
            _ => None,
        }
    }

    /// The stable name written onto a chunk's `lang` field.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Rust => "rust",
            Self::TypeScript => "typescript",
            Self::Tsx => "tsx",
            Self::JavaScript => "javascript",
            Self::Python => "python",
            Self::Go => "go",
        }
    }

    /// Every language with a grammar, for tests and for `--help` text.
    pub const ALL: [Language; 6] = [
        Language::Rust,
        Language::TypeScript,
        Language::Tsx,
        Language::JavaScript,
        Language::Python,
        Language::Go,
    ];
}

/// The lowercase extension of `path`, without the dot. Empty when there is none.
pub fn extension(path: &str) -> String {
    Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
}

/// The label written onto a chunk when no grammar claims the file.
///
/// The extension itself, so a retrieval hit in a `.sql` file still says `sql`.
/// `None` for an extensionless file rather than a guess.
pub fn fallback_label(path: &str) -> Option<String> {
    let extension = extension(path);
    (!extension.is_empty()).then_some(extension)
}

/// Extensions indexed by default, beyond the ones with grammars.
///
/// Kept as a list rather than "everything that is not binary" because an
/// allowlist fails towards indexing too little, and indexing too much is the
/// expensive direction: minified bundles, lockfiles and generated clients are
/// enormous, embed for real money, and are never the answer to a review query.
pub const EXTRA_EXTENSIONS: &[&str] = &[
    "c", "cc", "cpp", "cs", "h", "hpp", "java", "kt", "kts", "swift", "rb", "php", "scala", "sh",
    "bash", "zsh", "sql", "proto", "graphql", "gql", "tf", "md", "mdx", "rst", "toml", "yaml",
    "yml", "json", "css", "scss", "html", "vue", "svelte", "lua", "ex", "exs", "erl", "hs", "ml",
    "r", "pl", "dart", "zig",
];

/// Whether a path's extension is on the allowlist.
pub fn is_indexable(path: &str) -> bool {
    if Language::from_path(path).is_some() {
        return true;
    }
    let extension = extension(path);
    !extension.is_empty() && EXTRA_EXTENSIONS.contains(&extension.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tsx_is_not_typescript() {
        // Separate grammars: parsing TSX with the TypeScript grammar errors on
        // the first JSX element, and the chunker would silently fall back.
        assert_eq!(Language::from_path("a/b.tsx"), Some(Language::Tsx));
        assert_eq!(Language::from_path("a/b.ts"), Some(Language::TypeScript));
    }

    #[test]
    fn every_language_maps_back_from_at_least_one_extension() {
        for language in Language::ALL {
            let found = ["rs", "ts", "tsx", "js", "py", "go"]
                .iter()
                .filter_map(|e| Language::from_path(&format!("f.{e}")))
                .any(|l| l == language);
            assert!(found, "{language:?} is unreachable from any extension");
        }
    }

    #[test]
    fn an_unparsed_but_indexable_file_still_gets_a_language_label() {
        assert!(Language::from_path("schema.sql").is_none());
        assert!(is_indexable("schema.sql"));
        assert_eq!(fallback_label("schema.sql").as_deref(), Some("sql"));
    }

    #[test]
    fn an_extensionless_file_is_neither_indexable_nor_labelled() {
        assert!(!is_indexable("Makefile"));
        assert_eq!(fallback_label("Makefile"), None);
        assert_eq!(extension("Makefile"), "");
    }

    #[test]
    fn extensions_are_matched_case_insensitively() {
        assert_eq!(Language::from_path("A.RS"), Some(Language::Rust));
        assert!(is_indexable("READ.MD"));
    }
}
