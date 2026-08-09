//! Symbol extraction: one source file in, one [`ParsedFile`] out.
//!
//! Always compiled. This module owns every tree-sitter call in the crate that
//! concerns the graph; nothing downstream of it knows what a syntax node is.
//!
//! Import statements are walked in Rust rather than matched by query. A query
//! can find `import { a, b } from "@/lib/x"`, but pulling the specifier *and*
//! the bound names out of it needs per-language structure that the pattern
//! syntax cannot express, and guessing the names is what turns a call edge
//! into a wrong call edge.

use std::collections::BTreeSet;

use streaming_iterator::StreamingIterator;
use tree_sitter::{Node, Parser, QueryCursor};

use crate::error::{Error, Result};
use crate::graph::lang;
use crate::graph::path;
use crate::graph::types::{
    Definition, Heritage, ImportStmt, Language, ParsedFile, SourceFile, Usage,
};

/// Parse one file into definitions, imports and usages.
///
/// Returns `Ok(None)` for a language we do not parse, which is the common case
/// on any real repository and not an error.
pub fn parse(file: &SourceFile) -> Result<Option<ParsedFile>> {
    let Some(language) = Language::from_path(&file.path) else {
        return Ok(None);
    };
    parse_as(file, language).map(Some)
}

/// Parse `file` as `language`, regardless of its extension.
pub fn parse_as(file: &SourceFile, language: Language) -> Result<ParsedFile> {
    let mut parser = Parser::new();
    parser
        .set_language(&lang::grammar(language))
        .map_err(|e| Error::Config(format!("graph grammar for {}: {e}", language.tag())))?;
    let tree = parser.parse(file.text.as_bytes(), None).ok_or_else(|| {
        Error::Config(format!("graph: tree-sitter could not parse {}", file.path))
    })?;

    let query = lang::query(language)?;
    let source = file.text.as_bytes();
    let mut cursor = QueryCursor::new();

    let mut defs: Vec<Definition> = Vec::new();
    let mut imports: Vec<ImportStmt> = Vec::new();
    let mut heritage: Vec<Heritage> = Vec::new();
    let mut calls: Vec<Usage> = Vec::new();
    let mut refs: Vec<Usage> = Vec::new();
    // Rust `mod foo;` is both a declaration and a file reference. Recorded here
    // so the import walk can see it without re-querying the tree.
    let mut bodyless_mods: Vec<(String, Option<String>, u32, usize)> = Vec::new();
    // Byte ranges of Rust `mod name { ... }` blocks. A module written inline
    // has no file, so `use super::*` inside one names the *enclosing file*
    // rather than a sibling module — the single most common `use` in this
    // codebase, and one a file-based resolver would otherwise report as a
    // missing module forever.
    let mut inline_mods: Vec<(usize, usize)> = Vec::new();
    // Byte ranges of import statements. The names inside an import clause are
    // matched by the bare-identifier pattern too, and keeping them would make
    // every import also a `references` edge — including one from a file to
    // itself when it imports a name it re-exports.
    let mut import_spans: Vec<(usize, usize)> = Vec::new();

    let mut matches = cursor.matches(&query, tree.root_node(), source);
    while let Some(m) = matches.next() {
        let mut name_node: Option<Node> = None;
        let mut decl: Option<(Node, &str)> = None;

        for capture in m.captures {
            let capture_name = &query.capture_names()[capture.index as usize];
            match *capture_name {
                lang::CAP_NAME => name_node = Some(capture.node),
                lang::CAP_IMPORT => {
                    import_spans.push((capture.node.start_byte(), capture.node.end_byte()));
                    imports.extend(walk_import(capture.node, source, language));
                }
                lang::CAP_CALL => calls.push(usage(capture.node, source, true)),
                lang::CAP_REF => refs.push(usage(capture.node, source, false)),
                lang::CAP_HERITAGE => {
                    heritage.extend(standalone_heritage(capture.node, source, language));
                }
                other if other.starts_with(lang::CAP_DEF_PREFIX) => {
                    decl = Some((capture.node, other));
                }
                // A capture that exists only for a query predicate to test —
                // Ruby's `require` matcher. Named rather than swept up by the
                // catch-all so a mistyped capture name stays distinguishable
                // from a deliberately ignored one.
                other if other.starts_with(lang::CAP_IGNORE_PREFIX) => {}
                _ => {}
            }
        }

        if let (Some(name_node), Some((decl_node, capture_name))) = (name_node, decl)
            && let Some(kind) = lang::symbol_kind(capture_name)
        {
            let name = text(name_node, source);
            if language == Language::Rust && capture_name == "def.module" {
                if decl_node.child_by_field_name("body").is_none() {
                    bodyless_mods.push((
                        name.clone(),
                        rust_path_attribute(decl_node, source),
                        line(name_node),
                        decl_node.start_byte(),
                    ));
                } else {
                    inline_mods.push((decl_node.start_byte(), decl_node.end_byte()));
                }
            }
            heritage.extend(declared_heritage(decl_node, &name, source, language));
            defs.push(Definition {
                test: is_test_definition(decl_node, &name, kind, source, language),
                name,
                kind,
                line: line(name_node),
                start_byte: decl_node.start_byte(),
                end_byte: decl_node.end_byte(),
            });
        }
    }

    for (name, path_attribute, at, byte) in bodyless_mods {
        // `mod foo;` names a sibling file. Marked with a `mod ` prefix so the
        // resolver can tell it apart from a `use` path, which is resolved
        // against the crate root instead of the current directory.
        //
        // `#[path = "..."]` overrides that lookup entirely, and this repository
        // uses it for every out-of-line test module — so honouring it is the
        // difference between the graph seeing its own test files and not.
        let specifier = match path_attribute {
            Some(relative) => format!("path {relative}"),
            None => format!("mod {name}"),
        };
        imports.push(ImportStmt {
            specifier,
            names: vec![name],
            line: at,
            byte,
        });
    }

    if language == Language::Rust {
        imports.retain(|import| !names_the_enclosing_file(import, &inline_mods));
    }

    // An identifier in call position is also matched by the bare-identifier
    // reference pattern. Dropping the duplicate here keeps one usage per
    // occurrence, so a single call cannot produce both a `calls` and a
    // `references` edge.
    let called: BTreeSet<usize> = calls.iter().map(|u| u.byte).collect();
    let mut usages = calls;
    usages.extend(refs.into_iter().filter(|u| !called.contains(&u.byte)));
    usages.retain(|u| {
        !import_spans
            .iter()
            .any(|(start, end)| *start <= u.byte && u.byte < *end)
    });
    usages.sort_by_key(|u| (u.byte, u.call));
    usages.dedup_by_key(|u| u.byte);

    defs.sort_by_key(|d| (d.start_byte, d.end_byte));
    defs.dedup_by(|a, b| a.name == b.name && a.start_byte == b.start_byte);

    heritage.sort_by(|a, b| (a.byte, &a.child, &a.parent).cmp(&(b.byte, &b.child, &b.parent)));
    heritage.dedup_by(|a, b| a.child == b.child && a.parent == b.parent && a.byte == b.byte);

    Ok(ParsedFile {
        path: file.path.clone(),
        lang: language,
        defs,
        imports,
        usages,
        heritage,
        test_file: path::is_test_path(&file.path),
    })
}

