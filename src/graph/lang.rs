//! Grammar selection and the tree-sitter queries, one set per language.
//!
//! Always compiled: tree-sitter is a linked parser, not a network client, so
//! it does not need a feature gate.
//!
//! The queries capture three things and no more — declarations, import
//! statements, and identifier usages. Everything structural beyond that
//! (which definition encloses a usage, what an import specifier points at) is
//! computed in Rust, because a query language cannot express "innermost
//! ancestor" or "resolve against a tsconfig".

use tree_sitter::{Language as TsLanguage, Query};

use crate::error::{Error, Result};
use crate::graph::types::{Language, SymbolKind};

/// Capture name for the identifier that names a declaration.
pub(crate) const CAP_NAME: &str = "name";
/// Capture name prefix for a whole declaration node, e.g. `def.function`.
pub(crate) const CAP_DEF_PREFIX: &str = "def.";
/// Capture name for a whole import statement, walked by the extractor.
pub(crate) const CAP_IMPORT: &str = "import.stmt";
/// Capture name for an identifier in call position.
pub(crate) const CAP_CALL: &str = "use.call";
/// Capture name for an identifier in any other position.
pub(crate) const CAP_REF: &str = "use.ref";
/// Capture name for a standalone inheritance construct, walked by the
/// extractor.
///
/// Only needed where inheritance is *not* written on the declaration itself.
/// A TypeScript class carries its own `extends` clause, so the `def.class`
/// capture already reaches it; a Rust `impl Display for Ledger` is a separate
/// item that names two types and declares neither, so nothing else would.
pub(crate) const CAP_HERITAGE: &str = "heritage.stmt";
/// Prefix for a capture that exists only to be tested by a query predicate.
///
/// Ruby has no import *statement* — `require_relative "x"` is an ordinary
/// method call — so the only way to recognise one is a `#match?` on the callee,
/// and a predicate can only test something that was captured. The extractor
/// already ignores captures it does not know; this makes that deliberate rather
/// than incidental, and keeps the vocabulary test honest.
pub(crate) const CAP_IGNORE_PREFIX: &str = "_";

/// Map the `def.<kind>` capture suffix onto a [`SymbolKind`].
pub(crate) fn symbol_kind(capture: &str) -> Option<SymbolKind> {
    match capture.strip_prefix(CAP_DEF_PREFIX)? {
        "function" => Some(SymbolKind::Function),
        "method" => Some(SymbolKind::Method),
        "class" => Some(SymbolKind::Class),
        "struct" => Some(SymbolKind::Struct),
        "enum" => Some(SymbolKind::Enum),
        "interface" => Some(SymbolKind::Interface),
        "type" => Some(SymbolKind::Type),
        "const" => Some(SymbolKind::Const),
        "module" => Some(SymbolKind::Module),
        _ => None,
    }
}

/// The tree-sitter grammar for a language.
pub(crate) fn grammar(lang: Language) -> TsLanguage {
    match lang {
        Language::Rust => tree_sitter_rust::LANGUAGE.into(),
        Language::Python => tree_sitter_python::LANGUAGE.into(),
        Language::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        Language::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
        Language::Go => tree_sitter_go::LANGUAGE.into(),
        Language::Java => tree_sitter_java::LANGUAGE.into(),
        Language::Ruby => tree_sitter_ruby::LANGUAGE.into(),
    }
}

/// The query source for a language.
///
/// TypeScript and TSX share one source because they share a node vocabulary —
/// only the grammar differs — so a fix to the TypeScript patterns cannot drift
/// away from the `.tsx` half of the same codebase.
pub(crate) fn query_source(lang: Language) -> &'static str {
    match lang {
        Language::Rust => RUST_QUERY,
        Language::Python => PYTHON_QUERY,
        Language::TypeScript | Language::Tsx => TYPESCRIPT_QUERY,
        Language::Go => GO_QUERY,
        Language::Java => JAVA_QUERY,
        Language::Ruby => RUBY_QUERY,
    }
}

/// Compile the query for a language.
pub(crate) fn query(lang: Language) -> Result<Query> {
    Query::new(&grammar(lang), query_source(lang))
        .map_err(|e| Error::Config(format!("graph query for {}: {e}", lang.tag())))
}

const RUST_QUERY: &str = r#"
(function_item name: (identifier) @name) @def.function
(struct_item name: (type_identifier) @name) @def.struct
(union_item name: (type_identifier) @name) @def.struct
(enum_item name: (type_identifier) @name) @def.enum
(trait_item name: (type_identifier) @name) @def.interface
(type_item name: (type_identifier) @name) @def.type
(const_item name: (identifier) @name) @def.const
(static_item name: (identifier) @name) @def.const
(macro_definition name: (identifier) @name) @def.function
(mod_item name: (identifier) @name) @def.module

(use_declaration) @import.stmt

(impl_item) @heritage.stmt

(call_expression function: (identifier) @use.call)
(call_expression function: (scoped_identifier name: (identifier) @use.call))
(call_expression function: (field_expression field: (field_identifier) @use.call))
(macro_invocation macro: (identifier) @use.call)
(macro_invocation macro: (scoped_identifier name: (identifier) @use.call))

(type_identifier) @use.ref
(scoped_identifier name: (identifier) @use.ref)
"#;

const PYTHON_QUERY: &str = r#"
(function_definition name: (identifier) @name) @def.function
(class_definition name: (identifier) @name) @def.class

(import_statement) @import.stmt
(import_from_statement) @import.stmt

(call function: (identifier) @use.call)
(call function: (attribute attribute: (identifier) @use.call))

