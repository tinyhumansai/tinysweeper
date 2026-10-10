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

/// The submodule directories a checkout is missing, by reason.
///
/// Kept apart because the indexer treats them differently. A *denied*
/// submodule — not on `retrieval.submodules`, or unparsable, or escaping the
/// checkout, or with no gitlink at this commit — is not at this head as far
/// as the index is concerned: its rows are revoked before anything else is
/// embedded. A *failed* one is allowed and really there, and could not be
/// fetched this time — network, auth — so its rows are kept and the run does
/// not claim the head, and the next delivery tries again.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Unfetched {
    /// Paths policy refused to fetch.
    pub denied: Vec<String>,
    /// Paths that were allowed and could not be fetched this time.
    pub failed: Vec<String>,
}

impl Unfetched {
    /// Every path that is not on disk, whatever the reason.
    pub fn all(&self) -> Vec<String> {
        self.denied
            .iter()
            .chain(self.failed.iter())
            .cloned()
            .collect()
    }

    /// Whether everything was fetched.
    pub fn is_empty(&self) -> bool {
        self.denied.is_empty() && self.failed.is_empty()
    }
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

    /// Fetch allowed submodules recursively into this checkout, shallowly.
    ///
    /// Only submodules whose remote is on `host`: the read token rides on
    /// every request as a header, and `.gitmodules` is written by whoever
    /// opened the pull request, so a remote elsewhere is never contacted.
    /// Each one is fetched exactly the way the superproject was — one commit
    /// by id, no history, no hooks — at the gitlink the superproject records,
    /// rather than through `git submodule update`, whose `--depth 1` clones
    /// the remote's default branch and fails when the pinned commit is not
    /// its tip.
    ///
    /// A submodule that is not fetched is named in the returned
    /// [`Unfetched`], under the reason: the checkout is still usable without
    /// it, but the two reasons mean different things to whoever indexes it.
    ///
    /// Only submodules whose repository is in `allowed` (`owner/name`) are
    /// fetched: `.gitmodules` is written by whoever opened the pull request,
    /// and neither same host nor same owner says the reviewed repository is
    /// entitled to pull the target with this token. The operator's list does.
    /// Every nested manifest is checked independently; repeated repository/commit
    /// pairs and paths deeper than sixteen gitlinks are denied.
    pub async fn fetch_submodules(
        &self,
        host: &str,
        token: &str,
        allowed: &[String],
    ) -> Result<Unfetched> {
        self.fetch_submodules_with(host, token, allowed, |dir, url, gitlink| async move {
            std::fs::create_dir_all(&dir).map_err(|err| {
                Error::Forge(format!("could not make a submodule directory: {err}"))
            })?;
            git(&dir, token, &["init", "--quiet"]).await?;
            git(
                &dir,
                token,
                &[
                    "fetch",
                    "--quiet",
                    "--depth",
                    "1",
                    "--no-tags",
                    &url,
                    &gitlink,
                ],
            )
            .await?;
            git(
                &dir,
                token,
                &["checkout", "--quiet", "--detach", "FETCH_HEAD"],
            )
            .await
        })
        .await
    }