fn usage(node: Node, source: &[u8], call: bool) -> Usage {
    Usage {
        name: text(node, source),
        call,
        line: line(node),
        byte: node.start_byte(),
    }
}

fn text(node: Node, source: &[u8]) -> String {
    node.utf8_text(source).unwrap_or_default().to_string()
}

fn line(node: Node) -> u32 {
    node.start_position().row as u32 + 1
}

/// Every named child of `node` carrying the field name `field`.
///
/// `Node::child_by_field_name` returns only the first, and Python's
/// `import a, b` and `from m import x, y` both repeat the `name` field — so
/// using the single-child accessor there would index the first name and drop
/// the rest.
fn fields<'t>(node: Node<'t>, field: &str) -> Vec<Node<'t>> {
    let mut walker = node.walk();
    let mut out = Vec::new();
    if walker.goto_first_child() {
        loop {
            if walker.field_name() == Some(field) {
                out.push(walker.node());
            }
            if !walker.goto_next_sibling() {
                break;
            }
        }
    }
    out
}

/// Strip one layer of quotes from a string literal's text.
fn unquote(raw: &str) -> String {
    let trimmed = raw.trim();
    let bytes = trimmed.as_bytes();
    if bytes.len() >= 2 {
        let first = bytes[0];
        let last = bytes[bytes.len() - 1];
        if first == last && (first == b'"' || first == b'\'' || first == b'`') {
            return trimmed[1..trimmed.len() - 1].to_string();
        }
    }
    trimmed.to_string()
}

