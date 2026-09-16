//! A [`TreeReader`] over the forge API, for the deployment that has no
//! checkout.
//!
//! Always compiled: it is generic over the [`ForgeRead`] port, so the mock
//! serves it in tests and the GitHub adapter serves it behind `github`.
//!
//! Reads are one `file_at` call each, pinned to the reviewed commit. Search
//! is not something a contents API offers, so this reader answers
//! [`Found::Unavailable`] for it and says so — the reviewer is told up front
//! and asks for paths instead.
//!
//! # Submodules
//!
//! The pull request that motivated the whole port bumps a vendored submodule
//! and calls into it, and the definition the reviewer needs is *inside* the
//! submodule. A contents call for `vendor/lib/src/x.rs` on the superproject
//! is a 404: the superproject's tree holds a gitlink, not the files. So a
//! miss is retried through the submodule: `.gitmodules` at the reviewed
//! commit names the path and its remote, [`ForgeRead::submodule_at`] gives
//! the gitlink commit, and the file is read from that repository at that
//! commit — provided the operator listed that repository in
//! `retrieval.submodules`. Nothing else is followed: the `.gitmodules` URL is
//! contributor-controlled, and neither same host nor same owner says the
//! reviewed repository is entitled to expose the target.
//!
//! Same host is not enough: a pull request can rewrite `.gitmodules` to name
//! any repository on that host, and this reader would then use the
//! installation-wide read token to pull it in — including a private sibling
//! at a different trust level than the repository under review. Whether the
//! installation can actually read an arbitrary repository is not something
//! this port can check cheaply, so a submodule is only followed when its
//! repository shares an owner with the repository under review
//! (`sub.repo.owner == self.repo.owner`). That does not cover an installation
//! that spans multiple trust levels under one owner, but it closes the
//! same-host-any-repo escape a `.gitmodules` edit alone can reach.

use async_trait::async_trait;
use tokio::sync::OnceCell;

use crate::error::Result;
use crate::forge::types::RepoId;
use crate::ports::forge::ForgeRead;
use crate::ports::tree::{Found, Lookup, TreeReader, sensitive_path_refusal, slice_lines};

/// One submodule the superproject declares.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Submodule {
    /// Path in the superproject, without a trailing slash.
    pub path: String,
    /// The repository the remote resolves to, when it is on this forge.
    pub repo: Option<RepoId>,
}

/// Parse `.gitmodules` into paths and, where the URL is on this forge, repos.
///
/// `host` is the forge's git host, `github.com` in production; a URL on any
/// other host resolves to `None` and stays unread.
pub fn parse_gitmodules(text: &str, host: &str) -> Vec<Submodule> {
    let mut out = Vec::new();
    let mut path: Option<String> = None;
    let mut url: Option<String> = None;
    let flush = |path: &mut Option<String>, url: &mut Option<String>, out: &mut Vec<Submodule>| {
        if let Some(p) = path.take() {
            out.push(Submodule {
                repo: url.take().and_then(|u| repo_from_url(&u, host)),
                path: p,
            });
        }
        *url = None;
    };
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            flush(&mut path, &mut url, &mut out);
            continue;
        }
        if let Some(rest) = line.strip_prefix("path")
            && let Some(value) = rest.trim().strip_prefix('=')
        {
            path = Some(value.trim().trim_end_matches('/').to_string());
        } else if let Some(rest) = line.strip_prefix("url")
            && let Some(value) = rest.trim().strip_prefix('=')
        {
            url = Some(value.trim().to_string());
        }
    }
    flush(&mut path, &mut url, &mut out);
    out
}

/// `owner/name` from a clone URL on `host`, in any of git's spellings.
pub fn repo_from_url(url: &str, host: &str) -> Option<RepoId> {
    let rest = url
        .strip_prefix(&format!("https://{host}/"))
        .or_else(|| url.strip_prefix(&format!("http://{host}/")))
        .or_else(|| url.strip_prefix(&format!("git@{host}:")))
        .or_else(|| url.strip_prefix(&format!("ssh://git@{host}/")))
        .or_else(|| url.strip_prefix(&format!("git://{host}/")))?;
    let rest = rest.trim_end_matches('/').trim_end_matches(".git");
    RepoId::parse(rest)
}

