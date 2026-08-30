//! Tests for duplicate pull request detection.

use super::*;
use crate::forge::types::{ChangedFile, FileStatus};

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
        "main",
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
    let one = Shape::of(1, "main", &[changed("a.rs", "@@\n+let a = 1;\n")]);
    let two = Shape::of(2, "main", &[changed("a.rs", "@@\n+let b = 2;\n")]);
    assert_eq!(duplicate_of(&two, &[one], 0.8, 0.9), None);
}

#[test]
fn the_same_lines_in_different_files_are_not_duplicates() {
    let one = Shape::of(1, "main", &[changed("a.rs", "@@\n+let a = 1;\n")]);
    let two = Shape::of(2, "main", &[changed("z.rs", "@@\n+let a = 1;\n")]);
    let score = overlap(&one, &two);
    // Neither axis matches: the paths differ, and the added lines are qualified
    // by the file they were added to, so they differ too.
    assert_eq!(score.edits, 0.0);
    assert_eq!(score.paths, 0.0);
    assert_eq!(duplicate_of(&two, &[one], 0.8, 0.9), None);
}

#[test]
fn a_pull_request_with_nothing_readable_duplicates_nothing() {
    let binary = Shape::of(
        2,
        "main",
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
        "main",
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
    let small = Shape::of(1, "main", &[changed("a.rs", "@@\n+let a = 1;\n")]);
    let big = Shape::of(
        2,
        "main",
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
    let one = Shape::of(1, "main", &files("a"));
    let two = Shape::of(2, "main", &files("b"));

    assert_eq!(overlap(&one, &two).paths, 1.0);
    assert_eq!(overlap(&one, &two).edits, 0.0);
    assert_eq!(duplicate_of(&two, &[one], 0.8, 0.9), None);
}

#[test]
fn two_edits_that_add_the_same_line_but_remove_different_ones_are_not_duplicates() {
    // Both change `a.rs` to `return false;`, in different functions. The
    // additions match perfectly; what separates them is what they took out.
    let one = Shape::of(
        1,
        "main",
        &[changed("a.rs", "@@\n-return alpha();\n+return false;\n")],
    );
    let two = Shape::of(
        2,
        "main",
        &[changed("a.rs", "@@\n-return beta();\n+return false;\n")],
    );

    // The added lines are identical; the removed ones are not, and a hunk
    // fingerprint carries both.
    assert_eq!(overlap(&one, &two).edits, 0.0);
    assert_eq!(duplicate_of(&two, &[one], 0.8, 0.9), None);
}

#[test]
fn the_same_edit_in_two_places_in_one_file_is_not_a_duplicate() {
    // Flipping `enabled = false` to `true` in two unrelated blocks produces
    // identical added and removed lines. What tells them apart is the context
    // each hunk carries, which is why the fingerprint is per hunk.
    let one = Shape::of(
        1,
        "main",
        &[changed(
            "config.toml",
            "@@ -1,3 +1,3 @@\n [alpha]\n-enabled = false\n+enabled = true\n",
        )],
    );
    let two = Shape::of(
        2,
        "main",
        &[changed(
            "config.toml",
            "@@ -9,3 +9,3 @@\n [beta]\n-enabled = false\n+enabled = true\n",
        )],
    );

    assert_eq!(overlap(&one, &two).paths, 1.0);
    assert_eq!(overlap(&one, &two).edits, 0.0);
    assert_eq!(duplicate_of(&two, &[one], 0.8, 0.9), None);
}

#[test]
fn an_unreadable_file_makes_the_whole_shape_incomparable() {
    // The textual halves match perfectly and the binaries are invisible, so a
    // shape that ignored them would close the newer one over bytes nobody saw.
    let with_binary = |number: u64| {
        Shape::of(
            number,
            "main",
            &[
                changed("README.md", "@@ -1,2 +1,2 @@\n # Title\n-old\n+new\n"),
                ChangedFile {
                    path: "logo.png".into(),
                    patch: None,
                    ..ChangedFile::default()
                },
            ],
        )
    };
    let one = with_binary(1);
    let two = with_binary(2);

    assert!(!one.every_file_readable);
    assert!(!two.is_comparable());
    assert_eq!(duplicate_of(&two, &[one], 0.8, 0.9), None);
}

#[test]
fn making_the_same_edit_twice_is_not_the_same_as_making_it_once() {
    // An older pull request changes one occurrence of a repeated block; the
    // newer one changes two. A set would collapse them to one fingerprint and
    // score 1.0, closing the newer one and losing its second edit.
    let once = Shape::of(
        1,
        "main",
        &[changed(
            "config.toml",
            "@@ -1,3 +1,3 @@\n [x]\n-enabled = false\n+enabled = true\n",
        )],
    );
    let twice = Shape::of(
        2,
        "main",
        &[changed(
            "config.toml",
            "@@ -1,3 +1,3 @@\n [x]\n-enabled = false\n+enabled = true\n\
             @@ -9,3 +9,3 @@\n [x]\n-enabled = false\n+enabled = true\n",
        )],
    );

    assert_eq!(twice.edits.values().sum::<usize>(), 2);
    assert_eq!(overlap(&once, &twice).edits, 0.5);
    assert_eq!(duplicate_of(&twice, &[once], 0.8, 0.9), None);
}

#[test]
fn a_backport_is_not_a_duplicate_of_the_change_it_backports() {
    // Identical diffs, different targets. The backport still has to land on the
    // release branch, and closing it loses it.
    let to_main = readme_fix(1);
    let to_release = Shape {
        base_ref: "release/2.1".into(),
        ..readme_fix(2)
    };

    assert_eq!(overlap(&to_main, &to_release).edits, 1.0);
    assert_eq!(duplicate_of(&to_release, &[to_main], 0.8, 0.9), None);
}

#[test]
fn the_same_patch_text_with_a_different_file_operation_is_not_a_duplicate() {
    // Emptying a file and deleting it can produce the same patch body and are
    // materially different results.
    let patch = "@@ -1,3 +1,1 @@\n keep\n-gone\n-also gone\n";
    let emptied = Shape::of(1, "main", &[changed("a.rs", patch)]);
    let deleted = Shape::of(
        2,
        "main",
        &[ChangedFile {
            path: "a.rs".into(),
            status: FileStatus::Removed,
            patch: Some(patch.into()),
            ..ChangedFile::default()
        }],
    );

    assert_eq!(overlap(&emptied, &deleted).paths, 1.0);
    assert_eq!(overlap(&emptied, &deleted).edits, 0.0);
    assert_eq!(duplicate_of(&deleted, &[emptied], 0.8, 0.9), None);
}

#[test]
fn two_edits_to_identical_distant_blocks_are_told_apart_by_position() {
    // Same context, same change, different place in the file. Nothing but the
    // hunk coordinates separates them.
    let first = Shape::of(
        1,
        "main",
        &[changed(
            "config.rs",
            "@@ -1,3 +1,3 @@\n let block = [\n-    \"old\",\n+    \"new\",\n",
        )],
    );
    let second = Shape::of(
        2,
        "main",
        &[changed(
            "config.rs",
            "@@ -40,3 +40,3 @@\n let block = [\n-    \"old\",\n+    \"new\",\n",
        )],
    );

    assert_eq!(overlap(&first, &second).edits, 0.0);
    assert_eq!(duplicate_of(&second, &[first], 0.8, 0.9), None);
}

#[test]
fn many_tiny_shared_hunks_do_not_outvote_one_large_distinct_one() {
    // Twenty shared one-line edits and one big distinct edit each. Counting
    // hunks scores 20/22 = 0.91 and closes the newer pull request; counting
    // lines sees that almost none of the substance is shared.
    let shared: String = (0..20)
        .map(|n| format!("@@ -{n},3 +{n},3 @@\n ctx{n}\n-old{n}\n+new{n}\n"))
        .collect();
    let big = |tag: &str| -> String {
        let body: String = (0..500).map(|n| format!("+{tag} line {n}\n")).collect();
        format!("@@ -900,1 +900,501 @@\n anchor\n{body} tail\n")
    };

    let one = Shape::of(1, "main", &[changed("a.rs", &format!("{shared}{}", big("alpha")))]);
    let two = Shape::of(2, "main", &[changed("a.rs", &format!("{shared}{}", big("beta")))]);

    let score = overlap(&one, &two);
    assert!(score.edits < 0.9, "scored {}", score.edits);
    assert_eq!(duplicate_of(&two, &[one], 0.8, 0.9), None);
}