// --- Inheritance -------------------------------------------------------------

/// Inheritance written *on* a declaration, which therefore names its own child.
///
/// `class Ledger extends Base`, `class Ledger(Base)`, `trait Ledger: Base`, and
/// a Go struct's embedded fields all take this path. Rust's `impl Trait for
/// Type` does not: it declares neither of the two types it names, so it is
/// captured separately and handled by [`standalone_heritage`].
fn declared_heritage(decl: Node, name: &str, source: &[u8], language: Language) -> Vec<Heritage> {
    let parents = match language {
        Language::TypeScript | Language::Tsx => ts_parents(decl, source),
        Language::Python => python_parents(decl, source),
        Language::Go => go_parents(decl, source),
        Language::Rust => rust_supertraits(decl, source),
        // Java (`extends`/`implements`) and Ruby (`< Superclass`) both write
        // inheritance on the declaration, but neither grammar has a parent
        // walker yet. Recording nothing is honest: a missing heritage edge
        // just means the graph is caller-complete for those two languages, not
        // inheritance-complete.
        Language::Java | Language::Ruby => Vec::new(),
    };
    parents
        .into_iter()
        .filter(|parent| parent != name)
        .map(|parent| Heritage {
            child: name.to_string(),
            parent,
            line: line(decl),
            byte: decl.start_byte(),
        })
        .collect()
}

/// Inheritance written as its own item: a Rust `impl Trait for Type`.
///
/// A bare `impl Type { .. }` names no trait and yields nothing — an inherent
/// impl relates a type to no other type, and inventing a self-edge for it
/// would put every type in the repository one hop from itself.
fn standalone_heritage(node: Node, source: &[u8], language: Language) -> Vec<Heritage> {
    if language != Language::Rust || node.kind() != "impl_item" {
        return Vec::new();
    }
    let (Some(trait_node), Some(type_node)) = (
        node.child_by_field_name("trait"),
        node.child_by_field_name("type"),
    ) else {
        return Vec::new();
    };
    let (Some(parent), Some(child)) = (type_name(trait_node, source), type_name(type_node, source))
    else {
        return Vec::new();
    };
    if parent == child {
        return Vec::new();
    }
    vec![Heritage {
        child,
        parent,
        line: line(node),
        byte: node.start_byte(),
    }]
}

