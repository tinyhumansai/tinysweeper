//! End-to-end graph tests: source in, nodes and edges out, then a traversal
//! back over what was stored.
//!
//! The decisive test is
//! [`alias_imported_callee_is_reachable_from_its_caller`]: a call across a
//! *path alias*, asserted all the way from the source text to a `neighbours`
//! walk that finds the caller from the callee. That is the case a
//! relative-path-only resolver cannot see at all, and it is the case a review
//! needs most, because the caller shares no vocabulary with the diff and
//! similarity search will never surface it.

use super::*;

use crate::graph::traverse::{NeighbourQuery, neighbours};
use crate::index::MockGraphStore;

const REPO: &str = "tinyhumansai/example";

fn files(entries: &[(&str, &str)]) -> Vec<SourceFile> {
    entries
        .iter()
        .map(|(path, text)| SourceFile::new(*path, *text))
        .collect()
}

fn has_edge(graph: &RepoGraph, from: &str, to: &str, kind: EdgeKind) -> bool {
    graph
        .edges
        .iter()
        .any(|e| e.from == from && e.to == to && e.kind == kind)
}

fn node_ids(graph: &RepoGraph) -> Vec<&str> {
    graph.nodes.iter().map(|n| n.id.as_str()).collect()
}

/// The alias-crossing fixture: `src/app/page.ts` imports and calls
/// `computeTotal`, defined in `src/lib/math.ts`, through the `@/*` alias.
fn alias_repo() -> Vec<SourceFile> {
    files(&[
        (
            "tsconfig.json",
            r#"{"compilerOptions":{"baseUrl":".","paths":{"@/*":["src/*"]}}}"#,
        ),
        (
            "src/lib/math.ts",
            "export function computeTotal(items: number[]): number {\n  return items.length;\n}\n",
        ),
        (
            "src/app/page.ts",
            "import { computeTotal } from \"@/lib/math\";\n\
             export function render(items: number[]): number {\n  return computeTotal(items);\n}\n",
        ),
    ])
}

#[test]
fn alias_imported_call_produces_a_symbol_level_edge() {
    let graph = build(REPO, &alias_repo()).expect("builds");

    assert!(
        has_edge(
            &graph,
            "src/app/page.ts",
            "src/lib/math.ts",
            EdgeKind::Imports
        ),
        "no imports edge across the alias: {:?}",
        graph.edges
    );
    assert!(
        has_edge(
            &graph,
            "src/app/page.ts#render",
            "src/lib/math.ts#computeTotal",
            EdgeKind::Calls
        ),
        "no calls edge across the alias: {:?}",
        graph.edges
    );
    assert!(node_ids(&graph).contains(&"src/lib/math.ts#computeTotal"));
    // The only import in the fixture is the aliased one, and it resolved.
    assert_eq!(graph.coverage.import_resolution_rate(), 1.0);
    assert!(graph.unresolved.is_empty(), "{:?}", graph.unresolved);
}

/// Seed the graph on the *callee* and get the caller back.
///
/// This is the acceptance criterion for the whole module. A graph that only
/// draws a diagram never runs this query; a resolver that gives up on
/// non-relative specifiers cannot answer it.
#[tokio::test]
async fn alias_imported_callee_is_reachable_from_its_caller() {
    let graph = build(REPO, &alias_repo()).expect("builds");
    let store = MockGraphStore::new();
    sync_all(&store, REPO, &graph).await.expect("stored");

    let query = NeighbourQuery::new(["src/lib/math.ts#computeTotal".to_string()]).hops(1);
    let hood = neighbours(&store, REPO, &query).await.expect("traverses");

    let reached: Vec<&str> = hood.nodes.iter().map(|n| n.id.as_str()).collect();
    assert!(
        reached.contains(&"src/app/page.ts#render"),
        "the aliased caller was not reachable from the callee: {reached:?}"
    );
}

