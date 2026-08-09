//! Reading a local git range into the same evidence a pull request produces.
//!
//! Always compiled. This is what `tinysweeper local-review` runs on, and it is
//! the only reason the offline default build spawns a process at all.
//!
//! # Reading a range is not running one
//!
//! The security boundary says contributor code is never executed, and the same
//! rule applies here even though the checkout is the operator's own. `git diff`
//! has two documented ways to run an arbitrary program — `diff.external` and a
//! `textconv` driver named from `.gitattributes` — and both are closed
//! explicitly rather than left to whatever the ambient configuration says:
//! every invocation passes `--no-ext-diff` and `--no-textconv`. `core.hooksPath`
//! is pointed at nowhere so no hook can fire, `core.fsmonitor` is disabled so
//! no monitor daemon is launched, and `GIT_TERMINAL_PROMPT=0` stops a
//! credential prompt turning a read into a hang.
//!
//! # The base is the merge base, not the base tip
//!
//! GitHub shows a pull request as `base...head` — the change *this branch*
//! made, excluding whatever landed on the base branch meanwhile. A two-dot
//! `git diff base head` shows those unrelated commits too, so a lane would
//! review code the author never touched and every anchoring rule in
//! `src/lanes/anchor.rs` would be arguing about somebody else's lines. So the
//! range is resolved through `git merge-base` first, and everything downstream
//! diffs against that.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use tokio::process::Command;

use crate::error::{Error, Result};
use crate::forge::types::{ChangedFile, Commit, FileStatus, RepoId};

/// Field separator inside one `git log` record.
///
/// Unit separator and record separator rather than a newline: a commit message
/// contains newlines by definition, so splitting on one would truncate every
/// body at its first blank line.
const FIELD: char = '\u{1f}';
/// Record separator between `git log` entries.
const RECORD: char = '\u{1e}';

/// How many commits get their patch fetched.
///
/// Matches [`crate::ports::forge::MAX_PATCHED_COMMITS`] so the `commits` lane
/// sees locally exactly what it would see on a pull request — including the
/// point at which patches stop arriving.
const MAX_PATCHED_COMMITS: usize = crate::ports::forge::MAX_PATCHED_COMMITS;

/// Which revisions to review.
#[derive(Debug, Clone)]
pub struct Range {
    /// The base revision, as given on the command line.
    pub base: String,
    /// The head revision. `None` means the working tree, uncommitted changes
    /// included — which is the point of the default, since the change being
    /// iterated on has usually not been committed yet.
    pub head: Option<String>,
}

/// A resolved range, in the shape the forge port speaks.
#[derive(Debug, Clone, Default)]
pub struct ResolvedRange {
    /// The merge base every diff below is taken against.
    pub base_sha: String,
    /// The head commit. With no `--head` this is the tip the working tree sits
    /// on, which is *not* what the diff was taken against — see
    /// [`ResolvedRange::dirty`].
    pub head_sha: String,
    /// Whether the diff includes uncommitted changes.
    ///
    /// Load-bearing for anything that reads a file "at the head commit": when
    /// this is true, `head_sha` names a commit that does not contain the
    /// changes under review, and the working tree does.
    pub dirty: bool,
    /// The files the range changed.
    pub files: Vec<ChangedFile>,
    /// The commits in the range, oldest first.
    pub commits: Vec<Commit>,
}

/// Resolve `range` in the git repository at `dir`.
pub async fn resolve(dir: &Path, range: &Range) -> Result<ResolvedRange> {
    let head_ref = range.head.as_deref().unwrap_or("HEAD");
    let dirty = range.head.is_none();

    let head_sha = rev_parse(dir, head_ref).await?;
    let base_sha = merge_base(dir, &range.base, &head_sha).await?;

    // Two-dot against the merge base is the three-dot diff, and it is also the
    // only form that works when the head is the working tree.
    let mut diff_range = vec![base_sha.clone()];
    if !dirty {
        diff_range.push(head_sha.clone());
    }

    let files = changed_files(dir, &diff_range, dirty, &head_sha).await?;
    let commits = commits(dir, &base_sha, head_ref).await?;

    Ok(ResolvedRange {
        base_sha,
        head_sha,
        dirty,
        files,
        commits,
    })
}

