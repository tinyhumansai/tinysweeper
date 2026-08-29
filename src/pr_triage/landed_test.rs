//! Tests for the "already on the base branch" detector.
//!
//! These are the golden tests of the close path: every one is a shape of pull
//! request that a sweep will actually meet on a busy repository, and the
//! refusals matter more than the acceptances.

use super::*;
use crate::forge::types::{ChangedFile, FileStatus};

fn changed(path: &str, patch: &str) -> ChangedFile {
    ChangedFile {
        path: path.into(),
        patch: Some(patch.into()),
        ..ChangedFile::default()
    }
}

fn present(text: &str) -> Base {
    Base::Present(text.into())
}

#[test]
fn a_hunk_becomes_the_two_versions_of_its_stretch() {
    let got = hunks("@@ -1,3 +1,4 @@\n before\n+added\n after\n");
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].before, vec!["before".to_string(), "after".into()]);
    assert_eq!(
        got[0].after,
        vec!["before".to_string(), "added".into(), "after".into()]
    );
    assert_eq!(got[0].changed, 1);
}

#[test]
fn each_hunk_is_judged_on_its_own_stretch() {
    let got = hunks("@@ -1 +1 @@\n-one\n+two\n@@ -9 +9 @@\n-three\n+four\n");
    assert_eq!(got.len(), 2);
    assert_eq!(got[1].before, vec!["three".to_string()]);
}

#[test]
fn diff_headers_are_not_mistaken_for_content() {
    let got = hunks("--- a/x.rs\n+++ b/x.rs\n@@ -1 +1 @@\n-old\n+new\n");
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].before, vec!["old".to_string()]);
    assert_eq!(got[0].after, vec!["new".to_string()]);
}

#[test]
fn indentation_is_kept_because_in_some_languages_it_is_the_change() {
    // Only trailing whitespace goes. Moving a call out of a Python `if` alters
    // nothing but the indentation, and a comparison that collapsed it would
    // find the line still inside the block and call the change applied.
    assert_eq!(normalise("    let  x =   1;   "), "    let  x =   1;");
}

#[test]
fn a_python_dedent_is_not_landed_against_the_indented_original() {
    let file = changed(
        "run.py",
        "@@ -1,3 +1,3 @@\n if ready:\n-    new()\n+new()\n",
    );
    assert_eq!(
        file_landed(&file, Some("if ready:\n    new()\n")),
        Err(NotLanded::RemovedLineStillPresent)
    );
}

#[test]
fn a_change_already_on_the_base_branch_is_landed() {
    let file = changed(
        "README.md",
        "@@ -1,3 +1,3 @@\n # Title\n-Rust 1.93.0\n+Rust 1.96.1\n more\n",
    );
    assert_eq!(
        file_landed(&file, Some("# Title\nRust 1.96.1\nmore\n")),
        Ok(2)
    );
}

#[test]
fn the_before_image_is_what_makes_a_one_line_change_safe() {
    // Same patch, base branch not yet updated. The stretch still reads exactly
    // as it did, which is conclusive however the additions look.
    let file = changed(
        "README.md",
        "@@ -1,3 +1,3 @@\n # Title\n-Rust 1.93.0\n+Rust 1.96.1\n more\n",
    );
    assert_eq!(
        file_landed(
            &file,
            Some("# Title\nRust 1.93.0\nmore\nRust 1.96.1 is coming\n")
        ),
        Err(NotLanded::RemovedLineStillPresent)
    );
}

#[test]
fn an_addition_that_exists_somewhere_else_is_not_landed_here() {
    // The reason a hunk carries its context. Adding three lines to a second
    // list, where the same three lines already sit in a first list, changes the
    // file — and a match on the lines alone would have called it a no-op.
    let file = changed(
        "config.rs",
        "@@ -10,2 +10,5 @@\n let second = [\n+    \"alpha\",\n+    \"beta\",\n+    \"gamma\",\n ];\n",
    );
    let base =
        "let first = [\n    \"alpha\",\n    \"beta\",\n    \"gamma\",\n];\nlet second = [\n];\n";
    assert_eq!(
        file_landed(&file, Some(base)),
        // The untouched `second` block is still on the branch, which is the
        // conclusive form of "not applied here".
        Err(NotLanded::RemovedLineStillPresent)
    );

    // And once it really is applied in that place, it is landed.
    let applied = "let first = [\n    \"alpha\",\n];\nlet second = [\n    \"alpha\",\n    \"beta\",\n    \"gamma\",\n];\n";
    assert_eq!(file_landed(&file, Some(applied)), Ok(3));
}

