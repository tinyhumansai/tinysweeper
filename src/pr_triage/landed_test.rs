//! Tests for the "already on the base branch" detector.
//!
//! These are the golden tests of the close path: every one of them is a shape
//! of pull request that a sweep will actually meet on a busy repository, and
//! the refusals matter more than the acceptances.

use super::*;
use crate::forge::types::{ChangedFile, FileStatus};

fn changed(path: &str, patch: &str) -> ChangedFile {
    ChangedFile {
        path: path.into(),
        patch: Some(patch.into()),
        ..ChangedFile::default()
    }
}

#[test]
fn a_run_is_broken_by_context_and_by_blank_lines() {
    let got = runs("@@\n context\n+alpha\n+beta\n context\n+gamma\n");
    assert_eq!(
        got.added,
        vec![vec!["alpha".to_string(), "beta".into()], vec!["gamma".into()]]
    );

    // The blank line ends the run rather than being carried inside it: two
    // stretches separated by whitespace exist separately on the base branch,
    // and joining them would look for a block that is nowhere.
    let split = runs("@@\n+alpha\n+   \n+beta\n");
    assert_eq!(
        split.added,
        vec![vec!["alpha".to_string()], vec!["beta".to_string()]]
    );
}

#[test]
fn diff_headers_are_not_mistaken_for_content() {
    let got = runs("--- a/x.rs\n+++ b/x.rs\n@@ -1 +1 @@\n-old\n+new\n");
    assert_eq!(got.added, vec![vec!["new".to_string()]]);
    assert_eq!(got.removed, vec![vec!["old".to_string()]]);
}

#[test]
fn indentation_does_not_make_the_same_line_a_different_one() {
    assert_eq!(normalise("    let  x =   1;"), "let x = 1;");
}

#[test]
fn a_change_already_on_the_base_branch_is_landed() {
    let file = changed("README.md", "@@ -1 +1 @@\n-Rust 1.93.0\n+Rust 1.96.1\n");
    let base = "# Title\nRust 1.96.1\nmore\n";
    assert!(file_landed(&file, Some(base)).is_ok());
}

#[test]
fn the_removed_line_is_what_makes_a_one_line_change_safe() {
    // Same patch, base branch not yet updated. The added line is absent, but
    // even if it were present somewhere the surviving old line would refuse.
    let file = changed("README.md", "@@ -1 +1 @@\n-Rust 1.93.0\n+Rust 1.96.1\n");
    let base = "# Title\nRust 1.93.0\nRust 1.96.1 is coming\n";
    assert_eq!(
        file_landed(&file, Some(base)),
        Err(NotLanded::RemovedLineStillPresent)
    );
}

#[test]
fn an_added_run_must_be_consecutive_on_the_base_branch() {
    let file = changed("a.rs", "@@\n+let a = 1;\n+let b = 2;\n");
    // Both lines exist, but not together — so this is a different change that
    // happens to reuse familiar lines.
    let scattered = "let a = 1;\nsomething();\nlet b = 2;\n";
    assert_eq!(
        file_landed(&file, Some(scattered)),
        Err(NotLanded::AddedLineMissing)
    );
    assert!(file_landed(&file, Some("let a = 1;\nlet b = 2;\n")).is_ok());
}

#[test]
fn deleting_a_file_somebody_already_deleted_is_landed() {
    let file = ChangedFile {
        path: "dead.rs".into(),
        status: FileStatus::Removed,
        patch: Some("@@ -1,2 +0,0 @@\n-fn dead() {}\n-// gone\n".into()),
        ..ChangedFile::default()
    };
    assert!(file_landed(&file, None).is_ok());
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
    assert_eq!(file_landed(&file, Some("something\n")), Err(NotLanded::Renamed));
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
    let file = changed("a.rs", "@@\n+}\n");
    let base = "fn main() {\n}\n";
    // The file itself matches — and that is exactly the coincidence the floor
    // exists to refuse.
    assert!(file_landed(&file, Some(base)).is_ok());
    assert_eq!(
        landed(&[file], &[Some(base.into())], 3),
        Err(NotLanded::TooSmall)
    );
}

#[test]
fn a_conflicting_small_change_says_so_rather_than_saying_too_small() {
    // The size floor is checked last on purpose: "it does not match" is a
    // truer answer than "it is too small to tell" when we did in fact tell.
    let file = changed("a.rs", "@@\n-old\n+new\n");
    assert_eq!(
        landed(&[file], &[Some("old\n".to_string())], 3),
        Err(NotLanded::RemovedLineStillPresent)
    );
}

#[test]
fn one_unlanded_file_sinks_the_whole_pull_request() {
    let landed_file = changed("a.rs", "@@\n+alpha\n+beta\n+gamma\n");
    let other = changed("b.rs", "@@\n+delta\n+epsilon\n+zeta\n");
    let bases = vec![
        Some("alpha\nbeta\ngamma\n".to_string()),
        Some("nothing like it\n".to_string()),
    ];
    assert_eq!(
        landed(&[landed_file, other], &bases, 3),
        Err(NotLanded::AddedLineMissing)
    );
}

#[test]
fn a_wholly_landed_pull_request_reports_how_much_it_checked() {
    let one = changed("a.rs", "@@\n+alpha\n+beta\n");
    let two = changed("b.rs", "@@ -1 +1 @@\n-was\n+is\n");
    let bases = vec![
        Some("alpha\nbeta\n".to_string()),
        Some("is\n".to_string()),
    ];
    assert_eq!(landed(&[one, two], &bases, 3), Ok(4));
}

#[test]
fn a_short_base_list_cannot_silently_skip_files() {
    let one = changed("a.rs", "@@\n+alpha\n+beta\n+gamma\n");
    let two = changed("b.rs", "@@\n+nowhere\n");
    assert_eq!(
        landed(&[one, two], &[Some("alpha\nbeta\ngamma\n".into())], 1),
        Err(NotLanded::NoReadableDiff)
    );
}