/// Read `path` as it stands at the head of `range`.
///
/// Reads the working tree when the range is dirty, and the head commit
/// otherwise, so the answer always describes the code actually under review.
/// A path that does not exist is `Ok(None)` rather than an error: "this
/// repository has no `AGENTS.md`" is a normal answer, not a failure.
pub async fn file_at(dir: &Path, range: &ResolvedRange, path: &str) -> Result<Option<String>> {
    if range.dirty {
        return match tokio::fs::read_to_string(dir.join(path)).await {
            Ok(content) => Ok(Some(content)),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(Error::path(dir.join(path), err)),
        };
    }

    match git(dir, &["show", &format!("{}:{path}", range.head_sha)]).await {
        Ok(content) => Ok(Some(content)),
        // `git show` cannot distinguish "no such path" from other failures by
        // exit code, and a missing file is the overwhelmingly common case.
        Err(_) => Ok(None),
    }
}

/// The repository this checkout came from, when `origin` names a forge.
///
/// Only cosmetic: it puts a real `owner/name` on the rendered output and on the
/// state key. A checkout with no recognisable remote is not an error — it
/// reviews perfectly well as `local/<directory>`.
pub async fn origin_repo(dir: &Path) -> Option<RepoId> {
    let url = git(dir, &["remote", "get-url", "origin"]).await.ok()?;
    parse_remote(url.trim())
}

/// Extract `owner/name` from an SSH or HTTPS remote URL.
fn parse_remote(url: &str) -> Option<RepoId> {
    let rest = url
        .rsplit_once(':')
        .map(|(_, rest)| rest)
        .filter(|rest| !rest.starts_with("//"))
        .unwrap_or(url);
    let rest = rest.trim_start_matches('/');
    let rest = rest.strip_suffix(".git").unwrap_or(rest);

    // Take the last two segments, so `https://github.com/owner/name` and
    // `git@github.com:owner/name` both land on the same answer.
    let mut segments = rest.rsplit('/');
    let name = segments.next()?;
    let owner = segments.next()?;
    RepoId::parse(&format!("{owner}/{name}"))
}

/// A fallback repository id for a checkout with no usable remote.
pub fn local_repo_id(dir: &Path) -> RepoId {
    let name = dir
        .canonicalize()
        .unwrap_or_else(|_| PathBuf::from("checkout"))
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "checkout".to_string());
    RepoId {
        owner: "local".to_string(),
        name,
    }
}

/// Resolve a revision to a full commit sha.
async fn rev_parse(dir: &Path, revision: &str) -> Result<String> {
    let out = git(
        dir,
        &["rev-parse", "--verify", &format!("{revision}^{{commit}}")],
    )
    .await
    .map_err(|err| {
        Error::Git(format!(
            "`{revision}` is not a revision in this checkout: {err}"
        ))
    })?;
    Ok(out.trim().to_string())
}

/// The commit both revisions descend from.
async fn merge_base(dir: &Path, base: &str, head_sha: &str) -> Result<String> {
    let out = git(dir, &["merge-base", base, head_sha])
        .await
        .map_err(|err| {
            Error::Git(format!(
                "no merge base between `{base}` and the head; is `{base}` fetched? ({err})"
            ))
        })?;
    Ok(out.trim().to_string())
}

/// Every file the range changed, with its patch.
async fn changed_files(
    dir: &Path,
    diff_range: &[String],
    dirty: bool,
    head_sha: &str,
) -> Result<Vec<ChangedFile>> {
    let statuses = name_status(dir, diff_range).await?;
    let counts = numstat(dir, diff_range).await?;

    let mut files = Vec::with_capacity(statuses.len());
    if dirty {
        // A file you have written but not yet `git add`ed is invisible to
        // `git diff`, and it is also the single most likely thing to be under
        // review — the new module you are iterating on. Skipping it silently
        // would be the "quietly reviewed half the change" failure, so it is
        // added here rather than left out or fixed by mutating the index.
        files.extend(untracked_files(dir).await?);
    }
    for (path, (status, previous_path)) in statuses {
        let (additions, deletions) = counts.get(&path).copied().unwrap_or((0, 0));
        // One `git diff` per file rather than one for the range split on
        // `diff --git`: a path containing a space makes that header ambiguous,
        // and a mis-attributed patch anchors every finding in it to the wrong
        // file. The extra processes are the price of never doing that.
        let patch = file_patch(dir, diff_range, &path).await?;
        let size_bytes = match status {
            FileStatus::Removed => None,
            _ => blob_size(dir, dirty, head_sha, &path).await,
        };

        files.push(ChangedFile {
            path,
            previous_path,
            status,
            additions,
            deletions,
            patch,
            size_bytes,
        });
    }
    Ok(files)
}

