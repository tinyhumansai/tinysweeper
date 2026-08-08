//! Symbol-aware splitting, behind the `treesitter` feature.
//!
//! The one invariant this module exists to hold: **a chunk never contains half
//! a definition**. Everything else here is in service of it.
//!
//! The method is deliberately not "walk the tree and emit every function". That
//! loses the text *between* definitions — imports, module comments, constants —
//! which is exactly the context a reviewer needs to judge whether a change fits
//! the file. Instead the grammar is used to place *cut points*, and the file is
//! then tiled between them: every byte still lands in some chunk, and no cut
//! lands inside a body.
//!
//! Three refinements on top of that:
//!
//! - A cut point moves backwards over the comment lines immediately above a
//!   definition, so a doc comment stays with the thing it documents rather than
//!   trailing the previous chunk.
//! - Small adjacent segments are merged up to the target size, so a file of
//!   twenty one-line constants is not twenty chunks.
//! - A container — an `impl`, a class, a module — that is larger than the target
//!   is opened up and its members become cut points too. Function bodies are
//!   never opened up, which is what keeps the invariant true.

use std::collections::BTreeMap;

use tree_sitter::{Node, Parser};

use crate::chunk::lang::Language;
use crate::chunk::lines;
use crate::chunk::types::{ChunkOptions, SourceChunk};
use crate::index::types::ChunkMethod;

/// How deep the container walk goes.
///
/// Three levels reaches a method inside a class inside a namespace, which is as
/// nested as real code gets before the members are too small to be worth
/// separate chunks anyway.
const MAX_DEPTH: usize = 3;

/// Node kinds whose members may become cut points when the node is too large.
///
/// A closed list, not a heuristic, and this is the load-bearing decision in the
/// module. Anything that ends up here has its children exposed as cut points, so
/// putting a function or a block in this list would allow a cut inside a body.
const CONTAINERS: &[&str] = &[
    // Rust
    "impl_item",
    "mod_item",
    "trait_item",
    "declaration_list",
    // TypeScript / JavaScript
    "class_declaration",
    "abstract_class_declaration",
    "class_body",
    "export_statement",
    "internal_module",
    "module",
    // Python
    "class_definition",
    "decorated_definition",
];

/// Node kinds that name something, beyond the generic suffix rule below.
const EXTRA_DEFINITIONS: &[&str] = &[
    "export_statement",
    "import_statement",
    "decorated_definition",
    "method_definition",
    "public_field_definition",
];

/// Split `source` at symbol boundaries.
///
/// `None` means the grammar could make nothing of the file — a `.ts` file that
/// is actually a data dump, say — and the caller should fall back to the line
/// splitter so the resulting chunks are labelled honestly.
pub fn split(source: &str, language: Language, options: &ChunkOptions) -> Option<Vec<SourceChunk>> {
    let mut parser = Parser::new();
    parser.set_language(&grammar(language)).ok()?;
    let tree = parser.parse(source, None)?;
    let root = tree.root_node();

    // A parse that found no named node, or that is mostly error, is not a
    // parse — a `.ts` file that is really a data dump, a template language
    // wearing a `.py` extension. Returning `None` here is what makes the
    // fallback honest rather than emitting one file-sized "parsed" chunk whose
    // boundaries nothing actually chose.
    if !source.trim().is_empty()
        && (root.named_child_count() == 0 || error_bytes(root) * 2 > source.len())
    {
        return None;
    }

    let mut cuts: BTreeMap<usize, Option<String>> = BTreeMap::new();
    collect_cuts(root, source, options, 0, &mut cuts);
    // The head of the file is always a cut point; without it the text above the
    // first definition would belong to no chunk.
    cuts.entry(0).or_insert(None);

    let segments = segments(&cuts, source.len());
    Some(assemble(source, &segments, options))
}

/// How many bytes of the top level the grammar could make nothing of.
fn error_bytes(root: Node<'_>) -> usize {
    root.children(&mut root.walk())
        .filter(|child| child.is_error() || child.is_missing())
        .map(|child| child.byte_range().len())
        .sum()
}