#[test]
fn deleting_a_file_somebody_already_deleted_is_landed() {
    let file = ChangedFile {
        path: "dead.rs".into(),
        status: FileStatus::Removed,
        patch: Some("@@ -1,2 +0,0 @@\n-fn dead() {}\n-// gone\n".into()),
        ..ChangedFile::default()
    };
    assert_eq!(file_landed(&file, None), Ok(2));
}

#[test]
fn a_deletion_is_never_judged_on_a_base_read_that_failed() {
    // The failure this variant exists for: an unreadable file that read as an
    // absent one would tell a deletion-only pull request its work is done.
    let file = ChangedFile {
        path: "alive.rs".into(),
        status: FileStatus::Removed,
        patch: Some("@@ -1,3 +0,0 @@\n-fn alive() {}\n-// still here\n-// really\n".into()),
        ..ChangedFile::default()
    };
    assert_eq!(
        landed(std::slice::from_ref(&file), &[Base::Unreadable], 1),
        Err(NotLanded::BaseUnreadable)
    );
    // Absent is a real answer, and a different one.
    assert_eq!(landed(&[file], &[Base::Absent], 1), Ok(3));
}

#[test]
fn a_rename_is_refused_rather_than_guessed_at() {
    let file = ChangedFile {
        path: "new.rs".into(),
        previous_path: Some("old.rs".into()),
        status: FileStatus::Renamed,
        patch: Some("@@\n+something\n".into()),
        ..ChangedFile::default()
    };
    assert_eq!(
        file_landed(&file, Some("something\n")),
        Err(NotLanded::Renamed)
    );
}

#[test]
fn a_file_with_no_patch_is_refused_rather_than_assumed_clean() {
    let file = ChangedFile {
        path: "logo.png".into(),
        patch: None,
        ..ChangedFile::default()
    };
    assert_eq!(file_landed(&file, None), Err(NotLanded::OpaqueFile));
}

#[test]
fn a_trivial_match_is_too_small_to_be_evidence() {
    let file = changed("a.rs", "@@ -1,2 +1,3 @@\n fn main() {\n+    work();\n }\n");
    let base = "fn main() {\n    work();\n}\n";
    assert_eq!(file_landed(&file, Some(base)), Ok(1));
    assert_eq!(
        landed(&[file], &[present(base)], 3),
        Err(NotLanded::TooSmall)
    );
}

#[test]
fn a_conflicting_small_change_says_so_rather_than_saying_too_small() {
    // The size floor is checked last on purpose: "it does not match" is a truer
    // answer than "it is too small to tell" when we did in fact tell.
    let file = changed("a.rs", "@@ -1,2 +1,2 @@\n keep\n-old\n+new\n");
    assert_eq!(
        landed(&[file], &[present("keep\nold\n")], 3),
        Err(NotLanded::RemovedLineStillPresent)
    );
}

#[test]
fn one_unlanded_file_sinks_the_whole_pull_request() {
    let one = changed(
        "a.rs",
        "@@ -1,2 +1,5 @@\n top\n+alpha\n+beta\n+gamma\n end\n",
    );
    let two = changed(
        "b.rs",
        "@@ -1,2 +1,5 @@\n head\n+delta\n+epsilon\n+zeta\n tail\n",
    );
    let bases = vec![
        present("top\nalpha\nbeta\ngamma\nend\n"),
        present("head\nnothing like it\ntail\n"),
    ];
    assert_eq!(
        landed(&[one, two], &bases, 3),
        Err(NotLanded::AddedLineMissing)
    );
}

#[test]
fn a_wholly_landed_pull_request_reports_how_much_it_checked() {
    let one = changed("a.rs", "@@ -1,2 +1,4 @@\n top\n+alpha\n+beta\n end\n");
    let two = changed("b.rs", "@@ -1,2 +1,2 @@\n head\n-was\n+is\n");
    let bases = vec![present("top\nalpha\nbeta\nend\n"), present("head\nis\n")];
    assert_eq!(landed(&[one, two], &bases, 3), Ok(4));
}

#[test]
fn a_short_base_list_cannot_silently_skip_files() {
    let one = changed(
        "a.rs",
        "@@ -1,2 +1,5 @@\n top\n+alpha\n+beta\n+gamma\n end\n",
    );
    let two = changed("b.rs", "@@\n+nowhere\n");
    assert_eq!(
        landed(&[one, two], &[present("top\nalpha\nbeta\ngamma\nend\n")], 1),
        Err(NotLanded::NoReadableDiff)
    );
}

