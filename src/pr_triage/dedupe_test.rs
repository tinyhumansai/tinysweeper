//! Tests for duplicate pull request detection.

use super::*;
use crate::forge::types::ChangedFile;

fn changed(path: &str, patch: &str) -> ChangedFile {
    ChangedFile {
        path: path.into(),
        patch: Some(patch.into()),
        ..ChangedFile::default()
    }
}

/// The shape of the real pair this feature was built for: two contributors
/// fixing the same six READMEs on the same day, in the same way.
fn readme_fix(number: u64) -> Shape {
    Shape::of(
        number,
        &[
            changed("README.md", "@@\n-Rust 1.93.0\n+Rust 1.96.1\n"),
            changed("crates/a/README.md", "@@\n-Rust 1.93.0\n+Rust 1.96.1\n"),
        ],
    )
}

#[test]
fn the_newer_of_two_identical_pull_requests_is_the_duplicate() {
    let older = readme_fix(5789);
    let newer = readme_fix(5798);

    assert_eq!(
        duplicate_of(&newer, std::slice::from_ref(&older), 0.8, 0.9).map(|(number, _)| number),
        Some(5789)
    );
    // And never the other way round, however the caller orders the list.
    assert_eq!(duplicate_of(&older, &[newer], 0.8, 0.9), None);
}

#[test]
fn the_same_files_changed_differently_are_not_duplicates() {
    let one = Shape::of(1, &[changed("a.rs", "@@\n+let a = 1;\n")]);
    let two = Shape::of(2, &[changed("a.rs", "@@\n+let b = 2;\n")]);
    assert_eq!(duplicate_of(&two, &[one], 0.8, 0.9), None);
}

#[test]
fn the_same_lines_in_different_files_are_not_duplicates() {
    let one = Shape::of(1, &[changed("a.rs", "@@\n+let a = 1;\n")]);
    let two = Shape::of(2, &[changed("z.rs", "@@\n+let a = 1;\n")]);
    let score = overlap(&one, &two);
    // Neither axis matches: the paths differ, and the added lines are qualified
    // by the file they were added to, so they differ too.
    assert_eq!(score.lines, 0.0);
    assert_eq!(score.paths, 0.0);
    assert_eq!(duplicate_of(&two, &[one], 0.8, 0.9), None);
}

#[test]
fn a_pull_request_with_nothing_readable_duplicates_nothing() {
    let binary = Shape::of(
        2,
        &[ChangedFile {
            path: "logo.png".into(),
            patch: None,
            ..ChangedFile::default()
        }],
    );
    assert!(!binary.is_comparable());
    // Two of them would otherwise score a confident 1.0 against each other.
    let other = Shape::of(
        1,
        &[ChangedFile {
            path: "logo.png".into(),
            patch: None,
            ..ChangedFile::default()
        }],
    );
    assert_eq!(duplicate_of(&binary, &[other], 0.8, 0.9), None);
}

#[test]
fn the_best_match_wins_and_ties_go_to_the_oldest() {
    let a = readme_fix(10);
    let b = readme_fix(20);
    let subject = readme_fix(30);
    assert_eq!(
        duplicate_of(&subject, &[b, a], 0.8, 0.9).map(|(number, _)| number),
        Some(10)
    );
}

#[test]
fn a_superset_pull_request_falls_below_the_path_floor() {
    // A pull request that makes the same fix *and* six other changes is not a
    // duplicate — closing it would lose the other six.
    let small = Shape::of(1, &[changed("a.rs", "@@\n+let a = 1;\n")]);
    let big = Shape::of(
        2,
        &[
            changed("a.rs", "@@\n+let a = 1;\n"),
            changed("b.rs", "@@\n+let b = 2;\n"),
            changed("c.rs", "@@\n+let c = 3;\n"),
        ],
    );
    assert!(overlap(&big, &small).paths < 0.8);
    assert_eq!(duplicate_of(&big, &[small], 0.8, 0.9), None);
}

#[test]
fn the_same_line_added_to_different_files_is_not_a_duplicate() {
    // The additions are identical and the path sets are identical, so a
    // repository-wide line set would score a confident 100% on both axes.
    let files = |which: &str| {
        vec![
            changed(
                "a.rs",
                if which == "a" {
                    "@@\n+return false;\n"
                } else {
                    "@@\n+let x = 1;\n"
                },
            ),
            changed(
                "b.rs",
                if which == "a" {
                    "@@\n+let x = 1;\n"
                } else {
                    "@@\n+return false;\n"
                },
            ),
        ]
    };
    let one = Shape::of(1, &files("a"));
    let two = Shape::of(2, &files("b"));

    assert_eq!(overlap(&one, &two).paths, 1.0);
    assert_eq!(overlap(&one, &two).added, 0.0);
    assert_eq!(duplicate_of(&two, &[one], 0.8, 0.9), None);
}

#[test]
fn two_edits_that_add_the_same_line_but_remove_different_ones_are_not_duplicates() {
    // Both change `a.rs` to `return false;`, in different functions. The
    // additions match perfectly; what separates them is what they took out.
    let one = Shape::of(
        1,
        &[changed("a.rs", "@@\n-return alpha();\n+return false;\n")],
    );
    let two = Shape::of(
        2,
        &[changed("a.rs", "@@\n-return beta();\n+return false;\n")],
    );

    let score = overlap(&one, &two);
    assert_eq!(score.added, 1.0);
    assert_eq!(score.removed, 0.0);
    assert_eq!(score.lines, 0.0, "the lower of the two, not the mean");
    assert_eq!(duplicate_of(&two, &[one], 0.8, 0.9), None);
}