/// The name a type expression ultimately refers to.
///
/// Generic arguments are deliberately *not* followed. `extends Repository<User>`
/// inherits from `Repository`; recording `User` as a base class would make the
/// blast radius of a change to `User` include every class that merely mentions
/// it in a type position.
fn type_name(node: Node, source: &[u8]) -> Option<String> {
    match node.kind() {
        "type_identifier" | "identifier" | "primitive_type" => Some(text(node, source)),
        "generic_type"
        | "qualified_type"
        | "scoped_type_identifier"
        | "nested_type_identifier"
        | "member_expression"
        | "attribute" => node
            .child_by_field_name("name")
            .or_else(|| node.child_by_field_name("type"))
            .or_else(|| node.child_by_field_name("property"))
            .or_else(|| node.child_by_field_name("attribute"))
            .and_then(|inner| type_name(inner, source)),
        "pointer_type" | "generic_type_with_turbofish" => node
            .named_child(0)
            .and_then(|inner| type_name(inner, source)),
        _ => None,
    }
}

/// Collect type names from a subtree, without descending into type arguments.
fn collect_type_names(node: Node, source: &[u8], out: &mut Vec<String>) {
    if node.kind() == "type_arguments" || node.kind() == "type_parameters" {
        return;
    }
    if let Some(name) = type_name(node, source) {
        out.push(name);
        return;
    }
    let mut walker = node.walk();
    for child in node.named_children(&mut walker) {
        collect_type_names(child, source, out);
    }
}

/// `class A extends B implements C`, and `interface A extends B`.
fn ts_parents(decl: Node, source: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    let mut walker = decl.walk();
    for child in decl.named_children(&mut walker) {
        if matches!(
            child.kind(),
            "class_heritage" | "extends_clause" | "implements_clause" | "extends_type_clause"
        ) {
            collect_type_names(child, source, &mut out);
        }
    }
    out
}

/// `class Ledger(Base, metaclass=Meta)` — the positional bases only.
fn python_parents(decl: Node, source: &[u8]) -> Vec<String> {
    let Some(bases) = decl.child_by_field_name("superclasses") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut walker = bases.walk();
    for child in bases.named_children(&mut walker) {
        // `metaclass=Meta` is configuration, not a base class.
        if child.kind() == "keyword_argument" {
            continue;
        }
        if let Some(name) = type_name(child, source) {
            out.push(name);
        }
    }
    out
}

/// Go's embedding: a struct or interface field with a type and no name.
///
/// This is the language's whole inheritance story, and the "no name" test is
/// what distinguishes it — `Mutex` embeds, `mu Mutex` is an ordinary field and
/// promotes nothing.
fn go_parents(decl: Node, source: &[u8]) -> Vec<String> {
    let Some(underlying) = decl.child_by_field_name("type") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    match underlying.kind() {
        "struct_type" => {
            let Some(fields) = underlying.named_child(0) else {
                return out;
            };
            let mut walker = fields.walk();
            for field in fields.named_children(&mut walker) {
                if field.kind() != "field_declaration"
                    || field.child_by_field_name("name").is_some()
                {
                    continue;
                }
                if let Some(name) = field
                    .child_by_field_name("type")
                    .and_then(|t| type_name(t, source))
                {
                    out.push(name);
                }
            }
        }
        "interface_type" => {
            let mut walker = underlying.walk();
            for child in underlying.named_children(&mut walker) {
                // A method signature declares behaviour; only a bare type name
                // embeds another interface.
                if matches!(child.kind(), "method_spec" | "method_elem") {
                    continue;
                }
                collect_type_names(child, source, &mut out);
            }
        }
        _ => {}
    }
    out
}

/// `trait Ledger: Display + Debug` — the supertrait bounds.
fn rust_supertraits(decl: Node, source: &[u8]) -> Vec<String> {
    if decl.kind() != "trait_item" {
        return Vec::new();
    }
    let Some(bounds) = decl.child_by_field_name("bounds") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    collect_type_names(bounds, source, &mut out);
    // Lifetimes and `Sized` bounds are not supertraits worth an edge: every
    // trait in the language has them and they relate it to nothing local.
    out.retain(|name| name != "Sized");
    out
}

// --- Test scopes -------------------------------------------------------------

