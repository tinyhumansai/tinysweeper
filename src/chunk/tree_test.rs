//! Tests for the symbol-aware splitter.
//!
//! The first test is the one the module exists for: a function longer than the
//! target size must still come out as one whole chunk. Every other test here
//! guards a way that invariant could be lost quietly.

use super::*;

fn options() -> ChunkOptions {
    ChunkOptions::default()
}

/// A Rust file whose middle function is far longer than the target size.
fn long_rust_function() -> String {
    let body: String = (1..=120)
        .map(|i| format!("    let value_{i} = compute(value_{}, {i});\n", i - 1))
        .collect();
    format!(
        "//! A module.\n\
         \n\
         use std::fmt;\n\
         \n\
         const LIMIT: usize = 10;\n\
         \n\
         /// Does a great deal.\n\
         fn long_function(value_0: usize) -> usize {{\n{body}    value_120\n}}\n\
         \n\
         fn after() -> bool {{\n    true\n}}\n"
    )
}

/// Every `{` in the fixture is matched inside the same chunk.
///
/// A cut inside a body is exactly a chunk with unbalanced braces, so this is
/// the invariant restated in a form a test can check without knowing where the
/// splitter chose to cut.
fn braces_balance(text: &str) -> bool {
    let mut depth = 0_i64;
    for byte in text.bytes() {
        match byte {
            b'{' => depth += 1,
            b'}' => depth -= 1,
            _ => {}
        }
        if depth < 0 {
            return false;
        }
    }
    depth == 0
}

#[test]
fn a_function_longer_than_the_target_is_never_split() {
    let source = long_rust_function();
    assert!(
        source.len() > options().target_chars * 2,
        "the fixture must be well over the target for this test to mean anything"
    );

    let chunks = split(&source, Language::Rust, &options()).expect("rust parses");

    let holding: Vec<_> = chunks
        .iter()
        .filter(|c| c.text.contains("fn long_function"))
        .collect();
    assert_eq!(holding.len(), 1, "the signature must appear exactly once");

    let chunk = holding[0];
    assert!(
        chunk.text.contains("let value_120 = compute(value_119, 120);"),
        "the chunk holding the signature must hold the whole body"
    );
    assert!(chunk.text.contains("    value_120\n}"), "and its tail");
    assert_eq!(chunk.symbol.as_deref(), Some("long_function"));
    assert_eq!(chunk.method, ChunkMethod::Parsed);

    for chunk in &chunks {
        assert!(
            braces_balance(&chunk.text),
            "a chunk with unbalanced braces cut a body:\n{}",
            chunk.text
        );
    }
}

#[test]
fn a_doc_comment_travels_with_the_function_it_documents() {
    let chunks = split(&long_rust_function(), Language::Rust, &options()).expect("parses");
    let chunk = chunks
        .iter()
        .find(|c| c.text.contains("fn long_function"))
        .expect("found");
    assert!(
        chunk.text.contains("/// Does a great deal."),
        "the doc comment must not be left at the end of the previous chunk"
    );
}

#[test]
fn line_numbers_point_at_the_real_lines() {
    let source = "fn a() {}\nfn b() {}\nfn c() {}\n";
    // A tiny target forces one chunk per function, which is what makes the
    // line numbers checkable.
    let chunks = split(source, Language::Rust, &ChunkOptions::with_target(5)).expect("parses");
    assert_eq!(chunks.len(), 3);
    for (index, chunk) in chunks.iter().enumerate() {
        assert_eq!(chunk.start_line, index as u32 + 1, "{chunk:?}");
        assert_eq!(chunk.end_line, index as u32 + 1, "{chunk:?}");
    }
}

#[test]
fn small_neighbours_are_merged_rather_than_emitted_one_per_line() {
    let source: String = (0..40).map(|i| format!("const A{i}: usize = {i};\n")).collect();
    let chunks = split(&source, Language::Rust, &options()).expect("parses");
    assert!(
        chunks.len() < 10,
        "forty one-line constants became {} chunks",
        chunks.len()
    );
}

#[test]
fn a_merged_chunk_claims_no_single_symbol() {
    let source = "fn a() {}\nfn b() {}\n";
    let chunks = split(source, Language::Rust, &options()).expect("parses");
    assert_eq!(chunks.len(), 1);
    assert_eq!(
        chunks[0].symbol, None,
        "naming one of two merged functions would read as a claim about the span"
    );
}

#[test]
fn a_large_impl_block_is_opened_up_into_its_methods() {
    let method = |name: &str| {
        let body: String = (1..=30)
            .map(|i| format!("        let _x{i} = {i};\n"))
            .collect();
        format!("    fn {name}(&self) {{\n{body}    }}\n")
    };
    let source = format!(
        "struct Thing;\n\nimpl Thing {{\n{}{}{}}}\n",
        method("one"),
        method("two"),
        method("three")
    );

    let chunks = split(&source, Language::Rust, &options()).expect("parses");
    let named: Vec<_> = chunks.iter().filter_map(|c| c.symbol.as_deref()).collect();
    assert!(named.contains(&"one"), "{named:?}");
    assert!(named.contains(&"three"), "{named:?}");
    for chunk in &chunks {
        assert!(braces_balance(&chunk.text) || chunk.text.contains("impl Thing"));
    }
}

