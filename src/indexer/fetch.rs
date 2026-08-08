//! Getting a repository's tree onto disk so it can be indexed. Requires `serve`.
//!
//! Gated on `serve` because only the server indexes: the CLI is handed a
//! checkout it already has.
//!
//! # Reading a tree is not running one
//!
//! The security boundary says contributor code is never executed, and a
//! shallow `git fetch` of one commit does not execute any: no build, no
//! dependency install, no repository script. The two ways git *could* run
//! something are both closed explicitly rather than left to defaults —
//! `core.hooksPath` is pointed at nowhere so a hook in the fetched history
//! cannot fire, and `GIT_TERMINAL_PROMPT=0` stops a credential prompt turning
//! a fetch into a hang. [`Checkout::fetch`] is the only place in the crate that
//! spawns a process, and the only program it will spawn is `git`.
//!
//! # The token is not in argv
//!
//! The obvious way to authenticate is `https://x-access-token:TOKEN@github.com/…`
//! as the remote URL, and it puts an installation token in the process table
//! for anything on the host to read. Git's `GIT_CONFIG_KEY_n` / `GIT_CONFIG_VALUE_n`
//! environment protocol takes the same config through the environment instead,
//! so the header is set without the credential ever appearing on a command
//! line. The temporary directory is removed when the [`Checkout`] is dropped.

use std::path::Path;
use std::process::Stdio;

use tokio::process::Command;

use crate::error::{Error, Result};

/// How long one git invocation may take.
///
/// A cold clone of a large repository is legitimately slow, so this is generous;
/// it exists to stop a hung network turning an indexing worker into a permanent
/// one, not to police clone size.
pub const GIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10 * 60);

/// A shallow checkout of exactly one commit, deleted when dropped.
#[derive(Debug)]
pub struct Checkout {
    dir: tempfile::TempDir,
    revision: String,
}

impl Checkout {
    /// Fetch `revision` of `repo` into a fresh temporary directory.
    ///
    /// `token` is an installation token with read access. It is a *read*
    /// credential: nothing in this module pushes, and the write token is still
    /// minted separately after every model call has returned.
    pub async fn fetch(host: &str, repo: &str, revision: &str, token: &str) -> Result<Self> {
        // Validated rather than trusted. `repo` and `revision` come from a
        // webhook payload and are about to be command arguments; a `--`-leading
        // value would be read as an option by git even though it cannot escape
        // the argv boundary into a shell.
        if revision.len() != 40 || !revision.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(Error::config(format!(
                "`{revision}` is not a commit sha; the indexer fetches one commit by id"
            )));
        }
        if repo.starts_with('-') || !repo.contains('/') || repo.contains("..") {
            return Err(Error::config(format!("`{repo}` is not owner/name")));
        }

        let dir = tempfile::Builder::new()
            .prefix("tinysweeper-index-")
            .tempdir()
            .map_err(|err| Error::Forge(format!("could not make a checkout directory: {err}")))?;
        let root = dir.path().to_path_buf();
        let url = format!("https://{host}/{repo}.git");

        git(&root, token, &["init", "--quiet"]).await?;
        // `--depth 1` of one commit id: the whole history is never on disk, so
        // indexing a monorepo costs its tree rather than its history. `--no-tags`
        // because a tag is a ref we would fetch and never look at.
        git(
            &root,
            token,
            &[
                "fetch",
                "--quiet",
                "--depth",
                "1",
                "--no-tags",
                &url,
                revision,
            ],
        )
        .await?;
        git(
            &root,
            token,
            &["checkout", "--quiet", "--detach", "FETCH_HEAD"],
        )
        .await?;

        Ok(Self {
            dir,
            revision: revision.to_string(),
        })
    }

    /// The directory the tree was checked out into.
    pub fn path(&self) -> &Path {
        self.dir.path()
    }

    /// The commit this checkout reflects.
    pub fn revision(&self) -> &str {
        &self.revision
    }
}

