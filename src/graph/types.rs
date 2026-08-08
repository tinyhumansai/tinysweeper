//! The core types of the repository graph: what we parse out of a file, what
//! we failed to resolve, and what the finished graph looks like.
//!
//! Always compiled. Nothing here touches tree-sitter or MongoDB — extraction
//! fills these in, resolution consumes them, and storage translates them into
//! the [`GraphNode`](crate::index::types::GraphNode) /
//! [`GraphEdge`](crate::index::types::GraphEdge) wire shapes the
//! [`GraphStore`](crate::ports::graph::GraphStore) port already speaks.

use serde::{Deserialize, Serialize};

use crate::index::types::{GraphEdge, GraphNode};

/// A language we can extract structure from.
///
/// Deliberately a closed enum rather than a string. Every variant here has a
/// grammar *and* a specifier resolver; a language with only one of the two
/// would produce a graph whose edges are silently missing, which is worse than
/// admitting we do not parse it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Language {
    /// Rust: `mod` / `use` paths against the crate root.
    Rust,
    /// Python: dotted module paths and explicit relative imports.
    Python,
    /// TypeScript proper (`.ts`, `.mts`, `.cts`).
    TypeScript,
    /// The JSX-bearing dialects (`.tsx`, `.jsx`, `.js`, `.mjs`, `.cjs`).
    ///
    /// Split from [`Language::TypeScript`] because they need a different
    /// grammar: TSX reads `<T>` as an element, plain TypeScript as a type
    /// parameter, and one grammar cannot serve both.
    Tsx,
    /// Go: package paths rooted at the `go.mod` module path.
    Go,
}

impl Language {
    /// Detect the language of a repo-relative path, or `None` if we do not
    /// parse it.
    pub fn from_path(path: &str) -> Option<Self> {
        // `.d.ts` before `.ts`: a declaration file is still TypeScript, but the
        // check has to come first or the suffix match picks the wrong arm for
        // nothing. Kept explicit so the ordering is not accidental.
        let lower = path.to_ascii_lowercase();
        let ext = lower.rsplit_once('.').map(|(_, e)| e)?;
        match ext {
            "rs" => Some(Self::Rust),
            "py" | "pyi" => Some(Self::Python),
            "ts" | "mts" | "cts" => Some(Self::TypeScript),
            "tsx" | "jsx" | "js" | "mjs" | "cjs" => Some(Self::Tsx),
            "go" => Some(Self::Go),
            _ => None,
        }
    }

    /// The tag written onto [`GraphNode::lang`].
    ///
    /// TSX collapses back to `typescript` here: the dialect split exists for
    /// grammar selection, and leaking it into stored data would make a `.ts`
    /// and a `.tsx` file look like different languages to every consumer.
    pub fn tag(self) -> &'static str {
        match self {
            Self::Rust => "rust",
            Self::Python => "python",
            Self::TypeScript | Self::Tsx => "typescript",
            Self::Go => "go",
        }
    }
}

/// A file handed to the builder: repo-relative path plus its text.
///
/// The builder never reads the filesystem itself. Everything it needs arrives
/// through this type, which is what lets the whole extraction and resolution
/// path be tested from string literals with no fixture directory.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SourceFile {
    /// Repo-relative, forward-slashed.
    pub path: String,
    /// The file's contents.
    pub text: String,
}

impl SourceFile {
    /// Build a source file.
    pub fn new(path: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            text: text.into(),
        }
    }
}

/// What kind of definition a symbol node stands for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SymbolKind {
    /// A free function.
    Function,
    /// A function bound to a type.
    Method,
    /// A class.
    Class,
    /// A Rust struct.
    Struct,
    /// An enum.
    Enum,
    /// A Rust trait or a TypeScript interface.
    Interface,
    /// A type alias.
    Type,
    /// A constant, static or module-level binding.
    Const,
    /// A named module (Rust `mod`, Go package).
    Module,
}

/// A definition found in a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Definition {
    /// The declared name, unqualified.
    pub name: String,
    /// What it declares.
    pub kind: SymbolKind,
    /// 1-based line of the name token, matching how evidence reports hunks.
    pub line: u32,
    /// Byte range of the whole declaration.
    ///
    /// Load-bearing, not decoration: a usage is attributed to the innermost
    /// definition whose range contains it, and that is how a `calls` edge gets
    /// a symbol on its *source* side instead of only a file.
    pub start_byte: usize,
    /// Exclusive end of the declaration.
    pub end_byte: usize,
}

