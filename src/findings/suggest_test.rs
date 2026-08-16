//! Suggestion tests.
//!
//! Most of these assert a **refusal**. That is the point of the module: an
//! inert fence that is slightly wrong is a typo, and an applicable one that is
//! slightly wrong is a button that breaks the build.

use super::*;

use crate::config::types::{LaneId, Severity};
use crate::evidence::diff::parse_file_patch;

/// A four-line indented block, all added, at head lines 3..6.
const PATCH: &str = "@@ -1,2 +1,6 @@\n fn main() {\n     let n = 1;\n+    if n > 0 {\n+        println!(\"{n}\");\n+    }\n+    done();\n";

fn diffs() -> Vec<FileDiff> {
    vec![parse_file_patch("src/main.rs", PATCH)]
}

fn finding(line: Option<u64>, end_line: Option<u64>, suggestion: Option<&str>) -> Finding {
    Finding {
        lane: LaneId::Critique,
        severity: Severity::Medium,
        confidence: 0.9,
        path: "src/main.rs".into(),
        line,
        end_line,
        rule: "example".into(),
        title: "Example".into(),
        body: "Because.".into(),
        suggestion: suggestion.map(str::to_string),
        applicable: None,
        late: false,
        identity: None,
        corroboration: 1,
    }
}

#[test]
fn a_finding_with_no_suggestion_produces_no_block() {
    assert!(applicable(&finding(Some(3), None, None), &diffs()).is_none());
}

#[test]
fn a_blank_suggestion_produces_no_block() {
    assert!(applicable(&finding(Some(3), None, Some("   \n")), &diffs()).is_none());
}

#[test]
fn an_unanchored_finding_produces_no_block() {
    // Nothing to replace, and nothing GitHub could attach a button to.
    assert!(applicable(&finding(None, None, Some("done();")), &diffs()).is_none());
}

/// The replacement would nest a fence inside the suggestion fence and render
/// as literal backticks.
#[test]
fn a_suggestion_containing_a_fence_produces_no_block() {
    let f = finding(Some(3), None, Some("```rust\nlet n = 2;\n```"));
    assert!(applicable(&f, &diffs()).is_none());
}

/// GitHub rejects a review comment anchored outside the diff — and rejects the
/// whole review with it, taking every other comment down.
#[test]
fn a_range_reaching_past_the_diff_produces_no_block() {
    let f = finding(Some(3), Some(99), Some("let n = 2;"));
    assert!(applicable(&f, &diffs()).is_none());
}

#[test]
fn a_finding_on_a_file_with_no_diff_produces_no_block() {
    let mut f = finding(Some(3), None, Some("let n = 2;"));
    f.path = "src/other.rs".into();
    assert!(applicable(&f, &diffs()).is_none());
}

/// Observed on tinyflows#52: the model quoted one line of a let-chain — the
/// smallest span that showed the MSRV problem, exactly as asked — and wrote a
/// replacement for the whole block it opened. Anchored to that one line, the
/// button would have inserted the rewritten block above the `errors.push(…)`
/// body it was meant to replace, leaving the file with both copies.
#[test]
fn a_multi_line_replacement_on_a_single_line_anchor_produces_no_block() {
    let f = finding(
        Some(3),
        None,
        Some("if n > 0 {\n    eprintln!(\"{n}\");\n}"),
    );
    assert!(applicable(&f, &diffs()).is_none());
}

/// The refusal is arithmetic over the whole anchor, not a special case for
/// single lines: a two-line anchor carrying a three-line replacement strands
/// the third line below the inserted block exactly the same way.
#[test]
fn a_replacement_longer_than_a_multi_line_anchor_produces_no_block() {
    let f = finding(
        Some(3),
        Some(4),
        Some("if n > 0 {\n    eprintln!(\"{n}\");\n}"),
    );
    assert!(applicable(&f, &diffs()).is_none());
}