/// Run one git command in `root`, with hooks and prompts disabled.
async fn git(root: &Path, token: &str, args: &[&str]) -> Result<()> {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(root)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        // Nothing inherited. A `GIT_CONFIG_COUNT` already in the environment
        // would silently renumber the pairs set below, and the ambient
        // `~/.gitconfig` of whoever runs the server is not policy this program
        // should be subject to.
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("HOME", root)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_COUNT", "2")
        // A hook in the fetched history must not run. This is the invariant,
        // not a hardening measure: it is what makes "we read the tree, we do
        // not execute it" true of a fetch.
        .env("GIT_CONFIG_KEY_0", "core.hooksPath")
        .env("GIT_CONFIG_VALUE_0", "/dev/null")
        .env("GIT_CONFIG_KEY_1", "http.extraHeader")
        .env(
            "GIT_CONFIG_VALUE_1",
            format!("Authorization: Basic {}", basic_auth(token)),
        );

    let output = tokio::time::timeout(GIT_TIMEOUT, command.output())
        .await
        .map_err(|_| Error::Forge(format!("git {} timed out", args[0])))?
        .map_err(|err| Error::Forge(format!("could not run git: {err}")))?;

    if !output.status.success() {
        // Scrubbed before it is ever formatted. git echoes the URL it was given
        // on failure, and while the token is not in the URL here, a message that
        // reaches a log must not be the one place a credential could surface.
        let message = crate::scan::secrets::scrub(&String::from_utf8_lossy(&output.stderr));
        return Err(Error::Forge(format!(
            "git {} failed: {}",
            args[0],
            message.trim()
        )));
    }
    Ok(())
}

/// `base64(x-access-token:<token>)`, for git's HTTP basic auth.
///
/// Hand-rolled for the same reason `Finding::fingerprint` hand-rolls hex: a
/// dependency for twenty lines that will never change is a dependency to audit
/// forever.
fn basic_auth(token: &str) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let raw = format!("x-access-token:{token}");
    let bytes = raw.as_bytes();
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for group in bytes.chunks(3) {
        let b0 = group[0] as u32;
        let b1 = *group.get(1).unwrap_or(&0) as u32;
        let b2 = *group.get(2).unwrap_or(&0) as u32;
        let packed = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[(packed >> 18) as usize & 63] as char);
        out.push(ALPHABET[(packed >> 12) as usize & 63] as char);
        out.push(if group.len() > 1 {
            ALPHABET[(packed >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if group.len() > 2 {
            ALPHABET[packed as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_the_reference_encoding_at_every_padding_length() {
        // The three residues are where a hand-rolled encoder goes wrong, and a
        // wrong Authorization header presents as an authentication failure
        // against a perfectly good token.
        assert_eq!(basic_auth(""), "eC1hY2Nlc3MtdG9rZW46");
        assert_eq!(basic_auth("a"), "eC1hY2Nlc3MtdG9rZW46YQ==");
        assert_eq!(basic_auth("ab"), "eC1hY2Nlc3MtdG9rZW46YWI=");
        assert_eq!(basic_auth("abc"), "eC1hY2Nlc3MtdG9rZW46YWJj");
    }

    #[tokio::test]
    async fn a_revision_that_is_not_a_commit_id_is_refused_before_git_runs() {
        for revision in ["--upload-pack=touch /tmp/x", "main", ""] {
            assert!(
                Checkout::fetch("github.com", "o/r", revision, "t")
                    .await
                    .is_err(),
                "`{revision}` must not reach git"
            );
        }
    }

    #[tokio::test]
    async fn a_repository_name_that_is_not_owner_slash_name_is_refused() {
        let sha = "0".repeat(40);
        for repo in ["--exec=x", "notaslash", "o/../r"] {
            assert!(
                Checkout::fetch("github.com", repo, &sha, "t")
                    .await
                    .is_err(),
                "`{repo}` must not reach git"
            );
        }
    }

    #[test]
    fn the_only_program_this_module_spawns_is_git() {
        // The security boundary says contributor code is never executed. This
        // module is the crate's single exception to "spawns nothing", so the
        // exception is pinned: adding a second program has to consciously
        // delete a test that says not to.
        let source = std::fs::read_to_string(file!()).expect("reads its own source");
        let body = source
            .split("#[cfg(test)]")
            .next()
            .expect("source before the tests");
        let spawned: Vec<&str> = body
            .match_indices("Command::new(")
            .map(|(at, _)| {
                body[at..]
                    .split_once('(')
                    .and_then(|(_, rest)| rest.split_once(')'))
                    .map(|(argument, _)| argument.trim())
                    .unwrap_or("<unparsed>")
            })
            .collect();
        assert_eq!(spawned, vec!["\"git\""], "{spawned:?}");
    }
}