    /// Walk declared gitlinks using the same policy for each repository.
    async fn fetch_submodules_with<F, Fut>(
        &self,
        host: &str,
        token: &str,
        allowed: &[String],
        mut fetch: F,
    ) -> Result<Unfetched>
    where
        F: FnMut(std::path::PathBuf, String, String) -> Fut,
        Fut: std::future::Future<Output = Result<()>>,
    {
        let root = self.dir.path();
        let allowed: Vec<crate::forge::types::RepoId> = allowed
            .iter()
            .filter_map(|r| crate::forge::types::RepoId::parse(r))
            .collect();
        let mut unfetched = Unfetched::default();
        let mut fetched: Vec<String> = Vec::new();
        // An explicit stack avoids recursive async futures and carries each
        // branch's ancestors, so shared dependencies remain valid while cycles
        // and contributor-controlled chains are bounded.
        let mut pending = vec![(root.to_path_buf(), String::new(), Vec::new())];
        while let Some((parent, prefix, ancestors)) = pending.pop() {
            let manifest = parent.join(".gitmodules");
            let Ok(metadata) = std::fs::symlink_metadata(&manifest) else {
                continue;
            };
            if !metadata.is_file() {
                if !prefix.is_empty() {
                    unfetched.denied.push(prefix);
                }
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&manifest) else {
                continue;
            };
            for sub in crate::forge::tree::parse_gitmodules(&text, host) {
                let full_path = if prefix.is_empty() {
                    sub.path.clone()
                } else {
                    format!("{prefix}/{}", sub.path)
                };
                let Some(repo) = &sub.repo else {
                    unfetched.denied.push(full_path);
                    continue;
                };
                if ancestors.len() >= crate::forge::tree::MAX_SUBMODULE_DEPTH
                    || !allowed.iter().any(|a| a == repo)
                {
                    unfetched.denied.push(full_path);
                    continue;
                }
                let dir = root.join(&full_path);
                if !safe_submodule_directory(root, &dir) {
                    unfetched.denied.push(full_path);
                    continue;
                }
                // Resolve the gitlink in its immediate parent's pinned HEAD,
                // never against the root or the remote's default branch.
                let Some(gitlink) = gitlink(&parent, token, &sub.path).await? else {
                    unfetched.denied.push(full_path);
                    continue;
                };
                let identity = (repo.clone(), gitlink.clone());
                if ancestors.contains(&identity) {
                    unfetched.denied.push(full_path);
                    continue;
                }
                // Construct the URL from the validated forge repository id;
                // contributor URLs never receive the installation credential.
                let url = format!("https://{host}/{}/{}.git", repo.owner, repo.name);
                match fetch(dir.clone(), url, gitlink).await {
                    Ok(()) => {
                        fetched.push(full_path.clone());
                        let mut ancestry = ancestors.clone();
                        ancestry.push(identity);
                        pending.push((dir, full_path, ancestry));
                    }
                    Err(err) => {
                        tracing::warn!(path = %full_path, %err, "a submodule could not be fetched; indexed without it");
                        unfetched.failed.push(full_path);
                    }
                }
            }
        }
        // A path named twice by `.gitmodules` — the contributor's file — is
        // classified once. Denial wins over everything: a second entry that
        // is allow-listed must not turn a revocation into a "keep it for
        // now". And a fetch that succeeded wins over one that failed: the
        // files are on disk, so the checkout is not missing them.
        let mut seen = std::collections::BTreeSet::new();
        unfetched.denied.retain(|path| seen.insert(path.clone()));
        let denied = unfetched.denied.clone();
        unfetched.failed.retain(|path| {
            !denied
                .iter()
                .any(|denied| path == denied || path.starts_with(&format!("{denied}/")))
                && !fetched.contains(path)
                && seen.insert(path.clone())
        });
        Ok(unfetched)
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

/// Reject existing symlink components before creating or initializing a repo.
/// A lexical containment check alone does not keep a nested checkout on disk
/// inside the root when a contributor supplied a symlink along the path.
fn safe_submodule_directory(root: &Path, dir: &Path) -> bool {
    let Ok(relative) = dir.strip_prefix(root) else {
        return false;
    };
    let mut current = root.to_path_buf();
    for component in relative.components() {
        let std::path::Component::Normal(name) = component else {
            return false;
        };
        current.push(name);
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.is_dir() => {}
            Ok(_) => return false,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return false,
        }
    }
    true
}

/// The commit the superproject's `HEAD` records for the submodule at `path`.
async fn gitlink(root: &Path, token: &str, path: &str) -> Result<Option<String>> {
    let stdout = git_stdout(root, token, &["ls-tree", "HEAD", "--", path]).await?;
    // `160000 commit <sha>\t<path>`; anything else at that path is not a
    // submodule and is left alone.
    let mut fields = stdout.split_whitespace();
    match (fields.next(), fields.next(), fields.next()) {
        (Some("160000"), Some("commit"), Some(sha))
            if sha.len() == 40 && sha.chars().all(|c| c.is_ascii_hexdigit()) =>
        {
            Ok(Some(sha.to_string()))
        }
        _ => Ok(None),
    }
}

/// Run one git command in `root`, with hooks and prompts disabled.
async fn git(root: &Path, token: &str, args: &[&str]) -> Result<()> {
    git_stdout(root, token, args).await.map(|_| ())
}

/// Run one git command in `root` and return what it printed.
async fn git_stdout(root: &Path, token: &str, args: &[&str]) -> Result<String> {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(root)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Dropping `command.output()` — GIT_TIMEOUT firing below, or the
        // caller's own deadline cancelling this future first, `review_inner`'s
        // `run.deadline` among them — must not leave `git` running unsupervised.
        // Without this, a `Future` dropped mid-fetch orphans the child: tokio
        // does not kill a spawned process on drop unless told to.
        .kill_on_drop(true)
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
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
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

    async fn fixture_repo(root: &Path, modules: &str, links: &[(&str, &str)]) -> String {
        std::fs::create_dir_all(root).unwrap();
        git(root, "", &["init", "--quiet"]).await.unwrap();
        std::fs::write(root.join(".gitmodules"), modules).unwrap();
        git(root, "", &["add", ".gitmodules"]).await.unwrap();
        for (path, sha) in links {
            git(
                root,
                "",
                &[
                    "update-index",
                    "--add",
                    "--cacheinfo",
                    &format!("160000,{sha},{path}"),
                ],
            )
            .await
            .unwrap();
        }
        git(
            root,
            "",
            &[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.com",
                "commit",
                "--quiet",
                "-m",
                "fixture",
            ],
        )
        .await
        .unwrap();
        git_stdout(root, "", &["rev-parse", "HEAD"])
            .await
            .unwrap()
            .trim()
            .to_string()
    }

    #[tokio::test]
    async fn nested_fetches_enforce_each_allowlist_entry_and_exact_gitlink() {
        let fixtures = tempfile::tempdir().unwrap();
        let inner = fixtures.path().join("inner");
        let inner_pin = fixture_repo(&inner, "", &[]).await;
        let outer = fixtures.path().join("outer");
        let outer_pin = fixture_repo(
            &outer,
            "[submodule \"inner\"]\npath = sdk/inner\nurl = https://github.com/acme/inner\n",
            &[("sdk/inner", &inner_pin)],
        )
        .await;
        for allow_inner in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let revision = fixture_repo(
                dir.path(),
                "[submodule \"outer\"]\npath = vendor/outer\nurl = https://github.com/acme/outer\n",
                &[("vendor/outer", &outer_pin)],
            )
            .await;
            let checkout = Checkout { dir, revision };
            let mut allowed = vec!["acme/outer".to_string()];
            if allow_inner {
                allowed.push("acme/inner".to_string());
            }
            let requests = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let recorded = requests.clone();
            let outer = outer.clone();
            let inner = inner.clone();
            let result = checkout
                .fetch_submodules_with("github.com", "", &allowed, move |dir, url, pin| {
                    recorded.lock().unwrap().push((url.clone(), pin.clone()));
                    let source = if url.ends_with("acme/outer.git") {
                        outer.clone()
                    } else {
                        inner.clone()
                    };
                    async move {
                        std::fs::create_dir_all(&dir).unwrap();
                        git(&dir, "", &["init", "--quiet"]).await?;
                        git(
                            &dir,
                            "",
                            &["fetch", "--quiet", source.to_str().unwrap(), &pin],
                        )
                        .await?;
                        git(&dir, "", &["checkout", "--quiet", "--detach", "FETCH_HEAD"]).await
                    }
                })
                .await
                .unwrap();
            let requests = requests.lock().unwrap();
            assert_eq!(
                requests[0],
                (
                    "https://github.com/acme/outer.git".into(),
                    outer_pin.clone()
                )
            );
            if allow_inner {
                assert!(result.is_empty(), "{result:?}");
                assert_eq!(requests.len(), 2);
                assert_eq!(
                    requests[1],
                    (
                        "https://github.com/acme/inner.git".into(),
                        inner_pin.clone()
                    )
                );
                assert!(
                    checkout
                        .path()
                        .join("vendor/outer/sdk/inner/.gitmodules")
                        .is_file()
                );
            } else {
                assert_eq!(result.denied, ["vendor/outer/sdk/inner"]);
                assert_eq!(requests.len(), 1, "a denied nested repo is never accessed");
            }
        }
    }

