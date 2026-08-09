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
    let file = parsed(
        "src/a.ts",
        "import { f } from './b';\nexport const x = f();\n",
    );
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
        file.usages.iter().any(|u| u.name == "Money" && !u.call),
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

// --- Inheritance ------------------------------------------------------------

fn heritage(file: &ParsedFile) -> Vec<(&str, &str)> {
    file.heritage
        .iter()
        .map(|h| (h.child.as_str(), h.parent.as_str()))
        .collect()
}

#[test]
fn typescript_extends_and_implements_are_both_heritage() {
    let file = parsed(
        "src/ledger.ts",
        "class Ledger extends Base implements Auditable, Serializable {}\n\
         interface Auditable extends Traceable {}\n",
    );
    assert!(heritage(&file).contains(&("Ledger", "Base")));
    assert!(heritage(&file).contains(&("Ledger", "Auditable")));
    assert!(heritage(&file).contains(&("Ledger", "Serializable")));
    assert!(heritage(&file).contains(&("Auditable", "Traceable")));
}

#[test]
fn a_generic_argument_is_not_a_base_class() {
    // `extends Repository<User>` inherits from `Repository`. Recording `User`
    // too would put every class that merely mentions it one hop from it.
    let file = parsed(
        "src/repo.ts",
        "class UserRepo extends Repository<User> {}\n",
    );
    assert_eq!(heritage(&file), vec![("UserRepo", "Repository")]);
}

#[test]
fn python_positional_bases_are_heritage_and_a_metaclass_is_not() {
    let file = parsed(
        "app/ledger.py",
        "class Ledger(Base, Auditable, metaclass=Meta):\n    pass\n",
    );
    assert!(heritage(&file).contains(&("Ledger", "Base")));
    assert!(heritage(&file).contains(&("Ledger", "Auditable")));
    assert!(
        !heritage(&file).iter().any(|(_, parent)| *parent == "Meta"),
        "{:?}",
        file.heritage
    );
}

#[test]
fn a_rust_trait_impl_relates_the_type_to_the_trait_it_implements() {
    let file = parsed(
        "src/ledger.rs",
        "pub struct Ledger;\nimpl Display for Ledger {}\nimpl Ledger { fn new() {} }\n",
    );
    assert_eq!(heritage(&file), vec![("Ledger", "Display")]);
}

#[test]
fn rust_supertraits_are_heritage() {
    let file = parsed("src/port.rs", "pub trait Store: Send + Sync + Sized {}\n");
    assert!(heritage(&file).contains(&("Store", "Send")));
    assert!(heritage(&file).contains(&("Store", "Sync")));
    assert!(
        !heritage(&file).iter().any(|(_, parent)| *parent == "Sized"),
        "every trait has it, so it relates this one to nothing: {:?}",
        file.heritage
    );
}

#[test]
fn go_embedding_is_heritage_and_a_named_field_is_not() {
    let file = parsed(
        "pkg/store/store.go",
        "type Store struct {\n\tsync.Mutex\n\tReader\n\tname string\n}\n",
    );
    assert!(heritage(&file).contains(&("Store", "Mutex")));
    assert!(heritage(&file).contains(&("Store", "Reader")));
    assert!(
        !heritage(&file)
            .iter()
            .any(|(_, parent)| *parent == "string"),
        "a named field promotes nothing: {:?}",
        file.heritage
    );
}

#[test]
fn go_an_embedded_interface_is_heritage_and_a_method_signature_is_not() {
    let file = parsed(
        "pkg/io/reader.go",
        "type Reader interface {\n\tio.Closer\n\tRead(p []byte) (n int, err error)\n}\n",
    );
    assert!(heritage(&file).contains(&("Reader", "Closer")));
    assert!(
        !heritage(&file)
            .iter()
            .any(|(_, parent)| *parent == "Read"),
        "a method signature declares behaviour; it embeds nothing: {:?}",
        file.heritage
    );
}