#[test]
fn an_impl_is_named_for_the_trait_and_type_it_joins() {
    let source = "struct S;\nimpl fmt::Display for S {\n    fn fmt(&self) {}\n}\n";
    let chunks = split(source, Language::Rust, &ChunkOptions::with_target(20)).expect("parses");
    let named: Vec<_> = chunks.iter().filter_map(|c| c.symbol.as_deref()).collect();
    assert!(
        named.iter().any(|n| n.contains("fmt::Display for S")),
        "{named:?}"
    );
}

#[test]
fn python_classes_and_functions_are_named() {
    let source = "import os\n\n\nclass Widget:\n    def render(self):\n        return 1\n\n\ndef helper(a):\n    return a\n";
    let chunks = split(source, Language::Python, &ChunkOptions::with_target(30)).expect("parses");
    let named: Vec<_> = chunks.iter().filter_map(|c| c.symbol.as_deref()).collect();
    assert!(named.contains(&"Widget"), "{named:?}");
    assert!(named.contains(&"helper"), "{named:?}");
}

#[test]
fn go_functions_and_types_are_named() {
    let source = "package main\n\ntype Server struct {\n\tPort int\n}\n\nfunc Serve(s *Server) error {\n\treturn nil\n}\n";
    let chunks = split(source, Language::Go, &ChunkOptions::with_target(20)).expect("parses");
    let named: Vec<_> = chunks.iter().filter_map(|c| c.symbol.as_deref()).collect();
    assert!(named.contains(&"Serve"), "{named:?}");
}

#[test]
fn typescript_exports_and_arrow_consts_are_named() {
    let source = "import x from 'y';\n\nexport function build(a: number): number {\n  return a;\n}\n\nconst handler = (e: Event) => {\n  console.log(e);\n};\n";
    let chunks = split(source, Language::TypeScript, &ChunkOptions::with_target(30)).expect("parses");
    let named: Vec<_> = chunks.iter().filter_map(|c| c.symbol.as_deref()).collect();
    assert!(named.contains(&"build"), "{named:?}");
    assert!(named.contains(&"handler"), "{named:?}");
}

#[test]
fn tsx_parses_where_the_typescript_grammar_would_stumble() {
    let source = "export function View() {\n  return <div className=\"x\">hi</div>;\n}\n";
    let chunks = split(source, Language::Tsx, &options()).expect("tsx parses");
    assert!(chunks.iter().any(|c| c.symbol.as_deref() == Some("View")));
}

#[test]
fn javascript_functions_are_named() {
    let source = "function alpha() {\n  return 1;\n}\n\nclass Beta {\n  go() {}\n}\n";
    let chunks = split(source, Language::JavaScript, &ChunkOptions::with_target(15)).expect("parses");
    let named: Vec<_> = chunks.iter().filter_map(|c| c.symbol.as_deref()).collect();
    assert!(named.contains(&"alpha"), "{named:?}");
    assert!(named.contains(&"Beta"), "{named:?}");
}

#[test]
fn a_definition_past_the_ceiling_is_cut_on_lines_and_says_so() {
    // The chunk that gets truncated by the embedder must not claim to be a
    // whole parsed definition, because retrieval would then quote a body whose
    // tail was never embedded.
    let body: String = (1..=400)
        .map(|i| format!("    let value_{i} = {i};\n"))
        .collect();
    let source = format!("fn enormous() {{\n{body}}}\n");
    let options = ChunkOptions {
        target_chars: 200,
        max_chars: 400,
    };

    let chunks = split(&source, Language::Rust, &options).expect("parses");
    assert!(chunks.len() > 1);
    assert!(
        chunks.iter().all(|c| c.method == ChunkMethod::Lines),
        "an over-ceiling definition must be labelled as line-split"
    );
}

#[test]
fn an_empty_file_yields_no_chunks() {
    assert_eq!(split("", Language::Rust, &options()), Some(vec![]));
    assert_eq!(split("\n\n  \n", Language::Rust, &options()), Some(vec![]));
}

#[test]
fn a_file_the_grammar_cannot_use_falls_back_rather_than_lying() {
    // Bytes that produce no named node at all: the caller must be told to use
    // the line splitter so the chunk is labelled `lines`.
    assert_eq!(split("!!!", Language::Go, &options()), None);
}

#[test]
fn every_grammar_loads() {
    // A version bump that breaks an ABI shows up here rather than as silent
    // line-splitting of every file in that language.
    for language in Language::ALL {
        let mut parser = Parser::new();
        parser
            .set_language(&grammar(language))
            .unwrap_or_else(|err| panic!("{language:?}: {err}"));
    }
}