/// Files in the working tree that git is not tracking yet.
///
/// `--exclude-standard` means `.gitignore` is honoured, so build output and
/// `target/` do not arrive as a thousand added files.
async fn untracked_files(dir: &Path) -> Result<Vec<ChangedFile>> {
    let raw = git(dir, &["ls-files", "--others", "--exclude-standard", "-z"]).await?;

    let mut files = Vec::new();
    for path in raw.split('\0').filter(|p| !p.is_empty()) {
        // `git diff --no-index` renders a proper unified diff for a file the
        // index has never seen, without an `add -N` that would leave the
        // operator's index modified after a read-only command.
        let patch = no_index_patch(dir, path).await;
        let additions = patch
            .as_deref()
            .map(|patch| {
                patch
                    .lines()
                    .filter(|line| line.starts_with('+') && !line.starts_with("+++"))
                    .count() as u64
            })
            .unwrap_or(0);

        files.push(ChangedFile {
            path: path.to_string(),
            previous_path: None,
            status: FileStatus::Added,
            additions,
            deletions: 0,
            patch,
            size_bytes: tokio::fs::metadata(dir.join(path))
                .await
                .ok()
                .map(|m| m.len()),
        });
    }
    Ok(files)
}

/// The diff of a new file against nothing.
///
/// `git diff --no-index` reports "files differ" with exit status 1, which is
/// its normal answer here rather than a failure — so the status is ignored and
/// only the absence of output is treated as "no patch".
async fn no_index_patch(dir: &Path, path: &str) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "core.hooksPath=/dev/null"])
        .args(["-c", "core.fsmonitor=false"])
        .args(["-c", "diff.external="])
        .args([
            "diff",
            "--no-index",
            "--no-ext-diff",
            "--no-textconv",
            "--",
            "/dev/null",
            path,
        ])
        // `GIT_CONFIG_KEY_n` and `GIT_CONFIG_PARAMETERS` inject configuration
        // at the same precedence as `-c`, so the flags above cannot be relied
        // on to displace an inherited `diff.external`. The ambient environment
        // is dropped rather than argued with.
        .env_remove("GIT_CONFIG_COUNT")
        .env_remove("GIT_CONFIG_PARAMETERS")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_PAGER", "cat")
        .stdin(Stdio::null())
        .output()
        .await
        .ok()?;

    let patch = String::from_utf8_lossy(&output.stdout).into_owned();
    (!patch.trim().is_empty()).then_some(patch)
}

/// `path -> (status, previous_path)` for the range.
async fn name_status(
    dir: &Path,
    diff_range: &[String],
) -> Result<Vec<(String, (FileStatus, Option<String>))>> {
    let mut args = diff_args(diff_range);
    args.push("--name-status".into());
    args.push("-z".into());
    let raw = git_owned(dir, &args).await?;

    let mut out = Vec::new();
    let mut fields = raw.split('\0').filter(|f| !f.is_empty());
    while let Some(code) = fields.next() {
        let Some(letter) = code.chars().next() else {
            continue;
        };
        // A rename or copy carries two paths; everything else carries one.
        // Reading the wrong number here desynchronises the rest of the stream,
        // so the branch is on the status letter rather than on a guess.
        if matches!(letter, 'R' | 'C') {
            let (Some(previous), Some(path)) = (fields.next(), fields.next()) else {
                break;
            };
            out.push((
                path.to_string(),
                (FileStatus::Renamed, Some(previous.to_string())),
            ));
            continue;
        }
        let Some(path) = fields.next() else { break };
        let status = match letter {
            'A' => FileStatus::Added,
            'D' => FileStatus::Removed,
            _ => FileStatus::Modified,
        };
        out.push((path.to_string(), (status, None)));
    }
    Ok(out)
}

/// `path -> (additions, deletions)` for the range.
async fn numstat(dir: &Path, diff_range: &[String]) -> Result<BTreeMap<String, (u64, u64)>> {
    let mut args = diff_args(diff_range);
    args.push("--numstat".into());
    args.push("-z".into());
    let raw = git_owned(dir, &args).await?;

    let mut out = BTreeMap::new();
    let mut fields = raw.split('\0').filter(|f| !f.is_empty());
    while let Some(record) = fields.next() {
        let mut parts = record.splitn(3, '\t');
        let (Some(added), Some(removed)) = (parts.next(), parts.next()) else {
            continue;
        };
        // A binary file reports `-` for both counts. Zero is the honest answer:
        // no *lines* changed, and `ChangedFile::evidence_missing` depends on
        // that to tell a binary apart from a patch the forge truncated.
        let added = added.parse().unwrap_or(0);
        let removed = removed.parse().unwrap_or(0);
        // With `-z`, a rename emits an empty third field and then the two paths
        // as separate records.
        let path = match parts.next() {
            Some(path) if !path.is_empty() => path.to_string(),
            _ => {
                let _previous = fields.next();
                match fields.next() {
                    Some(path) => path.to_string(),
                    None => break,
                }
            }
        };
        out.insert(path, (added, removed));
    }
    Ok(out)
}

