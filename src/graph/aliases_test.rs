//! Alias-discovery tests. These cover the configuration formats the resolver
//! reads: `tsconfig.json` (including the JSONC dialect real ones are written
//! in), `go.mod`, and `Cargo.toml`.

use super::*;

fn files(entries: &[(&str, &str)]) -> Vec<SourceFile> {
    entries
        .iter()
        .map(|(path, text)| SourceFile::new(*path, *text))
        .collect()
}

#[test]
fn tsconfig_paths_are_joined_onto_base_url() {
    let config = AliasConfig::discover(&files(&[(
        "tsconfig.json",
        r#"{
  "compilerOptions": {
    "baseUrl": ".",
    "paths": { "@/*": ["src/*"], "~ui/*": ["src/components/*"] }
  }
}"#,
    )]));

    assert_eq!(
        alias_map(&config),
        BTreeMap::from([
            ("@/*".to_string(), vec!["src/*".to_string()]),
            ("~ui/*".to_string(), vec!["src/components/*".to_string()]),
        ])
    );
    assert_eq!(config.ts_base_urls, vec![String::new()]);
    assert_eq!(
        config.expand_ts("@/lib/math"),
        vec!["src/lib/math".to_string()]
    );
}

#[test]
fn tsconfig_without_base_url_resolves_targets_against_its_own_directory() {
    let config = AliasConfig::discover(&files(&[(
        "apps/web/tsconfig.json",
        r#"{ "compilerOptions": { "paths": { "@/*": ["./src/*"] } } }"#,
    )]));
    assert_eq!(
        config.expand_ts("@/lib/math"),
        vec!["apps/web/src/lib/math".to_string()]
    );
}

#[test]
fn a_jsonc_tsconfig_still_yields_its_aliases() {
    // Real tsconfigs carry comments and trailing commas; `serde_json` rejects
    // both, and bailing would silently drop every alias in the repository.
    let config = AliasConfig::discover(&files(&[(
        "tsconfig.json",
        r#"{
  // the compiler options
  "compilerOptions": {
    /* base of the world */
    "baseUrl": "./src",
    "paths": {
      "@/*": ["*"],
    },
  },
}"#,
    )]));
    assert_eq!(
        config.expand_ts("@/lib/math"),
        vec!["src/lib/math".to_string()]
    );
    assert_eq!(config.ts_base_urls, vec!["src".to_string()]);
}

#[test]
fn a_url_inside_a_string_is_not_treated_as_a_comment() {
    let config = AliasConfig::discover(&files(&[(
        "tsconfig.json",
        r#"{"compilerOptions":{"baseUrl":".","paths":{"@//x":["https://e.com"],"@/*":["src/*"]}}}"#,
    )]));
    assert_eq!(
        config.expand_ts("@/lib/math"),
        vec!["src/lib/math".to_string()]
    );
}

#[test]
fn an_exact_pattern_matches_only_itself() {
    let pattern = AliasPattern {
        pattern: "@app".to_string(),
        targets: vec!["src/app/index.ts".to_string()],
    };
    assert_eq!(
        pattern.expand("@app"),
        Some(vec!["src/app/index.ts".to_string()])
    );
    assert_eq!(pattern.expand("@app/thing"), None);
}

#[test]
fn the_most_specific_pattern_wins() {
    let config = AliasConfig::discover(&files(&[(
        "tsconfig.json",
        r#"{"compilerOptions":{"baseUrl":".","paths":{"@/*":["src/*"],"@/lib/*":["packages/lib/*"]}}}"#,
    )]));
    // Both patterns match `@/lib/math`. `tsc` prefers the longer literal
    // prefix; anything else silently sends the alias to the wrong package.
    assert_eq!(
        config.expand_ts("@/lib/math"),
        vec!["packages/lib/math".to_string()]
    );
}

#[test]
fn go_module_path_and_root_come_from_go_mod() {
    let config =
        AliasConfig::discover(&files(&[("go.mod", "module example.com/app\n\ngo 1.22\n")]));
    assert_eq!(config.go_module.as_deref(), Some("example.com/app"));
    assert_eq!(config.go_root, "");
}

#[test]
fn a_nested_go_mod_records_its_directory() {
    let config = AliasConfig::discover(&files(&[("svc/go.mod", "module example.com/svc\n")]));
    assert_eq!(config.go_module.as_deref(), Some("example.com/svc"));
    assert_eq!(config.go_root, "svc");
}

#[test]
fn cargo_crate_name_is_normalised_like_rustc_does() {
    let config = AliasConfig::discover(&files(&[(
        "Cargo.toml",
        "[package]\nname = \"my-crate\"\nversion = \"0.1.0\"\n",
    )]));
    // `use my_crate::x` has to match a package called `my-crate`.
    assert_eq!(config.rust_crate.as_deref(), Some("my_crate"));
    assert_eq!(config.rust_src_root, "src");
}

#[test]
fn a_nested_cargo_toml_roots_the_crate_at_its_own_src() {
    let config = AliasConfig::discover(&files(&[(
        "crates/core/Cargo.toml",
        "[package]\nname = \"core\"\n",
    )]));
    assert_eq!(config.rust_src_root, "crates/core/src");
}

#[test]
fn malformed_config_files_are_ignored_rather_than_fatal() {
    let config = AliasConfig::discover(&files(&[
        ("tsconfig.json", "{ this is not json"),
        ("Cargo.toml", "[package"),
        ("go.mod", "// nothing useful"),
    ]));
    assert_eq!(config, AliasConfig::discover(&[]));
}

#[test]
fn a_repository_with_no_config_has_sensible_defaults() {
    let config = AliasConfig::discover(&[]);
    assert!(config.ts_paths.is_empty());
    assert_eq!(config.rust_src_root, "src");
    assert!(config.go_module.is_none());
}