(identifier) @use.ref
"#;

const TYPESCRIPT_QUERY: &str = r#"
(function_declaration name: (identifier) @name) @def.function
(generator_function_declaration name: (identifier) @name) @def.function
(class_declaration name: (type_identifier) @name) @def.class
(abstract_class_declaration name: (type_identifier) @name) @def.class
(interface_declaration name: (type_identifier) @name) @def.interface
(type_alias_declaration name: (type_identifier) @name) @def.type
(enum_declaration name: (identifier) @name) @def.enum
(method_definition name: (property_identifier) @name) @def.method
(variable_declarator name: (identifier) @name) @def.const

(import_statement) @import.stmt
(export_statement source: (string)) @import.stmt

(call_expression function: (identifier) @use.call)
(call_expression function: (member_expression property: (property_identifier) @use.call))
(new_expression constructor: (identifier) @use.call)

(identifier) @use.ref
(type_identifier) @use.ref
"#;

const GO_QUERY: &str = r#"
(function_declaration name: (identifier) @name) @def.function
(method_declaration name: (field_identifier) @name) @def.method
(type_spec name: (type_identifier) @name) @def.type
(const_spec name: (identifier) @name) @def.const
(var_spec name: (identifier) @name) @def.const

(import_declaration) @import.stmt

(call_expression function: (identifier) @use.call)
(call_expression function: (selector_expression field: (field_identifier) @use.call))

(type_identifier) @use.ref
(field_identifier) @use.ref
"#;

/// Node names cross-checked against aider's `java-tags.scm`, which is the
/// best-tested public inventory of this grammar. The declaration set is wider
/// here — aider captures classes, methods and interfaces; a graph also needs
/// enums, records and fields, or an edge to a constant resolves to nothing.
const JAVA_QUERY: &str = r#"
(class_declaration name: (identifier) @name) @def.class
(interface_declaration name: (identifier) @name) @def.interface
(annotation_type_declaration name: (identifier) @name) @def.interface
(enum_declaration name: (identifier) @name) @def.enum
(record_declaration name: (identifier) @name) @def.class
(method_declaration name: (identifier) @name) @def.method
(constructor_declaration name: (identifier) @name) @def.method
(field_declaration declarator: (variable_declarator name: (identifier) @name)) @def.const

(import_declaration) @import.stmt

(method_invocation name: (identifier) @use.call)
(object_creation_expression type: (type_identifier) @use.call)

(type_identifier) @use.ref
"#;

/// Ruby has no import statement, so the `require` family is matched as the
/// method call it actually is. The `@_require` capture exists only for the
/// predicate to test — see [`CAP_IGNORE_PREFIX`].
///
/// `(identifier) @use.ref` is deliberately absent, unlike Python and
/// TypeScript. Ruby writes local variables, method calls and attribute reads
/// with the same bare-identifier syntax, so capturing every identifier as a
/// reference would make `name` in one file a reference to `name` in every other
/// file that has an attribute by that name. Constants are capitalised and
/// therefore unambiguous, so they carry the reference edges instead.
const RUBY_QUERY: &str = r#"
(method name: (_) @name) @def.method
(singleton_method name: (_) @name) @def.method
(class name: (constant) @name) @def.class
(module name: (constant) @name) @def.module
(assignment left: (constant) @name) @def.const

((call
   method: (identifier) @_require
   arguments: (argument_list (string)))
 @import.stmt
 (#match? @_require "^(require|require_relative|load|autoload)$"))

(call method: (identifier) @use.call)

(constant) @use.ref
"#;

#[cfg(test)]
mod tests {
    use super::*;

    /// Every query must compile against its own grammar.
    ///
    /// A malformed pattern is a *runtime* error in tree-sitter, so without
    /// this test a typo in a node name would only surface as a language that
    /// silently produces no edges.
    #[test]
    fn every_query_compiles() {
        for lang in [
            Language::Rust,
            Language::Python,
            Language::TypeScript,
            Language::Tsx,
            Language::Go,
            Language::Java,
            Language::Ruby,
        ] {
            let compiled = query(lang);
            assert!(compiled.is_ok(), "{}: {:?}", lang.tag(), compiled.err());
        }
    }

    #[test]
    fn def_captures_map_to_symbol_kinds() {
        assert_eq!(symbol_kind("def.function"), Some(SymbolKind::Function));
        assert_eq!(symbol_kind("def.module"), Some(SymbolKind::Module));
        assert_eq!(symbol_kind("use.call"), None);
        assert_eq!(symbol_kind("def.nonsense"), None);
    }

    /// Every `def.*` capture a query declares must be a kind we understand, or
    /// the declaration is extracted and then thrown away.
    #[test]
    fn every_def_capture_in_every_query_is_known() {
        for lang in [
            Language::Rust,
            Language::Python,
            Language::TypeScript,
            Language::Tsx,
            Language::Go,
            Language::Java,
            Language::Ruby,
        ] {
            let compiled = query(lang).expect("query compiles");
            for capture in compiled.capture_names() {
                if capture.starts_with(CAP_DEF_PREFIX) {
                    assert!(
                        symbol_kind(capture).is_some(),
                        "{}: unknown capture {capture}",
                        lang.tag()
                    );
                } else {
                    assert!(
                        [CAP_NAME, CAP_IMPORT, CAP_CALL, CAP_REF, CAP_HERITAGE].contains(capture)
                            || capture.starts_with(CAP_IGNORE_PREFIX),
                        "{}: stray capture {capture}",
                        lang.tag()
                    );
                }
            }
        }
    }
}