/// Whether a declaration is a test, by the rule its own test runner uses.
///
/// Nothing here is a guess about intent. `go test` runs `TestX`; pytest
/// collects `test*` and `Test*`; `cargo test` runs what `#[test]` or
/// `#[cfg(test)]` marks. TypeScript is absent on purpose — a Jest test is an
/// anonymous callback inside `it(...)`, so the file is the only scope there is,
/// and [`ParsedFile::test_file`] carries that.
fn is_test_definition(
    decl: Node,
    name: &str,
    kind: crate::graph::types::SymbolKind,
    source: &[u8],
    language: Language,
) -> bool {
    use crate::graph::types::SymbolKind;
    match language {
        Language::Rust => rust_test_attribute(decl, source),
        Language::Python => match kind {
            SymbolKind::Class => starts_with_word(name, "Test"),
            SymbolKind::Function | SymbolKind::Method => name.starts_with("test"),
            _ => false,
        },
        Language::Go => {
            matches!(kind, SymbolKind::Function)
                && ["Test", "Benchmark", "Fuzz", "Example"]
                    .iter()
                    .any(|prefix| starts_with_word(name, prefix))
        }
        Language::TypeScript | Language::Tsx => false,
        // No test-runner rule is defined for these yet. Java's `@Test` is an
        // attribute like Rust's `#[test]`, and Ruby's runners name methods
        // `test_*` or describe blocks; until one of those is modelled, a Java
        // or Ruby file claims no test scope, which is a coverage miss the
        // reader sees rather than a name-guess that lies.
        Language::Java | Language::Ruby => false,
    }
}

/// Whether `name` begins with `prefix` as a complete word.
///
/// `TestLedger` is a Go test; `Testing` is not, and neither is `Tester`. The
/// rule is the runner's: the character after the prefix must not be lowercase.
fn starts_with_word(name: &str, prefix: &str) -> bool {
    let Some(rest) = name.strip_prefix(prefix) else {
        return false;
    };
    !rest.starts_with(|c: char| c.is_lowercase())
}

/// Whether an item carries a `#[test]`-family or `#[cfg(test)]` attribute.
///
/// `#[cfg(test)]` counts, and counting it is the point: it is what marks the
/// `mod tests` block that holds every in-crate test in this codebase, and
/// marking the module makes every function inside it a test scope without
/// having to recognise `#[tokio::test]`, `#[rstest]` and the rest by name.
fn rust_test_attribute(node: Node, source: &[u8]) -> bool {
    let mut sibling = node.prev_sibling();
    while let Some(current) = sibling {
        match current.kind() {
            "attribute_item" => {
                // The attribute's own path decides, not every word its text
                // contains: `#[cfg(feature = "test")]` sets a feature flag and
                // `#[serde(rename = "test")]` names a serialised field, and
                // neither puts the item in a test scope. A token scan could
                // not tell them apart — `test` sat inside a string literal.
                if let Some(attribute) = current.named_child(0)
                    && let Some(path) = attribute.named_child(0)
                {
                    let path = text(path, source);
                    if path == "test" || path.ends_with("::test") {
                        return true;
                    }
                    if (path == "cfg" || path.ends_with("::cfg"))
                        && attribute
                            .child_by_field_name("arguments")
                            .is_some_and(|args| bare_test_identifier(args, source))
                    {
                        return true;
                    }
                }
            }
            "line_comment" | "block_comment" | "attribute" => {}
            _ => break,
        }
        sibling = current.prev_sibling();
    }
    false
}

/// Whether a `cfg(..)` argument list names the bare `test` identifier.
///
/// `cfg(test)` holds one; `cfg(feature = "test")` does not — there the word
/// sits inside a string literal, which selects nothing and must not count.
/// Wrapped predicates are walked so `cfg(all(test, ...))` still counts; only
/// identifiers qualify, so no recursion can reach the inside of a literal.
fn bare_test_identifier(node: Node, source: &[u8]) -> bool {
    if node.kind() == "identifier" {
        return text(node, source) == "test";
    }
    let mut walker = node.walk();
    node.named_children(&mut walker)
        .any(|child| bare_test_identifier(child, source))
}