#[test]
fn rust_a_lifetime_bound_is_not_a_supertrait() {
    // `'static` constrains how long the trait can be stored, it is not a
    // parent. Every trait gets the bound family or none of them; recording it
    // as heritage would relate each trait to nothing local.
    let file = parsed(
        "src/cache.rs",
        "pub trait Cache: IntoIterator + Send + 'static {}\n",
    );
    assert!(heritage(&file).contains(&("Cache", "IntoIterator")));
    assert!(heritage(&file).contains(&("Cache", "Send")));
    assert!(
        !heritage(&file)
            .iter()
            .any(|(_, parent)| *parent == "'static"),
        "{:?}",
        file.heritage
    );
}

// --- Test scopes ------------------------------------------------------------

fn scope_of(file: &ParsedFile, name: &str) -> bool {
    let def = file
        .defs
        .iter()
        .find(|d| d.name == name)
        .unwrap_or_else(|| panic!("no definition named {name} in {:?}", names(file)));
    file.in_test_scope(def.start_byte)
}

#[test]
fn a_rust_cfg_test_module_makes_everything_inside_it_a_test_scope() {
    let file = parsed(
        "src/ledger.rs",
        "pub fn settle() {}\n\
         #[cfg(test)]\nmod tests {\n    #[test]\n    fn settles() {}\n}\n",
    );
    assert!(scope_of(&file, "settles"));
    assert!(!scope_of(&file, "settle"), "production code is not a test");
}

#[test]
fn an_attribute_whose_text_mentions_test_does_not_make_a_test_scope() {
    // The word matching `test` inside string literals is configuration, not a
    // test marker: `cfg(feature = "test")` gates a feature and `serde(rename)`
    // names a serialised field. Only the attribute path — or a bare `test`
    // argument to `cfg` — counts.
    let file = parsed(
        "src/ledger.rs",
        "pub fn settle() {}\n\
         #[cfg(feature = \"test\")]\nfn only_in_test_feature() {}\n\
         #[serde(rename = \"test\")]\npub struct Renamed {}\n\
         #[test]\nfn always_a_test() {}\n\
         #[tokio::test]\nasync fn also_a_test() {}\n",
    );
    assert!(!scope_of(&file, "settle"), "production code is not a test");
    assert!(
        !scope_of(&file, "only_in_test_feature"),
        "a feature flag named test is not a test scope"
    );
    assert!(
        !scope_of(&file, "Renamed"),
        "a serde rename mentioning test is not a test scope"
    );
    assert!(scope_of(&file, "always_a_test"));
    assert!(
        scope_of(&file, "also_a_test"),
        "`tokio::test` is still a test scope"
    );
}

#[test]
fn go_and_python_test_names_follow_their_runners_rules() {
    let go = parsed(
        "pkg/store/store.go",
        "func TestStore(t *testing.T) {}\nfunc Testing() {}\nfunc BenchmarkStore(b *B) {}\n",
    );
    assert!(scope_of(&go, "TestStore"));
    assert!(scope_of(&go, "BenchmarkStore"));
    assert!(
        !scope_of(&go, "Testing"),
        "`go test` does not run it either"
    );

    let python = parsed(
        "app/ledger.py",
        "def test_settles():\n    pass\n\ndef settle():\n    pass\n",
    );
    assert!(scope_of(&python, "test_settles"));
    assert!(!scope_of(&python, "settle"));
}

#[test]
fn a_typescript_test_file_is_a_test_scope_even_though_its_cases_are_anonymous() {
    // The case this exists for: `it("...", () => ...)` names no definition, so
    // without the file-level flag a Jest suite would cover nothing at all.
    let file = parsed(
        "src/lib/math.test.ts",
        "import { total } from './math';\nit('adds', () => { total(); });\n",
    );
    assert!(file.test_file);
    assert!(file.in_test_scope(0));

    let production = parsed("src/lib/math.ts", "export function total() { return 1; }\n");
    assert!(!production.test_file);
    assert!(!production.in_test_scope(0));
}
