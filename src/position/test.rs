//! The positioning corpus.
//!
//! One fixture file and one fixture diff, then every way a model is known to
//! mangle a snippet it copied. Each case asserts an exact range or an explicit
//! [`Unanchored`] — never a panic, and never a plausible-looking wrong line,
//! which is the failure this module exists to prevent.

use super::*;
use crate::config::types::Config;
use crate::evidence::diff::parse_file_patch;
use crate::harness::mock::MockModel;
use serde_json::json;

/// The head revision of `src/main.rs`.
///
/// Lines 3–4 exist only here: no hunk covers them, so a snippet quoting them
/// can only resolve through stage 2.
const FILE: &str = "\
use std::fs;

fn helper() {
    let cfg = load();
}

fn main() {
    let items = read();
    let x = items[i];
    println!(\"{x}\");
}
";

const PATCH: &str = "\
@@ -7,3 +7,5 @@
 fn main() {
-    let items = old();
+    let items = read();
+    let x = items[i];
+    println!(\"{x}\");
 }
";

fn diff() -> FileDiff {
    parse_file_patch("src/main.rs", PATCH)
}

fn config() -> Config {
    crate::config::DEFAULTS
        .parse::<toml::Table>()
        .unwrap()
        .try_into()
        .unwrap()
}

/// Resolve against both the diff and the file, which is the production case.
fn resolve(snippet: &str) -> Resolution {
    locate(snippet, Some(&diff()), Some(FILE))
}

#[test]
fn the_fixture_diff_and_file_agree_on_line_numbers() {
    // If this drifts, every expectation below is measuring the wrong thing.
    let diff = diff();
    assert_eq!(diff.changed_lines, [8, 9, 10].into_iter().collect());
    assert_eq!(FILE.lines().nth(8), Some("    let x = items[i];"));
}

#[test]
fn an_exact_snippet_resolves_in_the_hunk() {
    let resolution = resolve("    let x = items[i];");
    assert_eq!(resolution.range(), Some((9, 9)));
    match resolution {
        Resolution::Anchored(anchor) => {
            assert_eq!(anchor.stage, Stage::Hunk);
            assert_eq!(anchor.side, Side::New);
            assert!(!anchor.relocated);
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn wrong_indentation_still_resolves() {
    assert_eq!(resolve("let x = items[i];").range(), Some((9, 9)));
    assert_eq!(resolve("\t\t\tlet x = items[i];   ").range(), Some((9, 9)));
}

#[test]
fn a_leaked_plus_marker_still_resolves() {
    assert_eq!(resolve("+    let x = items[i];").range(), Some((9, 9)));
}

#[test]
fn a_leaked_minus_marker_resolves_against_the_old_side() {
    // The quoted line was deleted, so it has no head-revision number of its
    // own. It borrows the first line that replaced it, which is where a human
    // would want the comment.
    let resolution = resolve("-    let items = old();");
    assert_eq!(resolution.range(), Some((8, 8)));
    match resolution {
        Resolution::Anchored(anchor) => assert_eq!(anchor.side, Side::Old),
        other => panic!("{other:?}"),
    }
}

#[test]
fn blank_line_drift_still_resolves() {
    let snippet = "    let x = items[i];\n\n\n    println!(\"{x}\");";
    assert_eq!(resolve(snippet).range(), Some((9, 10)));
}

#[test]
fn crlf_line_endings_still_resolve() {
    let snippet = "    let x = items[i];\r\n    println!(\"{x}\");\r\n";
    assert_eq!(resolve(snippet).range(), Some((9, 10)));
}

#[test]
fn a_snippet_that_only_exists_outside_the_diff_resolves_through_the_whole_file() {
    let resolution = resolve("fn helper() {\n    let cfg = load();");
    assert_eq!(resolution.range(), Some((3, 4)));
    match resolution {
        Resolution::Anchored(anchor) => assert_eq!(anchor.stage, Stage::WholeFile),
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_snippet_that_exists_nowhere_reports_unanchored_rather_than_a_line() {
    assert_eq!(
        resolve("    let y = totally_absent();"),
        Resolution::Unanchored(Unanchored::NoMatch)
    );
}

#[test]
fn an_empty_snippet_is_unanchored_and_says_why() {
    assert_eq!(
        resolve("   \n\n"),
        Resolution::Unanchored(Unanchored::NoSnippet)
    );
    assert_eq!(resolve(""), Resolution::Unanchored(Unanchored::NoSnippet));
}

#[test]
fn without_the_file_an_out_of_diff_snippet_is_unanchored_not_a_guess() {
    // A forge-only run has no checkout, so stage 2 cannot run. It must degrade
    // to unanchored rather than anchoring somewhere else in the hunk.
    let resolution = locate("fn helper() {", Some(&diff()), None);
    assert_eq!(resolution, Resolution::Unanchored(Unanchored::NoMatch));
}

#[test]
fn with_neither_a_diff_nor_a_file_nothing_resolves_and_nothing_panics() {
    assert_eq!(
        locate("anything", None, None),
        Resolution::Unanchored(Unanchored::NoMatch)
    );
}

#[test]
fn an_empty_diff_and_an_empty_file_are_handled() {
    let empty = parse_file_patch("src/main.rs", "");
    assert!(!locate("x", Some(&empty), Some("")).is_anchored());
}

#[test]
fn a_hunk_of_pure_deletions_anchors_at_the_hunk_start() {
    // Nothing survives to borrow a number from, and line 0 does not exist.
    let diff = parse_file_patch("f.rs", "@@ -4,2 +3,0 @@\n-gone();\n-also_gone();\n");
    assert_eq!(locate("gone();", Some(&diff), None).range(), Some((3, 3)));
}

#[test]
fn the_whole_corpus_resolves() {
    // The headline number: every way a model is known to mangle a snippet,
    // except the one that is genuinely unrecoverable offline.
    let cases: Vec<&str> = vec![
        "    let x = items[i];",
        "let x = items[i];",
        "+    let x = items[i];",
        "-    let items = old();",
        "    let x = items[i];\n\n    println!(\"{x}\");",
        "    let x = items[i];\r\n    println!(\"{x}\");",
        "fn helper() {\n    let cfg = load();",
        "  +  println!(\"{x}\");",
    ];

    let resolved = cases.iter().filter(|s| resolve(s).is_anchored()).count();
    assert_eq!(resolved, cases.len(), "unresolved case in the corpus");
}

#[tokio::test]
async fn a_resolvable_snippet_never_spends_a_model_call() {
    let model = MockModel::new();
    let config = config();
    let diff = diff();
    let mut spend = Spend::default();

    let resolution = Positioner::new(&model, &config)
        .resolve(
            PositionRequest {
                snippet: "let x = items[i];",
                diff: Some(&diff),
                file: Some(FILE),
                comment: "…",
                rendered_diff: PATCH,
            },
            &mut spend,
        )
        .await;

    assert_eq!(resolution.range(), Some((9, 9)));
    assert_eq!(model.calls(), 0, "spent money on a solved problem");
}

#[tokio::test]
async fn a_hopeless_snippet_is_recovered_by_the_relocation_call() {
    let model = MockModel::new().then(json!({
        "existing_code": "```rust\n    let x = items[i];\n```"
    }));
    let config = config();
    let diff = diff();
    let mut spend = Spend::default();

    let resolution = Positioner::new(&model, &config)
        .resolve(
            PositionRequest {
                snippet: "the index is never bounds-checked",
                diff: Some(&diff),
                file: Some(FILE),
                comment: "Guard the index before dereferencing",
                rendered_diff: PATCH,
            },
            &mut spend,
        )
        .await;

    assert_eq!(resolution.range(), Some((9, 9)));
    match resolution {
        Resolution::Anchored(anchor) => assert!(anchor.relocated),
        other => panic!("{other:?}"),
    }
    assert_eq!(model.calls(), 1);
}

#[tokio::test]
async fn a_failed_relocation_leaves_the_finding_unanchored_rather_than_wrong() {
    let model = MockModel::new().then(json!({"existing_code": "let z = elsewhere();"}));
    let config = config();
    let diff = diff();
    let mut spend = Spend::default();

    let resolution = Positioner::new(&model, &config)
        .resolve(
            PositionRequest {
                snippet: "nothing like the code",
                diff: Some(&diff),
                file: Some(FILE),
                comment: "…",
                rendered_diff: PATCH,
            },
            &mut spend,
        )
        .await;

    assert_eq!(resolution, Resolution::Unanchored(Unanchored::NoMatch));
}

#[tokio::test]
async fn a_relocation_call_that_errors_does_not_fail_the_resolution() {
    let model = MockModel::new().then_error("upstream exploded");
    let config = config();
    let diff = diff();
    let mut spend = Spend::default();

    let resolution = Positioner::new(&model, &config)
        .resolve(
            PositionRequest {
                snippet: "nothing like the code",
                diff: Some(&diff),
                file: Some(FILE),
                comment: "…",
                rendered_diff: PATCH,
            },
            &mut spend,
        )
        .await;

    assert!(!resolution.is_anchored());
    assert_eq!(spend, Spend::default(), "a failed call bills nothing");
}

#[tokio::test]
async fn relocation_usage_is_accounted_for() {
    let model = MockModel::new()
        .then(json!({"existing_code": "    let x = items[i];"}))
        .with_usage(Usage {
            input_tokens: 900,
            output_tokens: 20,
            cached_tokens: 0,
            cost_usd: 0.001,
        });
    let config = config();
    let diff = diff();
    let mut spend = Spend::default();

    Positioner::new(&model, &config)
        .resolve(
            PositionRequest {
                snippet: "nothing like the code",
                diff: Some(&diff),
                file: Some(FILE),
                comment: "…",
                rendered_diff: PATCH,
            },
            &mut spend,
        )
        .await;

    assert_eq!(usage.input_tokens, 900);
    assert!(usage.cost_usd > 0.0, "the call has to reach the budget");
}

#[test]
fn a_pure_deletion_hunk_does_not_provide_a_postable_new_side_anchor() {
    // When a hunk contains only deletions (new_lines == 0), it covers no
    // head-revision lines at all. A comment anchored to such a hunk cannot be
    // posted on the new side, because GitHub rejects comments on nonexistent
    // lines. The finding must either anchor to old-side deleted code or remain
    // unanchored.
    let pure_deletion_patch = "\
@@ -1,3 +0,0 @@
-fn old_helper() {
-    do_something();
-}
";
    let diff = parse_file_patch("src/main.rs", pure_deletion_patch);
    assert_eq!(diff.hunks.len(), 1);
    let hunk = &diff.hunks[0];
    assert_eq!(hunk.new_lines, 0, "hunk must be pure deletion");

    // within_hunk should reject any anchor in this hunk, because no new-side
    // lines exist to post on.
    assert!(!diff.within_hunk(hunk.new_start, hunk.new_start));
}