/// Reads the reviewed tree through the forge.
pub struct ForgeTree<'a> {
    forge: &'a dyn ForgeRead,
    repo: RepoId,
    sha: String,
    host: String,
    submodules: OnceCell<Vec<Submodule>>,
    /// The submodule repositories the operator allows to be read.
    allowed: Vec<RepoId>,
}

/// What a read of one path came back with.
#[derive(Debug)]
enum Read {
    /// The file, from the superproject or a submodule this reader may follow.
    Content(String),
    /// No such file at this commit.
    Missing,
    /// Under a submodule policy does not let this reader open; the path
    /// names it.
    Denied(String),
}

impl<'a> ForgeTree<'a> {
    /// Read `repo` at `sha` through `forge`. No submodule is followed until
    /// [`Self::allowing`] names its repository.
    pub fn new(forge: &'a dyn ForgeRead, repo: RepoId, sha: &str, host: &str) -> Self {
        Self {
            forge,
            repo,
            sha: sha.to_string(),
            host: host.to_string(),
            submodules: OnceCell::new(),
            allowed: Vec::new(),
        }
    }

    /// Follow submodules whose remote is one of `repos` (`owner/name`).
    pub fn allowing<'s>(mut self, repos: impl IntoIterator<Item = &'s String>) -> Self {
        self.allowed = repos.into_iter().filter_map(|r| RepoId::parse(r)).collect();
        self
    }

    async fn submodules(&self) -> &[Submodule] {
        self.submodules
            .get_or_init(|| async {
                match self
                    .forge
                    .file_at(&self.repo, ".gitmodules", &self.sha)
                    .await
                {
                    Ok(Some(text)) => parse_gitmodules(&text, &self.host),
                    _ => Vec::new(),
                }
            })
            .await
    }

    /// Read `path`, following one level of submodule when the superproject
    /// has no such file.
    ///
    /// A path under a submodule this reader may not follow — one the
    /// operator did not list, or whose remote is not on this host — answers
    /// [`Read::Denied`], not [`Read::Missing`]: the file may well exist in a
    /// repository policy refused to open, and a reviewer told it is missing
    /// reports it missing.
    async fn read(&self, path: &str) -> Result<Read> {
        if let Some(content) = self.forge.file_at(&self.repo, path, &self.sha).await? {
            return Ok(Read::Content(content));
        }
        let submodules = self.submodules().await;
        let Some(sub) = submodules
            .iter()
            .filter(|s| path.starts_with(&format!("{}/", s.path)))
            .max_by_key(|s| s.path.len())
        else {
            return Ok(Read::Missing);
        };
        let Some(repo) = &sub.repo else {
            return Ok(Read::Denied(sub.path.clone()));
        };
        // A `.gitmodules` edit is contributor-controlled and can name any
        // repository on this host, and the installation-wide read token
        // would follow it — into a private sibling under the same owner as
        // readily as anywhere. Only a repository the operator listed in
        // `retrieval.submodules` is read.
        if !self.allowed.iter().any(|a| a == repo) {
            return Ok(Read::Denied(sub.path.clone()));
        }
        let Some((_url, commit)) = self
            .forge
            .submodule_at(&self.repo, &sub.path, &self.sha)
            .await?
        else {
            return Ok(Read::Missing);
        };
        let inner = &path[sub.path.len() + 1..];
        Ok(match self.forge.file_at(repo, inner, &commit).await? {
            Some(content) => Read::Content(content),
            None => Read::Missing,
        })
    }
}

