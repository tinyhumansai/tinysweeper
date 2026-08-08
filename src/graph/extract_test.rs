//! Extraction tests: one per language, plus the enclosing-symbol attribution
//! that gives a `calls` edge a symbol on its source side.

use super::*;
use crate::graph::types::SymbolKind;

fn parsed(path: &str, text: &str) -> ParsedFile {
    parse(&SourceFile::new(path, text))
        .expect("parses")
        .expect("a known language")
}

fn names(file: &ParsedFile) -> Vec<&str> {
    file.defs.iter().map(|d| d.name.as_str()).collect()
}

fn specifiers(file: &ParsedFile) -> Vec<&str> {
    file.imports.iter().map(|i| i.specifier.as_str()).collect()
}

fn calls(file: &ParsedFile) -> Vec<&str> {
    file.usages
        .iter()
        .filter(|u| u.call)
        .map(|u| u.name.as_str())
        .collect()
}

#[test]
fn unknown_extensions_are_not_an_error() {
    assert!(
        parse(&SourceFile::new("README.md", "# hi"))
            .expect("no error")
            .is_none()
    );
}

#[test]
fn typescript_definitions_imports_and_calls() {
    let file = parsed(
        "src/app/page.ts",
        r#"
import { computeTotal, type Money } from "@/lib/math";
import Widget from "../ui/widget";
import * as helpers from "@/lib/helpers";

export interface Cart { items: number }

export function render(cart: Cart): number {
  return computeTotal(cart.items);
}

export const total = () => render({ items: 1 });
"#,
    );

    assert!(names(&file).contains(&"render"));
    assert!(names(&file).contains(&"Cart"));
    assert!(names(&file).contains(&"total"));
    assert_eq!(
        specifiers(&file),
        vec!["@/lib/math", "../ui/widget", "@/lib/helpers"]
    );
    assert!(calls(&file).contains(&"computeTotal"));
    assert!(calls(&file).contains(&"render"));

    let math = &file.imports[0];
    assert_eq!(math.names, vec!["computeTotal", "Money"]);
    assert_eq!(file.imports[1].names, vec!["Widget"]);
    assert_eq!(file.imports[2].names, vec!["helpers"]);
}

#[test]
fn typescript_aliased_import_binds_the_local_name() {
    let file = parsed(
        "src/a.ts",
        r#"import { computeTotal as sum } from "@/lib/math";
export function go() { return sum(1); }"#,
    );
    // The *local* name is what a later call uses; binding the exported name
    // instead would leave `sum(1)` unattributable.
    assert_eq!(file.imports[0].names, vec!["sum"]);
    assert!(calls(&file).contains(&"sum"));
}

#[test]
fn tsx_grammar_handles_jsx_files() {
    let file = parsed(
        "src/ui/widget.tsx",
        r#"
import { label } from "@/lib/text";

export function Widget() {
  return <div title={label()}>hi</div>;
}
"#,
    );
    assert!(names(&file).contains(&"Widget"));
    assert_eq!(specifiers(&file), vec!["@/lib/text"]);
    assert!(calls(&file).contains(&"label"));
}

#[test]
fn rust_use_trees_flatten_to_one_specifier_per_binding() {
    let file = parsed(
        "src/graph/build.rs",
        r#"
use crate::graph::{resolve::Resolver, types::RepoGraph};
use std::collections::BTreeMap;
use super::path::dir_of;

mod helpers;

pub fn build() -> RepoGraph {
    let _ = dir_of("a/b");
    RepoGraph::default()
}
"#,
    );

    let found = specifiers(&file);
    assert!(found.contains(&"crate::graph::resolve::Resolver"));
    assert!(found.contains(&"crate::graph::types::RepoGraph"));
    assert!(found.contains(&"std::collections::BTreeMap"));
    assert!(found.contains(&"super::path::dir_of"));
    // A bodyless `mod` is marked so the resolver bases it on the current
    // module directory rather than the crate root.
    assert!(found.contains(&"mod helpers"));
    assert!(names(&file).contains(&"build"));
}

#[test]
fn rust_wildcard_and_alias_uses_are_recorded() {
    let file = parsed(
        "src/lib.rs",
        "use crate::prelude::*;\nuse crate::error::Error as Failure;\n",
    );
    let found = specifiers(&file);
    assert!(found.contains(&"crate::prelude::*"));
    assert!(found.contains(&"crate::error::Error"));
}

#[test]
fn python_absolute_and_relative_imports() {
    let file = parsed(
        "pkg/service.py",
        r#"
import os
import pkg.util as util
from .models import Order, Line
from ..shared import config

class Service:
    def run(self):
        return build_order()
"#,
    );

    let found = specifiers(&file);
    assert!(found.contains(&"os"));
    assert!(found.contains(&"pkg.util"));
    assert!(found.contains(&".models"));
    assert!(found.contains(&"..shared"));

    let models = file
        .imports
        .iter()
        .find(|i| i.specifier == ".models")
        .expect("relative import");
    assert_eq!(models.names, vec!["Order", "Line"]);

    assert!(names(&file).contains(&"Service"));
    assert!(names(&file).contains(&"run"));
    assert!(calls(&file).contains(&"build_order"));
}

#[test]
fn go_import_blocks_yield_every_spec() {
    let file = parsed(
        "cmd/main.go",
        r#"
package main

import (
	"fmt"
	"example.com/app/internal/store"
)

func main() {
	fmt.Println(store.Load())
}
"#,
    );
    assert_eq!(
        specifiers(&file),
        vec!["fmt", "example.com/app/internal/store"]
    );
    assert!(names(&file).contains(&"main"));
    assert!(calls(&file).contains(&"Load"));
}

#[test]
fn usages_attribute_to_the_innermost_definition() {
    let file = parsed(
        "src/a.ts",
        r#"
export class Service {
  run() {
    return helper();
  }
}
function helper() { return 1; }
"#,
    );
    let call = file
        .usages
        .iter()
        .find(|u| u.name == "helper" && u.call)
        .expect("the call");
    let enclosing = file.enclosing(call.byte).expect("an enclosing definition");
    // The method, not the class: the class also contains the byte, and picking
    // the outermost would lose which method actually calls what.
    assert_eq!(enclosing.name, "run");
    assert_eq!(enclosing.kind, SymbolKind::Method);
}

#[test]
fn a_call_never_also_counts_as_a_reference() {
    let file = parsed("src/a.ts", "import { f } from './b';\nexport const x = f();\n");
    let occurrences: Vec<&Usage> = file.usages.iter().filter(|u| u.name == "f").collect();
    assert_eq!(occurrences.len(), 1, "{occurrences:?}");
    assert!(occurrences[0].call);
}

#[test]
fn references_are_kept_for_non_call_usages() {
    let file = parsed(
        "src/a.ts",
        "import { Money } from './money';\nexport function price(): Money { return 0 as Money; }\n",
    );
    assert!(
        file.usages
            .iter()
            .any(|u| u.name == "Money" && !u.call),
        "{:?}",
        file.usages
    );
}

#[test]
fn a_syntax_error_still_yields_what_parsed() {
    // tree-sitter recovers rather than failing, and a pull request under review
    // is exactly where half-written code shows up.
    let file = parsed(
        "src/a.ts",
        "import { a } from './b';\nexport function ok() {}\nfunction broken( {",
    );
    assert_eq!(specifiers(&file), vec!["./b"]);
    assert!(names(&file).contains(&"ok"));
}
