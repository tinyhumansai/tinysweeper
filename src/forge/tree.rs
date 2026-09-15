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
//! commit — provided the remote is on the same host and the app can read it.
//! A remote elsewhere is reported as not readable rather than fetched: the
//! `.gitmodules` URL is contributor-controlled, and the only host this reader
//! will talk to is the forge it was built over.

use async_trait::async_trait;
use tokio::sync::OnceCell;

use crate::error::Result;
use crate::forge::types::RepoId;
use crate::ports::forge::ForgeRead;
use crate::ports::tree::{Found, Lookup, TreeReader, slice_lines};

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
}

impl<'a> ForgeTree<'a> {
    /// Read `repo` at `sha` through `forge`; submodule remotes on `host` are
    /// followed.
    pub fn new(forge: &'a dyn ForgeRead, repo: RepoId, sha: &str, host: &str) -> Self {
        Self {
            forge,
            repo,
            sha: sha.to_string(),
            host: host.to_string(),
            submodules: OnceCell::new(),
        }
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
    async fn read(&self, path: &str) -> Result<Option<String>> {
        if let Some(content) = self.forge.file_at(&self.repo, path, &self.sha).await? {
            return Ok(Some(content));
        }
        let submodules = self.submodules().await;
        let Some(sub) = submodules
            .iter()
            .filter(|s| path.starts_with(&format!("{}/", s.path)))
            .max_by_key(|s| s.path.len())
        else {
            return Ok(None);
        };
        let Some(repo) = &sub.repo else {
            return Ok(None);
        };
        let Some((_url, commit)) = self
            .forge
            .submodule_at(&self.repo, &sub.path, &self.sha)
            .await?
        else {
            return Ok(None);
        };
        let inner = &path[sub.path.len() + 1..];
        self.forge.file_at(repo, inner, &commit).await
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
                    Some(content) => {
                        let (start, end) = Lookup::read_range(*start, *end);
                        slice_lines(&content, start, end)
                    }
                    None => Found::NotFound,
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
        let tree = ForgeTree::new(&forge, repo(), "head", "github.com");

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
}