#[tokio::test]
async fn a_relative_only_resolver_would_have_found_nothing_here() {
    // Guards the fixture rather than the code: if someone "simplifies" the
    // fixture to a relative import, the test above stops proving anything.
    let sources = alias_repo();
    let page = sources
        .iter()
        .find(|f| f.path == "src/app/page.ts")
        .expect("the caller");
    assert!(page.text.contains("\"@/lib/math\""));
    assert!(!page.text.contains("./"));
    assert!(!page.text.contains("../"));
}

#[test]
fn defines_edges_connect_a_file_to_each_of_its_symbols() {
    let graph = build(REPO, &alias_repo()).expect("builds");
    assert!(has_edge(
        &graph,
        "src/lib/math.ts",
        "src/lib/math.ts#computeTotal",
        EdgeKind::Defines
    ));
    assert!(has_edge(
        &graph,
        "src/app/page.ts",
        "src/app/page.ts#render",
        EdgeKind::Defines
    ));
}

#[test]
fn every_edge_endpoint_exists_as_a_node() {
    let graph = build(REPO, &alias_repo()).expect("builds");
    let ids: Vec<&str> = node_ids(&graph);
    for edge in &graph.edges {
        assert!(ids.contains(&edge.from.as_str()), "dangling from {edge:?}");
        assert!(ids.contains(&edge.to.as_str()), "dangling to {edge:?}");
    }
}

#[test]
fn a_local_definition_shadows_an_import_of_the_same_name() {
    let graph = build(
        REPO,
        &files(&[
            ("src/lib/math.ts", "export function total() { return 1; }\n"),
            (
                "src/app/page.ts",
                "import { total } from \"../lib/math\";\n\
                 function total() { return 2; }\n\
                 export function go() { return total(); }\n",
            ),
        ]),
    )
    .expect("builds");

    assert!(has_edge(
        &graph,
        "src/app/page.ts#go",
        "src/app/page.ts#total",
        EdgeKind::Calls
    ));
    assert!(!has_edge(
        &graph,
        "src/app/page.ts#go",
        "src/lib/math.ts#total",
        EdgeKind::Calls
    ));
}

#[test]
fn a_non_call_usage_becomes_a_references_edge() {
    let graph = build(
        REPO,
        &files(&[
            ("src/money.ts", "export type Money = number;\n"),
            (
                "src/price.ts",
                "import { Money } from \"./money\";\nexport function price(): Money { return 0; }\n",
            ),
        ]),
    )
    .expect("builds");

    assert!(
        has_edge(
            &graph,
            "src/price.ts#price",
            "src/money.ts#Money",
            EdgeKind::References
        ),
        "{:?}",
        graph.edges
    );
}

#[test]
fn unresolved_specifiers_are_recorded_with_a_reason() {
    let graph = build(
        REPO,
        &files(&[(
            "src/a.ts",
            "import react from \"react\";\nimport { gone } from \"./missing\";\n",
        )]),
    )
    .expect("builds");

    let reasons: Vec<(&str, UnresolvedReason)> = graph
        .unresolved
        .iter()
        .map(|u| (u.specifier.as_str(), u.reason))
        .collect();
    assert!(reasons.contains(&("react", UnresolvedReason::External)));
    assert!(reasons.contains(&("./missing", UnresolvedReason::NoSuchFile)));

    // Coverage excludes the package from the denominator and counts the broken
    // relative import as the one internal failure.
    assert_eq!(graph.coverage.imports_total, 2);
    assert_eq!(graph.coverage.imports_external, 1);
    assert_eq!(graph.coverage.imports_resolved, 0);
    assert_eq!(graph.coverage.import_resolution_rate(), 0.0);
    assert_eq!(graph.coverage.unresolved_rate(), 1.0);
}

#[test]
fn coverage_over_a_repository_with_no_imports_is_not_a_division_by_zero() {
    let graph = build(REPO, &files(&[("src/a.ts", "export const x = 1;\n")])).expect("builds");
    assert_eq!(graph.coverage.import_resolution_rate(), 1.0);
}