/// An import as written, before resolution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportStmt {
    /// The specifier exactly as it appears: `@/lib/math`, `../util`,
    /// `crate::graph::types`, `github.com/me/mod/pkg`.
    pub specifier: String,
    /// The names bound by this import, when the syntax names them.
    ///
    /// Empty for a whole-module import. Used to resolve a bare call: `foo()`
    /// in a file that imported `foo` from `B` is a call into `B`, and without
    /// the binding list we would have to guess.
    pub names: Vec<String>,
    /// 1-based line of the statement.
    pub line: u32,
}

/// An identifier used somewhere in a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Usage {
    /// The name used, unqualified.
    pub name: String,
    /// Whether it appeared in call position.
    ///
    /// Decides [`EdgeKind::Calls`](crate::index::types::EdgeKind::Calls) versus
    /// [`EdgeKind::References`](crate::index::types::EdgeKind::References).
    pub call: bool,
    /// 1-based line.
    pub line: u32,
    /// Byte offset of the identifier, used to attribute it to an enclosing
    /// definition.
    pub byte: usize,
}

/// Everything extraction found in one file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedFile {
    /// Repo-relative path.
    pub path: String,
    /// The language it was parsed as.
    pub lang: Language,
    /// Definitions, in source order.
    pub defs: Vec<Definition>,
    /// Imports, in source order.
    pub imports: Vec<ImportStmt>,
    /// Identifier usages, in source order.
    pub usages: Vec<Usage>,
}

impl ParsedFile {
    /// The innermost definition containing `byte`, if any.
    ///
    /// Innermost rather than outermost so a call inside a method is attributed
    /// to the method, not to its class.
    pub fn enclosing(&self, byte: usize) -> Option<&Definition> {
        self.defs
            .iter()
            .filter(|d| d.start_byte <= byte && byte < d.end_byte)
            .min_by_key(|d| d.end_byte - d.start_byte)
    }
}

/// Why a specifier or a usage produced no edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnresolvedReason {
    /// The specifier names something outside this repository — a package, a
    /// standard-library module. Expected, and excluded from the coverage
    /// denominator.
    External,
    /// The specifier looks internal but matched no file in the tree.
    NoSuchFile,
    /// Several files could have been meant and none is a better answer.
    ///
    /// Recorded rather than guessed: a wrong edge sends retrieval to the wrong
    /// file with full confidence, which is worse than a missing one.
    Ambiguous,
    /// A name used but defined nowhere we can see.
    UnknownSymbol,
}

/// What the resolver could not turn into an edge.
///
/// The whole point of keeping these is that coverage is *measurable*. A
/// resolver that silently drops what it cannot handle reports the same
/// (perfect) success rate whether it resolves everything or nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unresolved {
    /// The file the specifier or usage was written in.
    pub path: String,
    /// The specifier or identifier itself.
    pub specifier: String,
    /// Why it did not resolve.
    pub reason: UnresolvedReason,
}

/// Counters describing how much of the source we actually turned into edges.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Coverage {
    /// Every import statement seen.
    pub imports_total: usize,
    /// Imports that resolved to at least one file in the repository.
    pub imports_resolved: usize,
    /// Imports naming something outside the repository.
    pub imports_external: usize,
    /// Every identifier usage seen.
    pub usages_total: usize,
    /// Usages bound to a definition in the repository.
    pub usages_resolved: usize,
}

impl Coverage {
    /// Share of *internal* imports that resolved, in `0.0..=1.0`.
    ///
    /// External specifiers are excluded from the denominator on purpose:
    /// `react` will never resolve to a file in this tree, and counting it as a
    /// failure would make the metric a measure of how many dependencies the
    /// repository has rather than of how good the resolver is.
    pub fn import_resolution_rate(&self) -> f64 {
        let internal = self.imports_total.saturating_sub(self.imports_external);
        if internal == 0 {
            return 1.0;
        }
        self.imports_resolved as f64 / internal as f64
    }

    /// Share of internal imports that looked internal and still failed.
    pub fn unresolved_rate(&self) -> f64 {
        1.0 - self.import_resolution_rate()
    }
}

/// A built graph, ready to be written through the
/// [`GraphStore`](crate::ports::graph::GraphStore) port.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RepoGraph {
    /// File and symbol nodes, deduplicated by id.
    pub nodes: Vec<GraphNode>,
    /// Edges, deduplicated by id.
    pub edges: Vec<GraphEdge>,
    /// Everything that did not become an edge, with a reason.
    pub unresolved: Vec<Unresolved>,
    /// How much we resolved.
    pub coverage: Coverage,
}
