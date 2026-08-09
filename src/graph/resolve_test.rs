//! Resolution tests, one language at a time, plus the unresolved bookkeeping
//! that makes coverage measurable.

use super::*;

fn resolver(paths: &[&str]) -> Resolver {
    Resolver::new(
        &paths
            .iter()
            .map(|p| SourceFile::new(*p, ""))
            .collect::<Vec<_>>(),
    )
}

fn resolver_with(entries: &[(&str, &str)]) -> Resolver {
    Resolver::new(
        &entries
            .iter()
            .map(|(p, t)| SourceFile::new(*p, *t))
            .collect::<Vec<_>>(),
    )
}

fn one(resolution: &Resolution) -> &str {
    match resolution.targets() {
        [single] => single,
        other => panic!("expected exactly one target, got {other:?}"),
    }
}

// --- TypeScript -------------------------------------------------------------

#[test]
fn ts_relative_specifiers_infer_the_extension() {
    let r = resolver(&["src/app/page.ts", "src/lib/math.ts"]);
    let resolved = r.resolve("src/app/page.ts", Language::TypeScript, "../lib/math");
    assert_eq!(one(&resolved), "src/lib/math.ts");
}

#[test]
fn ts_directory_specifiers_fall_back_to_index() {
    let r = resolver(&["src/app/page.ts", "src/lib/index.ts"]);
    let resolved = r.resolve("src/app/page.ts", Language::TypeScript, "../lib");
    assert_eq!(one(&resolved), "src/lib/index.ts");
}

/// **The case a relative-only regex cannot see.**
///
/// This is the exact shape that makes the obvious implementation of a
/// repository graph edgeless: `@/lib/math` is not a relative path, so a
/// resolver that bails on anything not starting with `.` records nothing —
/// while the codebase writes almost every internal import this way.
#[test]
fn ts_path_alias_resolves_where_a_relative_only_regex_bails() {
    let r = resolver_with(&[
        (
            "tsconfig.json",
            r#"{"compilerOptions":{"baseUrl":".","paths":{"@/*":["src/*"]}}}"#,
        ),
        ("src/app/page.ts", ""),
        ("src/lib/math.ts", ""),
    ]);
    let resolved = r.resolve("src/app/page.ts", Language::TypeScript, "@/lib/math");
    assert_eq!(one(&resolved), "src/lib/math.ts");
    assert!(!resolved.is_external());
}