    #[tokio::test]
    async fn failed_nested_fetches_keep_the_full_checkout_path() {
        let fixtures = tempfile::tempdir().unwrap();
        let outer = fixtures.path().join("outer");
        let inner_pin = "1".repeat(40);
        let outer_pin = fixture_repo(
            &outer,
            "[submodule \"inner\"]\npath = sdk/inner\nurl = https://github.com/acme/inner\n",
            &[("sdk/inner", &inner_pin)],
        )
        .await;
        let dir = tempfile::tempdir().unwrap();
        let revision = fixture_repo(
            dir.path(),
            "[submodule \"outer\"]\npath = vendor/outer\nurl = https://github.com/acme/outer\n",
            &[("vendor/outer", &outer_pin)],
        )
        .await;
        let checkout = Checkout { dir, revision };
        let result = checkout
            .fetch_submodules_with(
                "github.com",
                "",
                &["acme/outer".to_string(), "acme/inner".to_string()],
                move |dir, url, pin| {
                    let source = outer.clone();
                    async move {
                        if url.ends_with("acme/inner.git") {
                            return Err(Error::Forge("offline nested fetch failure".into()));
                        }
                        std::fs::create_dir_all(&dir).unwrap();
                        git(&dir, "", &["init", "--quiet"]).await?;
                        git(
                            &dir,
                            "",
                            &["fetch", "--quiet", source.to_str().unwrap(), &pin],
                        )
                        .await?;
                        git(&dir, "", &["checkout", "--quiet", "--detach", "FETCH_HEAD"]).await
                    }
                },
            )
            .await
            .unwrap();
        assert!(result.denied.is_empty());
        assert_eq!(result.failed, ["vendor/outer/sdk/inner"]);
    }