#[test]
fn deleting_a_file_the_base_branch_still_has_is_not_landed() {
    // The `after` image of a whole-file deletion is empty, and the empty run is
    // "present" in every file — so the images say nothing here and existence is
    // the only question worth asking.
    let file = ChangedFile {
        path: "alive.rs".into(),
        status: FileStatus::Removed,
        patch: Some("@@ -1,3 +0,0 @@\n-fn alive() {}\n-// still here\n-// really\n".into()),
        ..ChangedFile::default()
    };
    assert_eq!(
        file_landed(&file, Some("fn alive() {}\n// edited since\n")),
        Err(NotLanded::FileStillThere)
    );
    // Gone from the branch is still landed.
    assert_eq!(file_landed(&file, None), Ok(3));
}

#[test]
fn an_addition_with_no_surviving_context_has_nothing_to_locate_it_by() {
    // Blank context is dropped, so this hunk keeps no anchor at all. Its lines
    // exist in the base — somewhere — and that is precisely not enough.
    let file = changed("a.rs", "@@ -1,1 +1,4 @@\n \n+alpha\n+beta\n+gamma\n");
    assert_eq!(
        file_landed(&file, Some("something();\nalpha\nbeta\ngamma\n")),
        Err(NotLanded::NoAnchor)
    );
}

#[test]
fn making_a_repeated_change_twice_needs_it_on_the_branch_twice() {
    // The same addition in two identical blocks. Checking each hunk on its own
    // lets both match the single occurrence that is there, and calls a
    // half-applied pull request finished.
    let file = changed(
        "config.rs",
        "@@ -1,2 +1,3 @@\n let block = [\n+    \"new\",\n ];\n\
         @@ -9,2 +9,3 @@\n let block = [\n+    \"new\",\n ];\n",
    );

    let one_applied = "let block = [\n    \"new\",\n];\nlet block = [\n];\n";
    assert_eq!(
        file_landed(&file, Some(one_applied)),
        Err(NotLanded::PartiallyApplied)
    );

    let both_applied = "let block = [\n    \"new\",\n];\nlet block = [\n    \"new\",\n];\n";
    assert_eq!(file_landed(&file, Some(both_applied)), Ok(2));
}

#[test]
fn a_hunk_with_no_context_at_all_is_refused_whichever_way_it_changes() {
    // An empty after image is "present" in every file on earth, so a deletion
    // hunk without context has no evidence either.
    let file = changed("a.rs", "@@ -1,2 +1,1 @@\n-gone\n-also gone\n");
    assert_eq!(
        file_landed(&file, Some("something else\n")),
        Err(NotLanded::NoAnchor)
    );
}

#[test]
fn a_line_that_looks_like_a_diff_header_is_content_inside_a_hunk() {
    // `++counter;` renders as `+++counter;` in a unified diff. Discarding it as
    // a file header drops the line that is the whole difference.
    let got =
        hunks("--- a/x.c\n+++ b/x.c\n@@ -1,3 +1,3 @@\n if (n) {\n---counter;\n+++counter;\n }\n");
    assert_eq!(got.len(), 1);
    assert!(got[0].after.contains(&"++counter;".to_string()), "{got:?}");
    assert!(got[0].before.contains(&"--counter;".to_string()), "{got:?}");
}

#[test]
fn an_addition_into_one_of_two_identical_blocks_names_which() {
    // The base has the addition in the *second* block. The after image matches
    // there, but the first block still reads the old way — and that is the
    // before image, still on the branch.
    let file = changed(
        "config.rs",
        "@@ -1,2 +1,3 @@\n let block = [\n+    \"new\",\n ];\n",
    );
    let second_only = "let block = [\n];\nlet block = [\n    \"new\",\n];\n";
    assert_eq!(
        file_landed(&file, Some(second_only)),
        Err(NotLanded::RemovedLineStillPresent)
    );
}

#[test]
fn a_pure_addition_at_a_file_boundary_is_refused_rather_than_guessed_at() {
    // Context on one side only. The before image is a prefix that survives the
    // change rather than something the change breaks, so it proves nothing, and
    // the hunk cannot say which of two identical blocks it meant.
    let file = changed("a.rs", "@@ -1,1 +1,3 @@\n top\n+alpha\n+beta\n");
    assert_eq!(
        file_landed(&file, Some("top\nalpha\nbeta\n")),
        Err(NotLanded::NoAnchor)
    );
}