/// Pull the specifier and bound names out of one import statement.
fn walk_import(node: Node, source: &[u8], language: Language) -> Vec<ImportStmt> {
    match language {
        Language::Rust => rust_import(node, source),
        Language::Python => python_import(node, source),
        Language::TypeScript | Language::Tsx => ts_import(node, source),
        Language::Go => go_import(node, source),
        Language::Java => java_import(node, source),
        Language::Ruby => ruby_import(node, source),
    }
}

// --- Java -------------------------------------------------------------------

/// `import a.b.C;` and `import static a.b.C.member;`, single or on-demand.
///
/// The specifier keeps its dots; the resolver turns them into a path. A
/// wildcard import binds no name — `import a.b.*` makes every type in the
/// package visible without saying which — so the file edge is recorded and the
/// name bindings are left empty rather than guessed at.
fn java_import(node: Node, source: &[u8]) -> Vec<ImportStmt> {
    let text = text(node, source);
    let body = text
        .trim()
        .trim_start_matches("import")
        .trim()
        .trim_start_matches("static")
        .trim()
        .trim_end_matches(';')
        .trim();
    if body.is_empty() {
        return Vec::new();
    }

    let wildcard = body.ends_with(".*");
    let specifier = body.trim_end_matches(".*").trim_end_matches('.');
    if specifier.is_empty() {
        return Vec::new();
    }
    let names = if wildcard {
        Vec::new()
    } else {
        specifier
            .rsplit('.')
            .next()
            .map(|name| vec![name.to_string()])
            .unwrap_or_default()
    };
    vec![ImportStmt {
        byte: node.start_byte(),
        specifier: specifier.to_string(),
        names,
        line: line(node),
    }]
}

// --- Ruby -------------------------------------------------------------------

/// `require_relative "x"` and friends, which are method calls rather than
/// statements.
///
/// The callee is kept on the front of the specifier — `require_relative x`
/// versus `require x` — because the two resolve against completely different
/// roots and the resolver has no other way to tell them apart. Rust's
/// `mod foo;` marker uses the same trick for the same reason.
///
/// No names are bound. Ruby's `require` makes a file's constants visible
/// without naming any of them, so there is nothing to bind and inventing a
/// binding from the filename would attribute every use of `Foo` in the file to
/// whichever `require` happened to mention `foo`.
fn ruby_import(node: Node, source: &[u8]) -> Vec<ImportStmt> {
    let Some(method) = node.child_by_field_name("method") else {
        return Vec::new();
    };
    let Some(arguments) = node.child_by_field_name("arguments") else {
        return Vec::new();
    };
    let mut cursor = arguments.walk();
    let Some(first) = arguments
        .named_children(&mut cursor)
        .find(|child| child.kind() == "string")
    else {
        return Vec::new();
    };

    let literal = unquote(&text(first, source));
    if literal.is_empty() {
        return Vec::new();
    }
    vec![ImportStmt {
        byte: node.start_byte(),
        specifier: format!("{} {literal}", text(method, source)),
        names: Vec::new(),
        line: line(node),
    }]
}

// --- Rust ------------------------------------------------------------------

/// Whether a Rust `use` inside an inline `mod` block only names its own file.
///
/// `use super::*` written inside `mod tests { .. }` walks up to the module the
/// block lives in — which is the file itself. That is not a cross-file fact
/// and must not be resolved as one, or every test module in the repository
/// reports a missing sibling module.
fn names_the_enclosing_file(import: &ImportStmt, inline_mods: &[(usize, usize)]) -> bool {
    let depth = inline_mods
        .iter()
        .filter(|(start, end)| *start <= import.byte && import.byte < *end)
        .count();
    if depth == 0 {
        return false;
    }
    let mut segments = import.specifier.split("::");
    match segments.next() {
        Some("self") => true,
        Some("super") => {
            let ups = 1 + import
                .specifier
                .split("::")
                .skip(1)
                .take_while(|s| *s == "super")
                .count();
            ups <= depth
        }
        _ => false,
    }
}

