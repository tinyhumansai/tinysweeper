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
                        [CAP_NAME, CAP_IMPORT, CAP_CALL, CAP_REF].contains(capture),
                        "{}: stray capture {capture}",
                        lang.tag()
                    );
                }
            }
        }
    }
}