/// The unified diff of one file, or `None` when git produced no text for it.
async fn file_patch(dir: &Path, diff_range: &[String], path: &str) -> Result<Option<String>> {
    let mut args = diff_args(diff_range);
    args.push("--".into());
    args.push(path.to_string());
    let patch = git_owned(dir, &args).await?;
    Ok((!patch.trim().is_empty()).then_some(patch))
}

/// The size of `path` at the head, when it can be read cheaply.
///
/// `None` on any failure: a missing size costs the blob scanner one check, and
/// failing the whole review over it would be worse.
async fn blob_size(dir: &Path, dirty: bool, head_sha: &str, path: &str) -> Option<u64> {
    if dirty {
        return tokio::fs::metadata(dir.join(path))
            .await
            .ok()
            .map(|m| m.len());
    }
    git(dir, &["cat-file", "-s", &format!("{head_sha}:{path}")])
        .await
        .ok()
        .and_then(|out| out.trim().parse().ok())
}

/// The commits in `base_sha..head_ref`, oldest first, with patches for the
/// first [`MAX_PATCHED_COMMITS`].
async fn commits(dir: &Path, base_sha: &str, head_ref: &str) -> Result<Vec<Commit>> {
    let format = format!("--format=%H{FIELD}%an{FIELD}%ae{FIELD}%B{RECORD}");
    let raw = git(
        dir,
        &[
            "log",
            "--reverse",
            &format,
            &format!("{base_sha}..{head_ref}"),
        ],
    )
    .await?;

    let mut commits = Vec::new();
    for record in raw.split(RECORD) {
        let record = record.trim_start_matches('\n');
        if record.trim().is_empty() {
            continue;
        }
        let mut fields = record.splitn(4, FIELD);
        let (Some(sha), Some(author_name), Some(author_email), Some(message)) =
            (fields.next(), fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        commits.push(Commit {
            sha: sha.trim().to_string(),
            message: message.trim_end().to_string(),
            author_name: author_name.to_string(),
            author_email: author_email.to_string(),
            patch: None,
        });
    }

    for commit in commits.iter_mut().take(MAX_PATCHED_COMMITS) {
        let patch = git(
            dir,
            &[
                "show",
                "--no-ext-diff",
                "--no-textconv",
                "--format=",
                "--patch",
                &commit.sha,
            ],
        )
        .await
        .unwrap_or_default();
        commit.patch = (!patch.trim().is_empty()).then_some(patch);
    }

    Ok(commits)
}

/// The `git diff` prefix shared by every diff invocation.
fn diff_args(diff_range: &[String]) -> Vec<String> {
    let mut args = vec![
        "diff".to_string(),
        // Both of these can otherwise run a program named by the repository's
        // own configuration. See the module doc.
        "--no-ext-diff".to_string(),
        "--no-textconv".to_string(),
        "--find-renames".to_string(),
    ];
    args.extend(diff_range.iter().cloned());
    args
}

/// Run `git` in `dir` and return its stdout.
async fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let owned: Vec<String> = args.iter().map(|a| a.to_string()).collect();
    git_owned(dir, &owned).await
}