fn grammar(language: Language) -> tree_sitter::Language {
    match language {
        Language::Rust => tree_sitter_rust::LANGUAGE.into(),
        Language::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        Language::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
        Language::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
        Language::Python => tree_sitter_python::LANGUAGE.into(),
        Language::Go => tree_sitter_go::LANGUAGE.into(),
    }
}

/// One span of the file between two cut points.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Segment {
    start: usize,
    end: usize,
    symbol: Option<String>,
}

impl Segment {
    fn len(&self) -> usize {
        self.end.saturating_sub(self.start)
    }
}

fn collect_cuts(
    node: Node<'_>,
    source: &str,
    options: &ChunkOptions,
    depth: usize,
    cuts: &mut BTreeMap<usize, Option<String>>,
) {
    let children: Vec<Node<'_>> = node.named_children(&mut node.walk()).collect();

    for (index, child) in children.iter().enumerate() {
        if !is_definition(child.kind()) {
            continue;
        }

        // Pull the cut backwards over the comments directly above, so the doc
        // comment travels with its definition instead of ending the chunk
        // before it.
        let start = with_leading_comments(&children, index, source);
        let symbol = symbol_name(*child, source);
        cuts.entry(start).or_insert(symbol);

        // Only containers are opened up, and only when they are big enough that
        // one chunk would be useless. A function is never opened up at any size
        // — that is the invariant.
        if depth + 1 < MAX_DEPTH
            && child.byte_range().len() > options.target_chars
            && CONTAINERS.contains(&child.kind())
            && let Some(body) = body_of(*child)
        {
            collect_cuts(body, source, options, depth + 1, cuts);
        }
    }
}

/// Whether `kind` names a definition worth cutting at.
///
/// The suffix rule covers the four grammars without a per-language table:
/// `function_item`, `type_declaration`, `class_definition` and friends all end
/// one of three ways, while statements — `if_statement`, `expression_statement`
/// — deliberately do not, because cutting between the arms of an `if` would
/// split a body just as badly as cutting inside a function.
fn is_definition(kind: &str) -> bool {
    kind.ends_with("_item")
        || kind.ends_with("_declaration")
        || kind.ends_with("_definition")
        || EXTRA_DEFINITIONS.contains(&kind)
}