#[test]
fn a_rust_repository_resolves_its_own_module_tree() {
    let graph = build(
        REPO,
        &files(&[
            ("Cargo.toml", "[package]\nname = \"demo\"\n"),
            ("src/lib.rs", "pub mod graph;\n"),
            (
                "src/graph/mod.rs",
                "pub mod types;\nuse crate::graph::types::Definition;\n\
                 pub fn build() -> Definition { make() }\nfn make() -> Definition { todo!() }\n",
            ),
            (
                "src/graph/types.rs",
                "pub struct Definition { pub name: String }\n",
            ),
        ]),
    )
    .expect("builds");

    assert!(has_edge(
        &graph,
        "src/lib.rs",
        "src/graph/mod.rs",
        EdgeKind::Imports
    ));
    assert!(has_edge(
        &graph,
        "src/graph/mod.rs",
        "src/graph/types.rs",
        EdgeKind::Imports
    ));
    assert!(has_edge(
        &graph,
        "src/graph/mod.rs#build",
        "src/graph/mod.rs#make",
        EdgeKind::Calls
    ));
    assert!(has_edge(
        &graph,
        "src/graph/mod.rs#build",
        "src/graph/types.rs#Definition",
        EdgeKind::References
    ));
}

#[test]
fn a_python_repository_resolves_relative_packages() {
    let graph = build(
        REPO,
        &files(&[
            ("pkg/__init__.py", ""),
            ("pkg/models.py", "class Order:\n    pass\n"),
            (
                "pkg/service.py",
                "from .models import Order\n\ndef run():\n    return Order()\n",
            ),
        ]),
    )
    .expect("builds");

    assert!(has_edge(
        &graph,
        "pkg/service.py",
        "pkg/models.py",
        EdgeKind::Imports
    ));
    assert!(
        has_edge(
            &graph,
            "pkg/service.py#run",
            "pkg/models.py#Order",
            EdgeKind::Calls
        ),
        "{:?}",
        graph.edges
    );
}

#[test]
fn a_go_repository_resolves_module_relative_packages() {
    let graph = build(
        REPO,
        &files(&[
            ("go.mod", "module example.com/app\n"),
            (
                "internal/store/store.go",
                "package store\n\nfunc Load() int { return 1 }\n",
            ),
            (
                "cmd/main.go",
                "package main\n\nimport (\n\t\"example.com/app/internal/store\"\n)\n\n\
                 func main() {\n\t_ = store.Load()\n}\n",
            ),
        ]),
    )
    .expect("builds");

    assert!(has_edge(
        &graph,
        "cmd/main.go",
        "internal/store/store.go",
        EdgeKind::Imports
    ));
    assert!(
        has_edge(
            &graph,
            "cmd/main.go#main",
            "internal/store/store.go#Load",
            EdgeKind::Calls
        ),
        "{:?}",
        graph.edges
    );
}

#[test]
fn files_in_languages_we_do_not_parse_are_skipped_without_error() {
    let graph = build(
        REPO,
        &files(&[("README.md", "# hi"), ("Makefile", "all:\n\techo hi\n")]),
    )
    .expect("builds");
    assert!(graph.nodes.is_empty());
    assert!(graph.edges.is_empty());
}

#[test]
fn building_twice_produces_the_same_graph() {
    // Ids are deterministic, so a re-index writes the same rows rather than
    // accumulating duplicates.
    let first = build(REPO, &alias_repo()).expect("builds");
    let second = build(REPO, &alias_repo()).expect("builds");
    assert_eq!(first, second);
}

// --- storage ----------------------------------------------------------------