#[async_trait]
impl TreeReader for ForgeTree<'_> {
    async fn lookup(&self, lookup: &Lookup) -> Result<Found> {
        match lookup {
            Lookup::Read { path, start, end } => {
                if path.contains("..") || path.starts_with('/') {
                    return Ok(Found::NotFound);
                }
                Ok(match self.read(path).await? {
                    Read::Content(content) => {
                        let (start, end) = Lookup::read_range(*start, *end);
                        slice_lines(&content, start, end)
                    }
                    Read::Missing => Found::NotFound,
                    Read::Denied(sub) => Found::Unavailable {
                        reason: format!(
                            "the submodule at `{sub}` is not one this deployment may read, \
                             so nothing under it can be read; do not treat its files as \
                             missing"
                        ),
                    },
                })
            }
            Lookup::Search { .. } => Ok(Found::Unavailable {
                reason: "this deployment reads files through the forge API and cannot search \
                         the tree; ask to read a path you can name from the diff's imports \
                         or paths instead"
                    .into(),
            }),
        }
    }

    fn describe(&self) -> String {
        "Files can be read by path at the reviewed commit, including inside vendored \
         submodules. Search is not available: name the path."
            .into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::forge::mock::{MockForge, MockState};

    fn repo() -> RepoId {
        RepoId::parse("acme/app").unwrap()
    }

    #[test]
    fn gitmodules_resolve_same_host_remotes_only() {
        let text = "[submodule \"a\"]\n\tpath = vendor/a\n\turl = git@github.com:acme/a.git\n\
                    [submodule \"b\"]\n\tpath = vendor/b\n\turl = https://gitlab.com/acme/b\n";
        let subs = parse_gitmodules(text, "github.com");
        assert_eq!(subs.len(), 2);
        assert_eq!(subs[0].repo, RepoId::parse("acme/a"));
        assert_eq!(subs[1].repo, None, "another host is never fetched");
        assert_eq!(
            repo_from_url("https://github.com/acme/x.git/", "github.com"),
            RepoId::parse("acme/x")
        );
    }

    #[tokio::test]
    async fn a_read_inside_a_submodule_follows_the_gitlink() {
        let mut state = MockState::default();
        state.set_file(
            "head",
            ".gitmodules",
            "[submodule \"lib\"]\n\tpath = vendor/lib\n\turl = https://github.com/acme/lib\n",
        );
        state.set_submodule("head", "vendor/lib", "https://github.com/acme/lib", "pin");
        state.set_file("pin", "src/x.rs", "one\ntwo\nthree\n");
        let forge = MockForge::with_state(state);
        let tree = ForgeTree::new(&forge, repo(), "head", "github.com")
            .allowing(&["acme/lib".to_string()]);

        let found = tree
            .lookup(&Lookup::Read {
                path: "vendor/lib/src/x.rs".into(),
                start: Some(2),
                end: Some(3),
            })
            .await
            .unwrap();
        assert!(
            matches!(
                found,
                Found::Text {
                    start: 2,
                    end: 3,
                    total: 3,
                    ..
                }
            ),
            "{found:?}"
        );

        let missing = tree
            .lookup(&Lookup::Read {
                path: "vendor/lib/src/nope.rs".into(),
                start: None,
                end: None,
            })
            .await
            .unwrap();
        assert_eq!(missing, Found::NotFound);

        let search = tree
            .lookup(&Lookup::Search {
                pattern: "x".into(),
                glob: None,
            })
            .await
            .unwrap();
        assert!(matches!(search, Found::Unavailable { .. }));
    }

    #[tokio::test]
    async fn a_same_host_different_owner_submodule_is_not_followed() {
        // `.gitmodules` is contributor-controlled; pointing it at a same-host
        // repository owned by someone else must not pull that repository in
        // through the installation's read token.
        let mut state = MockState::default();
        state.set_file(
            "head",
            ".gitmodules",
            "[submodule \"lib\"]\n\tpath = vendor/lib\n\turl = https://github.com/other/lib\n",
        );
        state.set_submodule("head", "vendor/lib", "https://github.com/other/lib", "pin");
        state.set_file("pin", "src/x.rs", "one\ntwo\nthree\n");
        let forge = MockForge::with_state(state);
        let tree = ForgeTree::new(&forge, repo(), "head", "github.com");

        let found = tree
            .lookup(&Lookup::Read {
                path: "vendor/lib/src/x.rs".into(),
                start: None,
                end: None,
            })
            .await
            .unwrap();
        assert!(
            matches!(&found, Found::Unavailable { reason } if reason.contains("vendor/lib")),
            "a submodule the operator did not list must not be read, whoever owns it — \
             and the refusal is not a missing file: {found:?}"
        );

        let listed = ForgeTree::new(&forge, repo(), "head", "github.com")
            .allowing(&["other/lib".to_string()]);
        let found = listed
            .lookup(&Lookup::Read {
                path: "vendor/lib/src/x.rs".into(),
                start: None,
                end: None,
            })
            .await
            .unwrap();
        assert!(
            matches!(found, Found::Text { .. }),
            "listed, so read: {found:?}"
        );
    }
}
