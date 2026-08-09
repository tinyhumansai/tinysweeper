//! Resolving a real git range, against a real git.
//!
//! These drive `git` in a throwaway repository rather than parsing canned
//! output. The formats being read here — `--numstat -z`, `--name-status -z` —
//! are the ones that change shape for renames and binaries, and a fixture of
//! what git printed once is exactly the test that keeps passing after git stops
//! printing it.

use super::fixture::Repo;
use super::*;

/// The whole working tree, diffed against `main`.
fn worktree() -> Range {
    Range {
        base: "main".to_string(),
        head: None,
    }
}

/// A committed head, diffed against `main`.
fn committed() -> Range {
    Range {
        base: "main".to_string(),
        head: Some("HEAD".to_string()),
    }
}

#[tokio::test]
async fn a_modified_file_carries_its_patch_and_its_line_counts() {
    let repo = Repo::new();
    repo.git(&["checkout", "-b", "feature"]);
    repo.write("src/lib.rs", "fn one() {}\nfn two() {}\n");
    repo.commit("feat: add two functions");

    let range = resolve(repo.path(), &committed()).await.expect("resolves");

    assert_eq!(range.files.len(), 1, "{:?}", range.files);
    let file = &range.files[0];
    assert_eq!(file.path, "src/lib.rs");
    assert_eq!(file.status, FileStatus::Added);
    assert_eq!(file.additions, 2);
    assert_eq!(file.deletions, 0);
    assert!(
        file.patch
            .as_deref()
            .unwrap_or_default()
            .contains("+fn two"),
        "{:?}",
        file.patch
    );
    assert!(!range.dirty);
}

#[tokio::test]
async fn uncommitted_work_is_reviewed_when_no_head_is_named() {
    let repo = Repo::new();
    repo.write("src/lib.rs", "fn uncommitted() {}\n");

    let range = resolve(repo.path(), &worktree()).await.expect("resolves");

    assert!(range.dirty, "the working tree is the head");
    assert_eq!(range.files.len(), 1);
    assert_eq!(range.files[0].path, "src/lib.rs");
    // The head commit does not contain the change under review. Anything that
    // reads a file "at the head" has to know that.
    assert_eq!(range.commits.len(), 0);
}

#[tokio::test]
async fn an_untracked_file_is_reviewed_but_an_ignored_one_is_not() {
    let repo = Repo::new();
    repo.write(".gitignore", "target/\n");
    repo.commit("chore: ignore build output");

    repo.write("src/brand_new.rs", "fn brand_new() {}\n");
    repo.write("target/debug/huge.bin", "build output\n");

    let range = resolve(repo.path(), &worktree()).await.expect("resolves");
    let paths: Vec<&str> = range.files.iter().map(|f| f.path.as_str()).collect();

    // `git diff` alone reports neither: one is untracked, the other ignored.
    // Reviewing the first is the whole point of a working-tree range; reviewing
    // the second would drown the review in build output.
    assert!(paths.contains(&"src/brand_new.rs"), "{paths:?}");
    assert!(!paths.iter().any(|p| p.starts_with("target/")), "{paths:?}");

    let new_file = range
        .files
        .iter()
        .find(|f| f.path == "src/brand_new.rs")
        .expect("present");
    assert_eq!(new_file.status, FileStatus::Added);
    assert_eq!(new_file.additions, 1);
    assert!(
        new_file
            .patch
            .as_deref()
            .unwrap_or_default()
            .contains("+fn brand_new"),
        "{:?}",
        new_file.patch
    );
}

#[tokio::test]
async fn reviewing_an_untracked_file_does_not_stage_it() {
    let repo = Repo::new();
    repo.write("src/brand_new.rs", "fn brand_new() {}\n");

    resolve(repo.path(), &worktree()).await.expect("resolves");

    // `git add -N` would have made this diff non-empty. A read-only command
    // must leave the operator's index exactly as it found it.
    let staged = repo.git(&["diff", "--cached", "--name-only"]);
    assert!(staged.trim().is_empty(), "index was modified: {staged}");
    let status = repo.git(&["status", "--porcelain", "src/brand_new.rs"]);
    assert!(status.starts_with("??"), "still untracked: {status}");
}

#[tokio::test]
async fn a_rename_keeps_the_path_it_came_from() {
    let repo = Repo::new();
    repo.write("src/old.rs", "fn keep_me_long_enough_to_detect() {}\n");
    repo.commit("chore: add a file to rename");
    repo.git(&["checkout", "-b", "feature"]);
    repo.git(&["mv", "src/old.rs", "src/new.rs"]);
    repo.commit("refactor: rename the file");

    let range = resolve(repo.path(), &committed()).await.expect("resolves");

    let renamed = range
        .files
        .iter()
        .find(|f| f.path == "src/new.rs")
        .expect("the new path is reported");
    assert_eq!(renamed.status, FileStatus::Renamed);
    assert_eq!(renamed.previous_path.as_deref(), Some("src/old.rs"));
}

#[tokio::test]
async fn a_deleted_file_is_reported_with_no_size() {
    let repo = Repo::new();
    repo.write("src/gone.rs", "fn gone() {}\n");
    repo.commit("chore: add a file to delete");
    repo.git(&["checkout", "-b", "feature"]);
    repo.remove("src/gone.rs");
    repo.commit("chore: delete it");

    let range = resolve(repo.path(), &committed()).await.expect("resolves");

    let gone = range
        .files
        .iter()
        .find(|f| f.path == "src/gone.rs")
        .expect("reported");
    assert_eq!(gone.status, FileStatus::Removed);
    // There is no blob at the head to measure, and reporting zero would tell
    // the blob scanner a four-megabyte deletion was empty.
    assert_eq!(gone.size_bytes, None);
}