/// The value of a `#[path = "..."]` attribute preceding an item, if any.
fn rust_path_attribute(node: Node, source: &[u8]) -> Option<String> {
    let mut sibling = node.prev_sibling();
    while let Some(current) = sibling {
        if current.kind() == "attribute_item" {
            let text = text(current, source);
            if let Some(rest) = text.split_once("path")
                && let Some(open) = rest.1.find('"')
                && let Some(close) = rest.1[open + 1..].find('"')
            {
                return Some(rest.1[open + 1..open + 1 + close].to_string());
            }
        } else if current.kind() != "line_comment" && current.kind() != "block_comment" {
            break;
        }
        sibling = current.prev_sibling();
    }
    None
}

fn rust_import(node: Node, source: &[u8]) -> Vec<ImportStmt> {
    let Some(argument) = node.child_by_field_name("argument") else {
        return Vec::new();
    };
    let at = line(node);
    rust_use_paths(argument, source)
        .into_iter()
        .filter(|segments| !segments.is_empty())
        .map(|segments| {
            let name = segments.last().cloned().unwrap_or_default();
            ImportStmt {
                byte: node.start_byte(),
                specifier: segments.join("::"),
                // A `use` binds its last segment, which is exactly the name a
                // later bare call in this file will use.
                names: if name == "*" { Vec::new() } else { vec![name] },
                line: at,
            }
        })
        .collect()
}

/// Flatten a `use` tree into one segment list per bound path.
///
/// `use crate::a::{b::C, d}` becomes `[crate,a,b,C]` and `[crate,a,d]`. Nested
/// lists are why this is recursive rather than a text split: the segments
/// before a brace apply to every branch inside it.
fn rust_use_paths(node: Node, source: &[u8]) -> Vec<Vec<String>> {
    match node.kind() {
        "identifier" | "crate" | "self" | "super" | "primitive_type" | "metavariable" => {
            vec![vec![text(node, source)]]
        }
        "scoped_identifier" => {
            let name = node
                .child_by_field_name("name")
                .map(|n| text(n, source))
                .unwrap_or_default();
            match node.child_by_field_name("path") {
                Some(path) => extend_each(rust_use_paths(path, source), &name),
                None => vec![vec![name]],
            }
        }
        "scoped_use_list" => {
            let list = node.child_by_field_name("list");
            let tails = list
                .map(|l| rust_use_paths(l, source))
                .unwrap_or_else(|| vec![Vec::new()]);
            match node.child_by_field_name("path") {
                Some(path) => {
                    let heads = rust_use_paths(path, source);
                    let mut out = Vec::new();
                    for head in &heads {
                        for tail in &tails {
                            let mut joined = head.clone();
                            joined.extend(tail.iter().cloned());
                            out.push(joined);
                        }
                    }
                    out
                }
                None => tails,
            }
        }
        "use_list" => {
            let mut out = Vec::new();
            let mut walker = node.walk();
            for child in node.named_children(&mut walker) {
                out.extend(rust_use_paths(child, source));
            }
            out
        }
        "use_as_clause" => {
            // The alias renames the binding; the *path* is what has to resolve,
            // so the alias is deliberately not folded into the specifier.
            match node.child_by_field_name("path") {
                Some(path) => rust_use_paths(path, source),
                None => Vec::new(),
            }
        }
        "use_wildcard" => {
            let mut walker = node.walk();
            let inner: Vec<Node> = node.named_children(&mut walker).collect();
            match inner.first() {
                Some(first) => extend_each(rust_use_paths(*first, source), "*"),
                None => vec![vec!["*".to_string()]],
            }
        }
        _ => Vec::new(),
    }
}