/// The byte offset of the run of comments directly above `children[index]`.
fn with_leading_comments(children: &[Node<'_>], index: usize, source: &str) -> usize {
    let mut start = children[index].start_byte();
    let mut cursor = index;
    while cursor > 0 {
        let previous = children[cursor - 1];
        if !previous.kind().contains("comment") {
            break;
        }
        // Only *adjacent* comments attach. A comment separated by a blank line
        // is a section header for what follows, not a doc comment, and grabbing
        // it would blur the boundary rather than sharpen it.
        let between = &source[previous.end_byte()..start];
        if between.chars().filter(|c| *c == '\n').count() > 1 {
            break;
        }
        start = previous.start_byte();
        cursor -= 1;
    }
    start
}

/// The node whose children are the container's members.
fn body_of(node: Node<'_>) -> Option<Node<'_>> {
    if let Some(body) = node.child_by_field_name("body") {
        return Some(body);
    }
    // `export class Foo {}` and Python's `@decorator def f()` wrap the thing
    // that actually has the members, so the wrapper delegates one level down.
    if let Some(declaration) = node
        .child_by_field_name("declaration")
        .or_else(|| node.child_by_field_name("definition"))
    {
        return body_of(declaration).or(Some(declaration));
    }
    node.named_children(&mut node.walk())
        .find(|child| CONTAINERS.contains(&child.kind()))
}

/// The name of the definition `node` introduces, when it has one.
fn symbol_name(node: Node<'_>, source: &str) -> Option<String> {
    if let Some(name) = node.child_by_field_name("name") {
        return text(name, source);
    }
    // Rust's `impl` has no name field; the type it is for is the useful label,
    // and for a trait impl the pair is what a reader would search for.
    if let Some(kind) = node.child_by_field_name("type") {
        let type_name = text(kind, source)?;
        return match node.child_by_field_name("trait").and_then(|t| text(t, source)) {
            Some(trait_name) => Some(format!("{trait_name} for {type_name}")),
            None => Some(type_name),
        };
    }
    // `export function f()`, `@decorator def f()`: the name is one level in.
    if let Some(inner) = node
        .child_by_field_name("declaration")
        .or_else(|| node.child_by_field_name("definition"))
    {
        return symbol_name(inner, source);
    }
    // `const handler = () => {}` — the declarator carries the name.
    node.named_children(&mut node.walk())
        .find(|child| child.kind() == "variable_declarator")
        .and_then(|declarator| symbol_name(declarator, source))
}

fn text(node: Node<'_>, source: &str) -> Option<String> {
    source
        .get(node.byte_range())
        .map(|slice| slice.trim().to_string())
        .filter(|slice| !slice.is_empty())
}

fn segments(cuts: &BTreeMap<usize, Option<String>>, len: usize) -> Vec<Segment> {
    let offsets: Vec<usize> = cuts.keys().copied().filter(|offset| *offset < len).collect();
    offsets
        .iter()
        .enumerate()
        .map(|(index, start)| Segment {
            start: *start,
            end: offsets.get(index + 1).copied().unwrap_or(len),
            symbol: cuts.get(start).cloned().flatten(),
        })
        .collect()
}

/// Merge adjacent segments up to the target, then turn each group into a chunk.
fn assemble(source: &str, segments: &[Segment], options: &ChunkOptions) -> Vec<SourceChunk> {
    let starts = line_starts(source);
    let mut chunks = Vec::new();
    let mut group: Vec<&Segment> = Vec::new();
    let mut size = 0_usize;

    for segment in segments {
        if !group.is_empty() && size + segment.len() > options.target_chars {
            emit(source, &starts, &group, options, &mut chunks);
            group.clear();
            size = 0;
        }
        group.push(segment);
        size += segment.len();
    }
    emit(source, &starts, &group, options, &mut chunks);
    chunks
}

fn emit(
    source: &str,
    starts: &[usize],
    group: &[&Segment],
    options: &ChunkOptions,
    chunks: &mut Vec<SourceChunk>,
) {
    let (Some(first), Some(last)) = (group.first(), group.last()) else {
        return;
    };
    let (start, end) = (first.start, last.end);
    let Some(text) = source.get(start..end) else {
        return;
    };
    if text.trim().is_empty() {
        // Whitespace embeds to nothing useful and still costs a call.
        return;
    }
    let start_line = line_of(starts, start);

    // Past the ceiling even as a single definition: the embedder would truncate
    // it, so the chunk is cut on lines and labelled as such rather than stored
    // as a parsed chunk whose tail was never embedded.
    if text.len() > options.max_chars {
        chunks.extend(lines::split(text, start_line, options));
        return;
    }

    // A name is attached only when the chunk *is* that one definition. Naming
    // one of three merged functions would read as a claim about the whole span.
    let mut named = group.iter().filter_map(|segment| segment.symbol.as_ref());
    let symbol = match (named.next(), named.next()) {
        (Some(only), None) => Some(only.clone()),
        _ => None,
    };

    chunks.push(SourceChunk {
        start_line,
        end_line: line_of(starts, end.saturating_sub(1)),
        text: text.to_string(),
        symbol,
        method: ChunkMethod::Parsed,
    });
}

fn line_starts(source: &str) -> Vec<usize> {
    let mut starts = vec![0];
    starts.extend(
        source
            .char_indices()
            .filter(|(_, c)| *c == '\n')
            .map(|(index, _)| index + 1),
    );
    starts
}

/// The 1-based line containing `byte`.
fn line_of(starts: &[usize], byte: usize) -> u32 {
    match starts.binary_search(&byte) {
        Ok(index) => index as u32 + 1,
        Err(index) => index as u32,
    }
}

#[cfg(test)]
#[path = "tree_test.rs"]
mod tests;