#[tokio::test]
async fn a_binary_file_reports_no_line_counts() {
    let repo = Repo::new();
    repo.git(&["checkout", "-b", "feature"]);
    std::fs::write(repo.path().join("logo.bin"), [0u8, 159, 146, 150, 0, 255]).expect("write");
    repo.commit("chore: add a binary");

    let range = resolve(repo.path(), &committed()).await.expect("resolves");

    let binary = range
        .files
        .iter()
        .find(|f| f.path == "logo.bin")
        .expect("reported");
    // Zero, not a parse failure: `ChangedFile::evidence_missing` distinguishes
    // a binary (no lines changed) from a patch the forge truncated (lines
    // changed, patch withheld), and a wrong count here collapses the two.
    assert_eq!((binary.additions, binary.deletions), (0, 0));
    assert!(!binary.evidence_missing());
}

#[tokio::test]
async fn the_range_is_taken_from_the_merge_base_not_the_base_tip() {
    let repo = Repo::new();
    repo.git(&["checkout", "-b", "feature"]);
    repo.write("src/mine.rs", "fn mine() {}\n");
    repo.commit("feat: my change");

    // Somebody else lands on main after the branch was cut.
    repo.git(&["checkout", "main"]);
    repo.write("src/theirs.rs", "fn theirs() {}\n");
    repo.commit("feat: their change");
    repo.git(&["checkout", "feature"]);

    let range = resolve(repo.path(), &committed()).await.expect("resolves");

    let paths: Vec<&str> = range.files.iter().map(|f| f.path.as_str()).collect();
    assert_eq!(
        paths,
        ["src/mine.rs"],
        "a two-dot diff would have pulled in src/theirs.rs, which this branch \
         never touched and its author never wrote"
    );
}

#[tokio::test]
async fn commits_arrive_oldest_first_with_their_patches() {
    let repo = Repo::new();
    repo.git(&["checkout", "-b", "feature"]);
    repo.write("src/a.rs", "fn a() {}\n");
    repo.commit("feat: first\n\nWith a body that spans\nmore than one line.");
    repo.write("src/b.rs", "fn b() {}\n");
    repo.commit("feat: second");

    let range = resolve(repo.path(), &committed()).await.expect("resolves");

    assert_eq!(range.commits.len(), 2);
    assert!(range.commits[0].message.starts_with("feat: first"));
    // A message body contains newlines, so a record separator that was a
    // newline would have truncated this.
    assert!(range.commits[0].message.contains("more than one line."));
    assert_eq!(range.commits[1].message, "feat: second");
    assert!(
        range.commits[1]
            .patch
            .as_deref()
            .unwrap_or_default()
            .contains("+fn b"),
        "{:?}",
        range.commits[1].patch
    );
    assert_eq!(range.commits[0].author_email, "fixture@example.invalid");
}

#[tokio::test]
async fn an_unknown_base_is_an_error_that_names_it() {
    let repo = Repo::new();
    let range = Range {
        base: "origin/does-not-exist".to_string(),
        head: None,
    };

    let err = resolve(repo.path(), &range)
        .await
        .expect_err("no such base");
    assert!(
        err.to_string().contains("origin/does-not-exist"),
        "the message has to name the revision the operator typed: {err}"
    );
}

#[tokio::test]
async fn a_file_is_read_from_the_working_tree_when_the_range_is_dirty() {
    let repo = Repo::new();
    repo.write("AGENTS.md", "committed rules\n");
    repo.commit("docs: add agent rules");
    repo.write("AGENTS.md", "uncommitted rules\n");

    let range = resolve(repo.path(), &worktree()).await.expect("resolves");
    let content = file_at(repo.path(), &range, "AGENTS.md")
        .await
        .expect("reads")
        .expect("present");

    // The diff under review includes the uncommitted edit, so the rules the
    // extraction pass reads must be the uncommitted ones too.
    assert_eq!(content.trim(), "uncommitted rules");
}

#[tokio::test]
async fn a_file_is_read_at_the_head_commit_when_the_range_is_clean() {
    let repo = Repo::new();
    repo.write("AGENTS.md", "committed rules\n");
    repo.commit("docs: add agent rules");
    repo.write("AGENTS.md", "uncommitted rules\n");

    let range = resolve(repo.path(), &committed()).await.expect("resolves");
    let content = file_at(repo.path(), &range, "AGENTS.md")
        .await
        .expect("reads")
        .expect("present");

    assert_eq!(content.trim(), "committed rules");
}

#[tokio::test]
async fn a_missing_file_is_absent_rather_than_an_error() {
    let repo = Repo::new();
    let range = resolve(repo.path(), &worktree()).await.expect("resolves");

    assert!(
        file_at(repo.path(), &range, "AGENTS.md")
            .await
            .expect("no error")
            .is_none(),
        "a repository without an AGENTS.md is normal, not broken"
    );
}

#[tokio::test]
async fn a_checkout_with_no_remote_still_has_a_repository_id() {
    let repo = Repo::new();
    assert!(origin_repo(repo.path()).await.is_none());

    let id = local_repo_id(repo.path());
    assert_eq!(id.owner, "local");
    assert!(!id.name.is_empty());
}
