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
fn indentation_does_not_make_the_same_line_a_different_one() {
    assert_eq!(normalise("    let  x =   1;"), "let x = 1;");
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
        Err(NotLanded::AddedLineMissing)
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
    let one = changed("a.rs", "@@ -1,1 +1,4 @@\n top\n+alpha\n+beta\n+gamma\n");
    let two = changed("b.rs", "@@ -1,1 +1,4 @@\n head\n+delta\n+epsilon\n+zeta\n");
    let bases = vec![
        present("top\nalpha\nbeta\ngamma\n"),
        present("head\nnothing like it\n"),
    ];
    assert_eq!(
        landed(&[one, two], &bases, 3),
        Err(NotLanded::AddedLineMissing)
    );
}

#[test]
fn a_wholly_landed_pull_request_reports_how_much_it_checked() {
    let one = changed("a.rs", "@@ -1,1 +1,3 @@\n top\n+alpha\n+beta\n");
    let two = changed("b.rs", "@@ -1,2 +1,2 @@\n head\n-was\n+is\n");
    let bases = vec![present("top\nalpha\nbeta\n"), present("head\nis\n")];
    assert_eq!(landed(&[one, two], &bases, 3), Ok(4));
}

#[test]
fn a_short_base_list_cannot_silently_skip_files() {
    let one = changed("a.rs", "@@ -1,1 +1,4 @@\n top\n+alpha\n+beta\n+gamma\n");
    let two = changed("b.rs", "@@\n+nowhere\n");
    assert_eq!(
        landed(&[one, two], &[present("top\nalpha\nbeta\ngamma\n")], 1),
        Err(NotLanded::NoReadableDiff)
    );
}