    #[tokio::test]
    async fn an_uninitialized_checkout_without_gitmodules_needs_no_fetches() {
        let checkout = Checkout {
            dir: tempfile::tempdir().unwrap(),
            revision: "0".repeat(40),
        };
        let result = checkout
            .fetch_submodules_with("github.com", "", &[], |_, _, _| async {
                panic!("no gitlinks to fetch")
            })
            .await
            .unwrap();
        assert!(result.is_empty());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn submodule_directories_cannot_escape_through_a_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let pin = "1".repeat(40);
        let revision = fixture_repo(
            dir.path(),
            "[submodule \"lib\"]\npath = vendor/lib\nurl = https://github.com/acme/lib\n",
            &[("vendor/lib", &pin)],
        )
        .await;
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.path().join("vendor")).unwrap();
        let checkout = Checkout { dir, revision };
        let result = checkout
            .fetch_submodules_with(
                "github.com",
                "",
                &["acme/lib".to_string()],
                |_, _, _| async { panic!("symlink destination must not be fetched") },
            )
            .await
            .unwrap();
        assert_eq!(result.denied, ["vendor/lib"]);
        assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 0);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_symlink_gitmodules_file_is_never_read() {
        let checkout = Checkout {
            dir: tempfile::tempdir().unwrap(),
            revision: "0".repeat(40),
        };
        let outside = tempfile::tempdir().unwrap();
        let manifest = outside.path().join("manifest");
        std::fs::write(
            &manifest,
            "[submodule \"lib\"]\npath = vendor/lib\nurl = https://github.com/acme/lib\n",
        )
        .unwrap();
        std::os::unix::fs::symlink(&manifest, checkout.path().join(".gitmodules")).unwrap();
        // There is deliberately no initialized git repository: following the
        // manifest would invoke ls-tree and error before the fetch callback.
        assert!(
            checkout
                .fetch_submodules_with(
                    "github.com",
                    "",
                    &["acme/lib".to_string()],
                    |_, _, _| async { panic!("external manifest must not be read") }
                )
                .await
                .unwrap()
                .is_empty()
        );
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