#[tokio::test]
async fn sync_all_replaces_the_whole_repository() {
    let store = MockGraphStore::new();
    let graph = build(REPO, &alias_repo()).expect("builds");
    sync_all(&store, REPO, &graph).await.expect("stored");
    assert_eq!(store.node_count(), graph.nodes.len());
    assert_eq!(store.edge_count(), graph.edges.len());

    let smaller = build(REPO, &files(&[("src/a.ts", "export const x = 1;\n")])).expect("builds");
    sync_all(&store, REPO, &smaller).await.expect("stored");
    assert_eq!(store.node_count(), smaller.nodes.len());
}

#[tokio::test]
async fn sync_paths_replaces_only_the_changed_files() {
    let store = MockGraphStore::new();
    let before = build(REPO, &alias_repo()).expect("builds");
    sync_all(&store, REPO, &before).await.expect("stored");

    // `render` is renamed to `draw`. Without the delete-by-path the graph would
    // keep both, and traversal would keep offering a symbol that is gone.
    let mut sources = alias_repo();
    for file in &mut sources {
        if file.path == "src/app/page.ts" {
            file.text = file.text.replace("render", "draw");
        }
    }
    let after = build(REPO, &sources).expect("builds");
    sync_paths(&store, REPO, &after, &["src/app/page.ts".to_string()])
        .await
        .expect("stored");

    let hood = store
        .neighbours(REPO, &["src/app/page.ts".to_string()], 1, &EdgeKind::ALL)
        .await
        .expect("traverses");
    let ids: Vec<&str> = hood.nodes.iter().map(|n| n.id.as_str()).collect();
    assert!(ids.contains(&"src/app/page.ts#draw"), "{ids:?}");
    assert!(!ids.contains(&"src/app/page.ts#render"), "{ids:?}");
    // The untouched file kept its symbols.
    assert!(
        store
            .neighbours(REPO, &["src/lib/math.ts".to_string()], 1, &EdgeKind::ALL)
            .await
            .expect("traverses")
            .nodes
            .iter()
            .any(|n| n.id == "src/lib/math.ts#computeTotal")
    );
}

#[tokio::test]
async fn sync_paths_rebuilds_edges_from_unchanged_callers() {
    let store = MockGraphStore::new();
    let before = build(
        REPO,
        &files(&[
            (
                "src/a.ts",
                "import { old } from './b'; export function call() { old(); }",
            ),
            ("src/b.ts", "export function old() {}"),
        ]),
    )
    .expect("builds");
    sync_all(&store, REPO, &before).await.expect("stored");

    let after = build(
        REPO,
        &files(&[
            (
                "src/a.ts",
                "import { renamed } from './b'; export function call() { renamed(); }",
            ),
            ("src/b.ts", "export function renamed() {}"),
        ]),
    )
    .expect("builds");
    sync_paths(&store, REPO, &after, &["src/b.ts".to_string()])
        .await
        .expect("stored");

    let hood = store
        .neighbours(REPO, &["src/b.ts".to_string()], 2, &EdgeKind::ALL)
        .await
        .expect("traverses");
    assert!(hood.edges.iter().all(|edge| edge.to != "src/b.ts#old"));
    assert!(hood.edges.iter().any(|edge| {
        edge.from == "src/a.ts#call"
            && edge.to == "src/b.ts#renamed"
            && edge.kind == EdgeKind::Calls
    }));
}

#[tokio::test]
async fn sync_paths_with_no_paths_writes_nothing() {
    let store = MockGraphStore::new();
    let graph = build(REPO, &alias_repo()).expect("builds");
    assert_eq!(sync_paths(&store, REPO, &graph, &[]).await.expect("ok"), 0);
    assert_eq!(store.node_count(), 0);
}

#[test]
fn file_nodes_are_the_file_layer_only() {
    let graph = build(REPO, &alias_repo()).expect("builds");
    let paths: Vec<&str> = file_nodes(&graph).iter().map(|n| n.id.as_str()).collect();
    assert_eq!(paths, ["src/app/page.ts", "src/lib/math.ts"]);
}

// --- a real repository ------------------------------------------------------