/// The same replacement is fine once the anchor covers what it rewrites — the
/// refusal is about the two spans disagreeing, not about size.
#[test]
fn the_same_replacement_is_allowed_once_the_anchor_covers_it() {
    let f = finding(
        Some(3),
        Some(5),
        Some("if n > 0 {\n    eprintln!(\"{n}\");\n}"),
    );
    assert!(applicable(&f, &diffs()).is_some());
}

/// A replacement narrower than its anchor is a deliberate collapse — the span
/// the model quoted is the span that goes away, so there is no ambiguity.
#[test]
fn a_replacement_narrower_than_its_anchor_is_kept() {
    let f = finding(Some(3), Some(5), Some("done();"));
    let out = applicable(&f, &diffs()).expect("a block");
    assert_eq!((out.start_line, out.end_line), (3, 5));
}

#[test]
fn a_single_line_replacement_spans_exactly_that_line() {
    let f = finding(Some(6), None, Some("finish();"));
    let out = applicable(&f, &diffs()).expect("a block");
    assert_eq!((out.start_line, out.end_line), (6, 6));
}

/// The headline behaviour. `position` normalises leading whitespace away on
/// both sides, so a correctly-anchored replacement routinely arrives
/// flush-left. GitHub substitutes it verbatim.
#[test]
fn indentation_the_model_dropped_is_restored() {
    let f = finding(Some(6), None, Some("finish();"));
    let out = applicable(&f, &diffs()).expect("a block");
    assert_eq!(out.replacement, "    finish();");
}

/// Only the shortfall is added, so structure inside the replacement survives.
#[test]
fn relative_indentation_inside_the_replacement_is_preserved() {
    let f = finding(
        Some(3),
        Some(5),
        Some("if n > 0 {\n    eprintln!(\"{n}\");\n}"),
    );
    let out = applicable(&f, &diffs()).expect("a block");
    assert_eq!((out.start_line, out.end_line), (3, 5));
    assert_eq!(
        out.replacement,
        "    if n > 0 {\n        eprintln!(\"{n}\");\n    }"
    );
}

#[test]
fn a_replacement_that_is_already_indented_is_left_alone() {
    let f = finding(Some(6), None, Some("    finish();"));
    let out = applicable(&f, &diffs()).expect("a block");
    assert_eq!(out.replacement, "    finish();");
}

/// Padding a blank line out to the block's indent would commit trailing
/// whitespace.
#[test]
fn a_blank_line_inside_the_replacement_stays_blank() {
    let f = finding(Some(3), Some(5), Some("if n > 0 {\n\n}"));
    let out = applicable(&f, &diffs()).expect("a block");
    assert_eq!(out.replacement, "    if n > 0 {\n\n    }");
}

/// Nothing can be inferred when the replacement's own lines disagree about how
/// much indent they are missing, so it is left exactly as written.
#[test]
fn a_non_uniform_shortfall_is_not_guessed_at() {
    let f = finding(Some(3), Some(5), Some("if n > 0 {\n  x();\n}"));
    let out = applicable(&f, &diffs()).expect("a block");
    // Common indent of the replacement is "", of the target "    " — so the
    // uniform prefix is added and the inner relative indent is untouched.
    assert_eq!(out.replacement, "    if n > 0 {\n      x();\n    }");
}

#[test]
fn a_block_at_column_zero_is_not_reindented() {
    let patch = "@@ -1 +1,2 @@\n use std::io;\n+use std::fmt;\n";
    let diffs = vec![parse_file_patch("src/main.rs", patch)];
    let f = finding(Some(2), None, Some("use std::fmt::Debug;"));
    let out = applicable(&f, &diffs).expect("a block");
    assert_eq!(out.replacement, "use std::fmt::Debug;");
}

#[test]
fn a_removed_line_cannot_be_replaced() {
    // Removed lines do not exist in the head revision, so GitHub has nothing
    // to anchor a commit button to.
    let patch = "@@ -1,2 +1,1 @@\n fn main() {\n-    gone();\n";
    let diffs = vec![parse_file_patch("src/main.rs", patch)];
    let f = finding(Some(2), None, Some("kept();"));
    assert!(applicable(&f, &diffs).is_none());
}