/// Run `git` in `dir` and return its stdout.
///
/// The only program this module will ever spawn. Every configuration knob that
/// could turn a read into an execution is overridden on the command line rather
/// than assumed, because the ambient `git config` is not ours to trust.
async fn git_owned(dir: &Path, args: &[String]) -> Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "core.hooksPath=/dev/null"])
        .args(["-c", "core.fsmonitor=false"])
        .args(["-c", "diff.external="])
        .args(args)
        // `GIT_CONFIG_KEY_n` and `GIT_CONFIG_PARAMETERS` inject configuration
        // at the same precedence as `-c`, so the flags above cannot be relied
        // on to displace an inherited `diff.external`. The ambient environment
        // is dropped rather than argued with.
        .env_remove("GIT_CONFIG_COUNT")
        .env_remove("GIT_CONFIG_PARAMETERS")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_PAGER", "cat")
        .stdin(Stdio::null())
        .output()
        .await
        .map_err(|err| Error::Git(format!("could not run git: {err}")))?;

    if !output.status.success() {
        return Err(Error::Git(format!(
            "git {} failed: {}",
            args.first().map(String::as_str).unwrap_or(""),
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }

    // Lossy rather than strict: a patch can legitimately carry bytes that are
    // not UTF-8, and losing a review over one mojibake line would be worse than
    // showing the model a replacement character.
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// A throwaway git repository, for tests that need a real range.
///
/// Lives here rather than in a test file because both this module's tests and
/// `src/app/local_test.rs` need one, and a second hand-rolled copy of "set up a
/// repository" is a second place for the setup to drift from what git actually
/// does.
#[cfg(test)]
pub(crate) mod fixture {
    use std::path::Path;
    use std::process::Command;

    /// A temporary repository with an initial commit on `main`.
    pub struct Repo {
        dir: tempfile::TempDir,
    }

    impl Repo {
        /// Create one, with `README.md` committed on `main`.
        pub fn new() -> Self {
            let dir = tempfile::tempdir().expect("tempdir");
            let repo = Self { dir };
            repo.git(&["init", "-b", "main"]);
            repo.write("README.md", "# fixture\n");
            repo.commit("chore: initial commit");
            repo
        }

        /// The repository root.
        pub fn path(&self) -> &Path {
            self.dir.path()
        }

        /// Write `content` to `path`, creating parent directories.
        pub fn write(&self, path: &str, content: &str) {
            let full = self.dir.path().join(path);
            if let Some(parent) = full.parent() {
                std::fs::create_dir_all(parent).expect("mkdir");
            }
            std::fs::write(full, content).expect("write");
        }

        /// Delete `path` from the working tree.
        pub fn remove(&self, path: &str) {
            std::fs::remove_file(self.dir.path().join(path)).expect("remove");
        }

        /// Stage everything and commit it.
        pub fn commit(&self, message: &str) {
            self.git(&["add", "-A"]);
            self.git(&["commit", "-m", message]);
        }

        /// Run a git command, panicking with its stderr on failure.
        pub fn git(&self, args: &[&str]) -> String {
            let output = Command::new("git")
                .arg("-C")
                .arg(self.dir.path())
                // Identity and signing are set per-invocation so the test does
                // not depend on — or disturb — whatever the host has globally.
                .args(["-c", "user.name=fixture"])
                .args(["-c", "user.email=fixture@example.invalid"])
                .args(["-c", "commit.gpgsign=false"])
                .args(["-c", "init.defaultBranch=main"])
                .args(args)
                // The host's configuration is disowned entirely, not merely
                // overridden key by key. A developer machine with a
                // `prepare-commit-msg` hook writes extra trailers into every
                // commit, which made this fixture's commit messages depend on
                // whose laptop the suite ran on — and `GIT_CONFIG_KEY_n` sets
                // configuration at the same precedence as `-c`, so overriding
                // the key on the command line is not enough to displace it.
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_SYSTEM", "/dev/null")
                .env_remove("GIT_CONFIG_COUNT")
                .env_remove("GIT_CONFIG_PARAMETERS")
                .env("GIT_TERMINAL_PROMPT", "0")
                .output()
                .expect("git runs");
            assert!(
                output.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8_lossy(&output.stdout).into_owned()
        }
    }
}

#[cfg(test)]
#[path = "git_test.rs"]
mod plumbing_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ssh_and_https_remotes_name_the_same_repository() {
        for url in [
            "git@github.com:tinyhumansai/tinysweeper.git",
            "https://github.com/tinyhumansai/tinysweeper.git",
            "https://github.com/tinyhumansai/tinysweeper",
            "ssh://git@github.com/tinyhumansai/tinysweeper.git",
        ] {
            assert_eq!(
                parse_remote(url).map(|r| r.to_string()).as_deref(),
                Some("tinyhumansai/tinysweeper"),
                "failed on {url}"
            );
        }
    }

    #[test]
    fn a_remote_with_no_owner_is_not_a_repository_id() {
        assert!(parse_remote("").is_none());
        assert!(parse_remote("tinysweeper").is_none());
    }

    #[test]
    fn every_diff_closes_the_two_ways_git_runs_a_program() {
        let args = diff_args(&["abc123".to_string()]);
        assert!(args.contains(&"--no-ext-diff".to_string()));
        assert!(args.contains(&"--no-textconv".to_string()));
    }
}