/// Read this crate's own `src/` tree plus the root config files.
fn this_crate() -> Vec<SourceFile> {
    fn walk(dir: &std::path::Path, root: &std::path::Path, out: &mut Vec<SourceFile>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, root, out);
            } else if let Ok(text) = std::fs::read_to_string(&path) {
                let relative = path
                    .strip_prefix(root)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .replace('\\', "/");
                out.push(SourceFile::new(relative, text));
            }
        }
    }

    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files = Vec::new();
    walk(&root.join("src"), root, &mut files);
    if let Ok(text) = std::fs::read_to_string(root.join("Cargo.toml")) {
        files.push(SourceFile::new("Cargo.toml", text));
    }
    files
}

/// Coverage on a real codebase, not a fixture.
///
/// This is the honesty check on the whole resolver. A graph builder is easy to
/// make look good on three hand-written files; the question that matters is
/// whether it can follow the imports of a repository nobody wrote for it. Every
/// `crate::`, `super::`, `self::`, `mod`, and `#[path]` module in this crate
/// has to land on a file. At the time of writing the rate is exactly 1.0; the
/// threshold sits a little below that so one unusual module in a future commit
/// reports as a gap to fix rather than as a broken build for whoever wrote it.
#[test]
fn this_repository_resolves_every_internal_import() {
    let files = this_crate();
    assert!(
        files.len() > 50,
        "expected a real tree, got {}",
        files.len()
    );
    let graph = build(REPO, &files).expect("builds");

    let internal = graph.coverage.imports_total - graph.coverage.imports_external;
    assert!(internal > 200, "expected real imports, got {internal}");
    assert!(
        graph.coverage.import_resolution_rate() >= 0.98,
        "internal import resolution fell to {:.4}; unresolved: {:?}",
        graph.coverage.import_resolution_rate(),
        graph
            .unresolved
            .iter()
            .filter(|u| u.reason != UnresolvedReason::External)
            .collect::<Vec<_>>()
    );
    assert!(graph.edges.len() > 1000, "{}", graph.edges.len());
}

#[test]
fn a_path_attribute_module_resolves_to_the_file_it_names() {
    let graph = build(
        REPO,
        &files(&[
            ("Cargo.toml", "[package]\nname = \"demo\"\n"),
            (
                "src/lib.rs",
                "#[cfg(test)]\n#[path = \"lib_test.rs\"]\nmod tests;\n",
            ),
            ("src/lib_test.rs", "fn nothing() {}\n"),
        ]),
    )
    .expect("builds");

    assert!(has_edge(
        &graph,
        "src/lib.rs",
        "src/lib_test.rs",
        EdgeKind::Imports
    ));
    assert!(graph.unresolved.is_empty(), "{:?}", graph.unresolved);
}

#[test]
fn use_super_inside_an_inline_module_is_not_a_cross_file_import() {
    let graph = build(
        REPO,
        &files(&[
            ("Cargo.toml", "[package]\nname = \"demo\"\n"),
            (
                "src/lib.rs",
                "pub fn go() {}\n\n#[cfg(test)]\nmod tests {\n    use super::*;\n\n    #[test]\n    fn works() { go(); }\n}\n",
            ),
        ]),
    )
    .expect("builds");

    // The inline module *is* the file, so `super::*` names nothing new. Left
    // as an import it would be an unresolvable module forever.
    assert!(graph.unresolved.is_empty(), "{:?}", graph.unresolved);
    assert_eq!(graph.coverage.imports_total, 0);
}

#[test]
fn a_top_level_use_super_still_resolves_to_the_parent_module() {
    let graph = build(
        REPO,
        &files(&[
            ("Cargo.toml", "[package]\nname = \"demo\"\n"),
            ("src/lib.rs", "pub mod graph;\n"),
            ("src/graph/mod.rs", "pub mod types;\npub fn shared() {}\n"),
            ("src/graph/types.rs", "use super::shared;\n"),
        ]),
    )
    .expect("builds");

    assert!(has_edge(
        &graph,
        "src/graph/types.rs",
        "src/graph/mod.rs",
        EdgeKind::Imports
    ));
}