fn extend_each(mut paths: Vec<Vec<String>>, tail: &str) -> Vec<Vec<String>> {
    for path in &mut paths {
        path.push(tail.to_string());
    }
    paths
}

// --- Python ----------------------------------------------------------------

fn python_import(node: Node, source: &[u8]) -> Vec<ImportStmt> {
    let at = line(node);
    match node.kind() {
        "import_statement" => {
            let mut out = Vec::new();
            for child in fields(node, "name") {
                let module = match child.kind() {
                    "aliased_import" => child.child_by_field_name("name"),
                    _ => Some(child),
                };
                if let Some(module) = module {
                    out.push(ImportStmt {
                        specifier: text(module, source),
                        names: Vec::new(),
                        line: at,
                        byte: node.start_byte(),
                    });
                }
            }
            out
        }
        "import_from_statement" => {
            let Some(module) = node.child_by_field_name("module_name") else {
                return Vec::new();
            };
            let mut names = Vec::new();
            for child in fields(node, "name") {
                let bound = match child.kind() {
                    "aliased_import" => child.child_by_field_name("alias"),
                    _ => Some(child),
                };
                if let Some(bound) = bound {
                    names.push(text(bound, source));
                }
            }
            vec![ImportStmt {
                specifier: text(module, source),
                names,
                line: at,
                byte: node.start_byte(),
            }]
        }
        _ => Vec::new(),
    }
}

// --- TypeScript / JavaScript ------------------------------------------------

fn ts_import(node: Node, source: &[u8]) -> Vec<ImportStmt> {
    let Some(source_node) = node.child_by_field_name("source") else {
        return Vec::new();
    };
    let mut names = Vec::new();
    let mut walker = node.walk();
    for child in node.named_children(&mut walker) {
        collect_ts_bindings(child, source, &mut names);
    }
    vec![ImportStmt {
        specifier: unquote(&text(source_node, source)),
        names,
        line: line(node),
        byte: node.start_byte(),
    }]
}

/// Collect the local names an import clause binds.
///
/// The *local* name is the one that matters: `import { a as b }` makes a call
/// to `b`, not to `a`, the thing that must resolve into the imported file.
fn collect_ts_bindings(node: Node, source: &[u8], out: &mut Vec<String>) {
    match node.kind() {
        "import_specifier" | "export_specifier" => {
            let bound = node
                .child_by_field_name("alias")
                .or_else(|| node.child_by_field_name("name"));
            if let Some(bound) = bound {
                out.push(text(bound, source));
            }
        }
        "namespace_import" | "namespace_export" => {
            let mut walker = node.walk();
            for child in node.named_children(&mut walker) {
                if child.kind() == "identifier" {
                    out.push(text(child, source));
                }
            }
        }
        "identifier" => out.push(text(node, source)),
        _ => {
            let mut walker = node.walk();
            for child in node.named_children(&mut walker) {
                collect_ts_bindings(child, source, out);
            }
        }
    }
}

// --- Go ---------------------------------------------------------------------

fn go_import(node: Node, source: &[u8]) -> Vec<ImportStmt> {
    let at = line(node);
    let mut out = Vec::new();
    collect_go_specs(node, source, at, &mut out);
    out
}

fn collect_go_specs(node: Node, source: &[u8], at: u32, out: &mut Vec<ImportStmt>) {
    if node.kind() == "import_spec" {
        if let Some(path) = node.child_by_field_name("path") {
            out.push(ImportStmt {
                specifier: unquote(&text(path, source)),
                names: Vec::new(),
                line: at,
                byte: node.start_byte(),
            });
        }
        return;
    }
    let mut walker = node.walk();
    for child in node.named_children(&mut walker) {
        collect_go_specs(child, source, at, out);
    }
}

#[cfg(test)]
#[path = "extract_test.rs"]
mod tests;