#[test]
fn ts_base_url_resolves_a_bare_internal_specifier() {
    let r = resolver_with(&[
        ("tsconfig.json", r#"{"compilerOptions":{"baseUrl":"src"}}"#),
        ("src/app/page.ts", ""),
        ("src/lib/math.ts", ""),
    ]);
    let resolved = r.resolve("src/app/page.ts", Language::TypeScript, "lib/math");
    assert_eq!(one(&resolved), "src/lib/math.ts");
}

#[test]
fn ts_packages_are_reported_as_external_not_as_failures() {
    let r = resolver(&["src/app/page.ts"]);
    let resolved = r.resolve("src/app/page.ts", Language::TypeScript, "react");
    assert_eq!(resolved, Resolution::Unresolved(UnresolvedReason::External));
    assert!(resolved.is_external());
}

#[test]
fn ts_a_matched_alias_pointing_nowhere_is_internal_and_missing() {
    let r = resolver_with(&[
        (
            "tsconfig.json",
            r#"{"compilerOptions":{"baseUrl":".","paths":{"@/*":["src/*"]}}}"#,
        ),
        ("src/app/page.ts", ""),
    ]);
    // Not "external": it matched a configured alias, so calling it a package
    // would hide a real gap behind an expected one.
    assert_eq!(
        r.resolve("src/app/page.ts", Language::TypeScript, "@/lib/gone"),
        Resolution::Unresolved(UnresolvedReason::NoSuchFile)
    );
}

#[test]
fn ts_root_relative_specifiers_resolve_from_the_repository_root() {
    let r = resolver(&["src/app/page.ts", "src/lib/math.ts"]);
    let resolved = r.resolve("src/app/page.ts", Language::TypeScript, "/src/lib/math");
    assert_eq!(one(&resolved), "src/lib/math.ts");
}

// --- Rust -------------------------------------------------------------------

#[test]
fn rust_crate_paths_resolve_against_the_source_root() {
    let r = resolver(&["src/lib.rs", "src/graph/mod.rs", "src/graph/types.rs"]);
    let resolved = r.resolve("src/lib.rs", Language::Rust, "crate::graph::types");
    assert_eq!(one(&resolved), "src/graph/types.rs");
}

#[test]
fn rust_a_use_naming_an_item_resolves_to_its_module() {
    let r = resolver(&["src/lib.rs", "src/graph/types.rs"]);
    // `Definition` is a type, not a module: the longest-prefix search has to
    // drop it and land on the file that declares it.
    let resolved = r.resolve(
        "src/lib.rs",
        Language::Rust,
        "crate::graph::types::Definition",
    );
    assert_eq!(one(&resolved), "src/graph/types.rs");
}

#[test]
fn rust_mod_rs_and_flat_files_both_resolve() {
    let r = resolver(&["src/lib.rs", "src/graph/mod.rs"]);
    let resolved = r.resolve("src/lib.rs", Language::Rust, "crate::graph");
    assert_eq!(one(&resolved), "src/graph/mod.rs");
}

#[test]
fn rust_super_walks_up_the_module_tree() {
    let r = resolver(&["src/lib.rs", "src/graph/types.rs", "src/graph/path.rs"]);
    let resolved = r.resolve("src/graph/types.rs", Language::Rust, "super::path::dir_of");
    assert_eq!(one(&resolved), "src/graph/path.rs");
}

#[test]
fn rust_bodyless_mod_resolves_against_the_current_module_directory() {
    let r = resolver(&["src/lib.rs", "src/graph/mod.rs", "src/graph/build.rs"]);
    let resolved = r.resolve("src/graph/mod.rs", Language::Rust, "mod build");
    assert_eq!(one(&resolved), "src/graph/build.rs");
}

#[test]
fn rust_the_crates_own_name_is_an_alias_for_crate() {
    let r = resolver_with(&[
        ("Cargo.toml", "[package]\nname = \"tiny-sweeper\"\n"),
        ("src/lib.rs", ""),
        ("src/graph/types.rs", ""),
    ]);
    let resolved = r.resolve("src/lib.rs", Language::Rust, "tiny_sweeper::graph::types");
    assert_eq!(one(&resolved), "src/graph/types.rs");
}

#[test]
fn rust_external_crates_are_external() {
    let r = resolver(&["src/lib.rs"]);
    assert!(
        r.resolve("src/lib.rs", Language::Rust, "std::collections::BTreeMap")
            .is_external()
    );
    assert!(
        r.resolve("src/lib.rs", Language::Rust, "serde::Serialize")
            .is_external()
    );
}

// --- Python -----------------------------------------------------------------

#[test]
fn python_relative_imports_walk_up_packages() {
    let r = resolver(&[
        "pkg/__init__.py",
        "pkg/service.py",
        "pkg/models.py",
        "shared.py",
    ]);
    assert_eq!(
        one(&r.resolve("pkg/service.py", Language::Python, ".models")),
        "pkg/models.py"
    );
    assert_eq!(
        one(&r.resolve("pkg/service.py", Language::Python, "..shared")),
        "shared.py"
    );
}

#[test]
fn python_dotted_modules_resolve_through_a_src_layout() {
    // No packaging metadata is read: the file set itself says where `pkg`
    // lives, which is what makes a `src/` layout work without a pyproject.
    let r = resolver(&["src/pkg/__init__.py", "src/pkg/util.py", "app/main.py"]);
    assert_eq!(
        one(&r.resolve("app/main.py", Language::Python, "pkg.util")),
        "src/pkg/util.py"
    );
}

#[test]
fn python_packages_resolve_to_their_init() {
    let r = resolver(&["pkg/__init__.py", "app/main.py"]);
    assert_eq!(
        one(&r.resolve("app/main.py", Language::Python, "pkg")),
        "pkg/__init__.py"
    );
}

#[test]
fn python_missing_child_of_a_known_package_is_not_resolved_to_the_package() {
    let r = resolver(&["pkg/__init__.py", "app/main.py"]);
    assert_eq!(
        r.resolve("app/main.py", Language::Python, "pkg.missing"),
        Resolution::Unresolved(UnresolvedReason::NoSuchFile)
    );
}

#[test]
fn ts_aliases_use_the_importers_nearest_configuration() {
    let r = resolver_with(&[
        (
            "packages/a/tsconfig.json",
            r#"{"compilerOptions":{"baseUrl":".","paths":{"@/*":["src/*"]}}}"#,
        ),
        (
            "packages/b/tsconfig.json",
            r#"{"compilerOptions":{"baseUrl":".","paths":{"@/*":["src/*"]}}}"#,
        ),
        ("packages/a/src/page.ts", ""),
        ("packages/a/src/value.ts", ""),
        ("packages/b/src/page.ts", ""),
        ("packages/b/src/value.ts", ""),
    ]);
    assert_eq!(
        one(&r.resolve("packages/a/src/page.ts", Language::TypeScript, "@/value")),
        "packages/a/src/value.ts"
    );
    assert_eq!(
        one(&r.resolve("packages/b/src/page.ts", Language::TypeScript, "@/value")),
        "packages/b/src/value.ts"
    );
}

#[test]
fn python_two_equally_shallow_candidates_are_ambiguous_not_guessed() {
    let r = resolver(&["a/util.py", "b/util.py", "app/main.py"]);
    // A wrong edge is worse than a missing one: it sends retrieval into the
    // wrong file with full confidence.
    assert_eq!(
        r.resolve("app/main.py", Language::Python, "util"),
        Resolution::Unresolved(UnresolvedReason::Ambiguous)
    );
}

#[test]
fn python_standard_library_modules_are_external() {
    let r = resolver(&["app/main.py"]);
    assert!(
        r.resolve("app/main.py", Language::Python, "os.path")
            .is_external()
    );
}

// --- Go ---------------------------------------------------------------------

#[test]
fn go_module_relative_imports_resolve_to_every_file_in_the_package() {
    let r = resolver_with(&[
        ("go.mod", "module example.com/app\n"),
        ("cmd/main.go", ""),
        ("internal/store/store.go", ""),
        ("internal/store/query.go", ""),
        ("internal/store/store_test.go", ""),
    ]);
    let resolved = r.resolve(
        "cmd/main.go",
        Language::Go,
        "example.com/app/internal/store",
    );
    // A Go import names a package, and a package is a directory. Test files
    // are excluded because nothing importable lives in them.
    assert_eq!(
        resolved.targets(),
        ["internal/store/query.go", "internal/store/store.go"]
    );
}

#[test]
fn go_third_party_imports_are_external() {
    let r = resolver_with(&[("go.mod", "module example.com/app\n"), ("cmd/main.go", "")]);
    assert!(r.resolve("cmd/main.go", Language::Go, "fmt").is_external());
    assert!(
        r.resolve("cmd/main.go", Language::Go, "github.com/pkg/errors")
            .is_external()
    );
}

#[test]
fn go_an_own_module_path_with_no_files_is_missing_not_external() {
    let r = resolver_with(&[("go.mod", "module example.com/app\n"), ("cmd/main.go", "")]);
    assert_eq!(
        r.resolve("cmd/main.go", Language::Go, "example.com/app/internal/gone"),
        Resolution::Unresolved(UnresolvedReason::NoSuchFile)
    );
}

#[test]
fn a_known_file_is_reported_as_present() {
    let r = resolver(&["src/a.ts"]);
    assert!(r.has("src/a.ts"));
    assert!(!r.has("src/b.ts"));
}

// --- Java --------------------------------------------------------------------

/// The Maven/Gradle source root is a build-tool convention, so the package is
/// matched by suffix rather than joined onto a root read out of a `pom.xml`.
#[test]
fn a_java_import_finds_its_type_under_any_source_root() {
    let r = resolver(&[
        "app/src/main/java/com/example/pricing/Money.java",
        "app/src/main/java/com/example/Cart.java",
    ]);
    assert_eq!(
        one(&r.resolve(
            "app/src/main/java/com/example/Cart.java",
            Language::Java,
            "com.example.pricing.Money",
        )),
        "app/src/main/java/com/example/pricing/Money.java"
    );
}

/// A static import names a member, so it has one segment too many. Dropping the
/// last segment also covers a nested type written `Outer.Inner`.
#[test]
fn a_static_java_import_resolves_to_the_type_holding_the_member() {
    let r = resolver(&["src/main/java/com/example/Rates.java"]);
    assert_eq!(
        one(&r.resolve(
            "src/main/java/App.java",
            Language::Java,
            "com.example.Rates.VAT"
        )),
        "src/main/java/com/example/Rates.java"
    );
}

#[test]
fn the_java_standard_library_is_external_rather_than_missing() {
    let r = resolver(&["src/main/java/App.java"]);
    assert!(
        r.resolve("src/main/java/App.java", Language::Java, "java.util.List")
            .is_external()
    );
}

/// Two source roots providing the same type is normal in a multi-module build
/// — `src/main` and `src/test` — and the shallower one is the main tree.
#[test]
fn the_shallowest_java_candidate_wins_so_the_graph_is_reproducible() {
    let r = resolver(&[
        "src/test/java/com/example/Cart.java",
        "src/main/java/com/example/Cart.java",
    ]);
    let first = r.resolve("src/main/java/App.java", Language::Java, "com.example.Cart");
    let second = r.resolve("src/main/java/App.java", Language::Java, "com.example.Cart");
    assert_eq!(first.targets(), second.targets());
    assert_eq!(one(&first).matches('/').count(), 5);
}

// --- Ruby --------------------------------------------------------------------

#[test]
fn require_relative_resolves_against_the_requiring_files_directory() {
    let r = resolver(&["lib/shop/cart.rb", "lib/shop/money.rb"]);
    assert_eq!(
        one(&r.resolve("lib/shop/cart.rb", Language::Ruby, "require_relative money")),
        "lib/shop/money.rb"
    );
}

#[test]
fn require_relative_walks_up_out_of_its_own_directory() {
    let r = resolver(&["lib/shop/cart.rb", "lib/pricing.rb"]);
    assert_eq!(
        one(&r.resolve(
            "lib/shop/cart.rb",
            Language::Ruby,
            "require_relative ../pricing"
        )),
        "lib/pricing.rb"
    );
}

/// There is no load path to read — `$LOAD_PATH` is assembled at runtime by the
/// gem tooling — so a plain `require` is matched by suffix, which covers the
/// `lib/` layout every gem uses without hard-coding it.
#[test]
fn a_plain_require_finds_a_file_on_the_conventional_load_path() {
    let r = resolver(&["lib/shop/pricing.rb", "app/main.rb"]);
    assert_eq!(
        one(&r.resolve("app/main.rb", Language::Ruby, "require shop/pricing")),
        "lib/shop/pricing.rb"
    );
}

/// A gem and a typo are indistinguishable without resolving a Gemfile, and
/// reporting every gem as missing would bury the genuine breakages.
#[test]
fn a_require_of_a_gem_is_external_rather_than_missing() {
    let r = resolver(&["app/main.rb"]);
    assert!(
        r.resolve("app/main.rb", Language::Ruby, "require json")
            .is_external()
    );
}

/// A relative require that names nothing is a real breakage, not a gem, and
/// must be reported as such.
#[test]
fn a_broken_require_relative_is_reported_as_missing() {
    let r = resolver(&["lib/shop/cart.rb"]);
    let resolution = r.resolve("lib/shop/cart.rb", Language::Ruby, "require_relative gone");
    assert!(!resolution.is_external());
    assert!(resolution.targets().is_empty());
}

#[test]
fn a_require_that_already_carries_its_extension_still_resolves() {
    let r = resolver(&["lib/shop/cart.rb", "lib/shop/money.rb"]);
    assert_eq!(
        one(&r.resolve(
            "lib/shop/cart.rb",
            Language::Ruby,
            "require_relative money.rb"
        )),
        "lib/shop/money.rb"
    );
}