// --- Inheritance and coverage edges -----------------------------------------

#[test]
fn an_implementation_is_reachable_inbound_from_the_trait_it_implements() {
    // The blast radius a review wants: change `Store`, and the graph names
    // every type that has to change with it — across files, which is where a
    // similarity query has nothing to go on.
    let graph = build(
        REPO,
        &files(&[
            ("src/ports/store.rs", "pub trait Store {}\n"),
            (
                "src/mongo.rs",
                "use crate::ports::store::Store;\npub struct Mongo;\nimpl Store for Mongo {}\n",
            ),
        ]),
    )
    .expect("builds");

    assert!(
        has_edge(
            &graph,
            "src/mongo.rs#Mongo",
            "src/ports/store.rs#Store",
            EdgeKind::Extends
        ),
        "no extends edge across the crate: {:?}",
        graph.edges
    );
}

#[test]
fn a_test_that_calls_a_function_covers_it() {
    let graph = build(
        REPO,
        &files(&[
            (
                "src/lib/math.ts",
                "export function computeTotal(n: number[]): number { return n.length; }\n",
            ),
            (
                "src/lib/math.test.ts",
                "import { computeTotal } from './math';\n\
                 it('adds', () => { computeTotal([]); });\n",
            ),
        ]),
    )
    .expect("builds");

    assert!(
        has_edge(
            &graph,
            "src/lib/math.test.ts",
            "src/lib/math.ts#computeTotal",
            EdgeKind::Tests
        ),
        "the suite covers nothing: {:?}",
        graph.edges
    );
}

#[test]
fn production_code_never_claims_to_test_anything() {
    // The failure this guards is the expensive one: a coverage edge out of
    // non-test code would tell a reviewer a change is already exercised when
    // nothing exercises it.
    let graph = build(
        REPO,
        &files(&[
            ("src/lib/math.ts", "export function total(): number { return 1; }\n"),
            (
                "src/app/page.ts",
                "import { total } from '../lib/math';\nexport function render() { return total(); }\n",
            ),
        ]),
    )
    .expect("builds");

    assert!(
        !graph.edges.iter().any(|e| e.kind == EdgeKind::Tests),
        "{:?}",
        graph.edges
    );
}

#[test]
fn a_test_helper_called_by_a_test_is_not_reported_as_covered_code() {
    // Both ends are test code. Recording that would make a test file look like
    // the thing under test, and put test helpers in the blast radius of a
    // production change.
    let graph = build(
        REPO,
        &files(&[
            (
                "src/lib/support.test.ts",
                "export function fixture(): number { return 1; }\n",
            ),
            (
                "src/lib/math.test.ts",
                "import { fixture } from './support.test';\nit('adds', () => { fixture(); });\n",
            ),
        ]),
    )
    .expect("builds");

    assert!(
        !graph.edges.iter().any(|e| e.kind == EdgeKind::Tests),
        "{:?}",
        graph.edges
    );
}

#[test]
fn this_repositorys_own_tests_cover_its_own_code() {
    // Measured on the real tree rather than a fixture, for the same reason
    // `this_repository_resolves_every_internal_import` is: the conventions
    // being detected are this codebase's, and a fixture cannot fail when they
    // drift.
    let graph = build(REPO, &this_crate()).expect("builds");

    let covered: std::collections::BTreeSet<&str> = graph
        .edges
        .iter()
        .filter(|e| e.kind == EdgeKind::Tests)
        .map(|e| e.to.as_str())
        .collect();

    assert!(
        covered.len() > 100,
        "only {} symbols have a coverage edge; the in-crate test convention is not being seen",
        covered.len()
    );
    assert!(
        graph.edges.iter().any(|e| e.kind == EdgeKind::Extends),
        "no inheritance edges at all on a codebase built out of ports and impls"
    );
}
