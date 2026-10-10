//! A [`TreeReader`] over the forge API, for the deployment that has no
//! checkout.
//!
//! Always compiled: it is generic over the [`ForgeRead`] port, so the mock
//! serves it in tests and the GitHub adapter serves it behind `github`.
//!
//! Reads are pinned to the reviewed commit and every traversed gitlink. Search
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
//! Every nested submodule must pass the same explicit repository allowlist.
//! Traversal stops at a repeated repository/commit or sixteen gitlinks so a
//! contributor-controlled manifest cannot cause an unbounded chain of reads.

use async_trait::async_trait;
use tokio::sync::OnceCell;

use crate::error::Result;
use crate::forge::types::RepoId;
use crate::ports::forge::ForgeRead;
use crate::ports::tree::{
    Found, Hit, Lookup, TreeQuery, TreeReader, exploration_paths, sensitive_path_refusal,
    slice_lines, visible_exploration_path,
};

/// Maximum number of gitlinks followed for one path or checkout branch.
pub(crate) const MAX_SUBMODULE_DEPTH: usize = 16;

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
    for line in crate::ports::tree::git_config_lines(text) {
        let line = line.trim();
        if line.starts_with('[') {
            flush(&mut path, &mut url, &mut out);
            continue;
        }
        // Git-config keys are case-insensitive: `PATH = x` is `path = x`.
        if let Some(value) = crate::ports::tree::git_config_key(line, "path") {
            // One spelling, shared with the selector; a path nobody may
            // declare is dropped here rather than carried along unresolved.
            path = crate::ports::tree::canonical_submodule_path(value);
        } else if let Some(value) = crate::ports::tree::git_config_key(line, "url") {
            // Quoted and commented the same way a path may be.
            url = Some(crate::ports::tree::git_config_value(value));
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

    /// Read `path`, checking the operator's policy at every nested gitlink.
    ///
    /// Denied paths stay distinct from missing files: a reviewer must not
    /// report a file absent when this deployment could not open its repository.
    async fn read(&self, path: &str) -> Result<Read> {
        let mut repo = self.repo.clone();
        let mut sha = self.sha.clone();
        let mut inner = path;
        let mut visited = vec![(repo.clone(), sha.clone())];
        let mut depth = 0;
        loop {
            if let Some(content) = self.forge.file_at(&repo, inner, &sha).await? {
                return Ok(Read::Content(content));
            }
            let nested;
            let submodules = if depth == 0 {
                self.submodules().await
            } else {
                nested = match self.forge.file_at(&repo, ".gitmodules", &sha).await {
                    Ok(Some(text)) => parse_gitmodules(&text, &self.host),
                    _ => Vec::new(),
                };
                &nested
            };
            let Some(sub) = submodules
                .iter()
                .filter(|s| inner.starts_with(&format!("{}/", s.path)))
                .max_by_key(|s| s.path.len())
            else {
                return Ok(Read::Missing);
            };
            let full_path = format!("{}{}", &path[..path.len() - inner.len()], sub.path);
            let Some(next_repo) = &sub.repo else {
                return Ok(Read::Denied(full_path));
            };
            // Each manifest is contributor-controlled; the installation token
            // may access siblings the reviewed repository is not entitled to read.
            if depth >= MAX_SUBMODULE_DEPTH || !self.allowed.iter().any(|a| a == next_repo) {
                return Ok(Read::Denied(full_path));
            }
            let Some((_url, commit)) = self.forge.submodule_at(&repo, &sub.path, &sha).await?
            else {
                return Ok(Read::Missing);
            };
            let next = (next_repo.clone(), commit);
            if visited.contains(&next) {
                return Ok(Read::Denied(full_path));
            }
            inner = &inner[sub.path.len() + 1..];
            repo = next.0.clone();
            sha = next.1.clone();
            visited.push(next);
            depth += 1;
        }
    }
}

#[async_trait]
impl TreeReader for ForgeTree<'_> {
    async fn explore(&self, query: &TreeQuery) -> Result<Found> {
        if !query.valid() {
            return Ok(Found::Unavailable {
                reason: "invalid repository query".into(),
            });
        }
        match query {
            TreeQuery::List { path, limit } => {
                let listing = self.forge.tree_paths(&self.repo, &self.sha).await?;
                Ok(exploration_paths(
                    listing.paths,
                    path,
                    *limit,
                    listing.truncated,
                ))
            }
            TreeQuery::Symbol { symbol, limit } => {
                let listing = self.forge.tree_paths(&self.repo, &self.sha).await?;
                let mut paths: Vec<_> = listing
                    .paths
                    .into_iter()
                    .filter(|path| visible_exploration_path(path))
                    .collect();
                paths.sort();
                // Forge-only exploration must not silently issue one HTTP read
                // for every file in a large repository. Checkout search is broader.
                let mut truncated = listing.truncated || paths.len() > 32;
                let mut hits = Vec::new();
                'files: for path in paths.into_iter().take(32) {
                    let Read::Content(content) = self.read(&path).await? else {
                        continue;
                    };
                    // Scrub the complete fetched file before selecting hits so
                    // a symbol inside PEM material cannot omit its marker context.
                    let safe = crate::evidence::redact::scrub_rendered(&content);
                    for (line, text) in safe.lines().enumerate() {
                        if text.contains(symbol) {
                            if hits.len()
                                >= (*limit as usize).min(crate::ports::tree::MAX_SEARCH_HITS)
                            {
                                truncated = true;
                                break 'files;
                            }
                            hits.push(Hit {
                                path: path.clone(),
                                line: u32::try_from(line + 1).unwrap_or(u32::MAX),
                                text: text.to_owned(),
                            });
                        }
                    }
                }
                Ok(Found::Hits {
                    hits,
                    truncated,
                    skipped: Vec::new(),
                })
            }
            TreeQuery::History {
                commit,
                path,
                start,
                end,
            } => {
                if !visible_exploration_path(path) {
                    return Ok(sensitive_path_refusal());
                }
                Ok(match self.forge.file_at(&self.repo, path, commit).await? {
                    Some(content) => slice_lines(
                        &crate::evidence::redact::scrub_rendered(&content),
                        *start,
                        *end,
                    ),
                    None => Found::NotFound,
                })
            }
        }
    }

    async fn lookup(&self, lookup: &Lookup) -> Result<Found> {
        match lookup {
            Lookup::Read { path, start, end } => {
                if path.contains("..") || path.starts_with('/') {
                    return Ok(Found::NotFound);
                }
                if crate::scan::is_sensitive_path(path) {
                    return Ok(sensitive_path_refusal());
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

        // One spelling per directory: the selector and the manifest say
        // `libs/core/...`, so `./libs/core/` is `libs/core`.
        for spelled in [
            "./libs/core/",
            "libs//core",
            "libs/./core",
            "./libs/./core//",
        ] {
            let text = format!(
                "[submodule \"c\"]\n\tpath = {spelled}\n\turl = https://github.com/acme/c\n"
            );
            assert_eq!(
                parse_gitmodules(&text, "github.com")[0].path,
                "libs/core",
                "{spelled}"
            );
        }
        let continued =
            "[submodule \"k\"]\n\tpath = libs/k\n\turl = https://github.com/\\\nacme/k.git\n";
        assert_eq!(
            parse_gitmodules(continued, "github.com")[0].repo,
            RepoId::parse("acme/k"),
            "a continuation line is joined before the value is read"
        );
        let quoted_url = "[submodule \"q\"]\n\tpath = libs/q\n\turl = \"https://github.com/acme/q.git\" # note\n";
        assert_eq!(
            parse_gitmodules(quoted_url, "github.com")[0].repo,
            RepoId::parse("acme/q"),
            "a quoted, commented url resolves like a bare one"
        );
        let shouted = "[submodule \"e\"]\n\tPATH = libs/e\n\tURL = https://github.com/acme/e\n";
        let subs = parse_gitmodules(shouted, "github.com");
        assert_eq!(
            subs[0].path, "libs/e",
            "git-config keys are case-insensitive"
        );
        assert_eq!(subs[0].repo, RepoId::parse("acme/e"));
        let escaping = "[submodule \"d\"]\n\tpath = ../up\n\turl = https://github.com/acme/d\n";
        assert!(
            parse_gitmodules(escaping, "github.com").is_empty(),
            "a path out of the tree is not a submodule"
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
    async fn nested_submodule_reads_follow_each_pin_and_enforce_each_allowlist_entry() {
        let mut state = MockState::default();
        state.set_file(
            "head",
            ".gitmodules",
            "[submodule \"outer\"]\npath = vendor/outer\nurl = https://github.com/acme/outer\n",
        );
        state.set_submodule("head", "vendor/outer", "ignored", "outer-pin");
        state.set_file(
            "outer-pin",
            ".gitmodules",
            "[submodule \"inner\"]\npath = sdk/inner\nurl = https://github.com/acme/inner\n",
        );
        state.set_submodule("outer-pin", "sdk/inner", "ignored", "inner-pin");
        state.set_file("inner-pin", "src/lib.rs", "nested pinned content");
        // Any attempt to access the denied target must fail the test rather
        // than quietly returning a missing file.
        state.set_unreadable_file("inner-pin", "src/lib.rs");
        let forge = MockForge::with_state(state.clone());
        let tree = ForgeTree::new(&forge, repo(), "head", "github.com")
            .allowing(&["acme/outer".to_string()]);
        let lookup = Lookup::Read {
            path: "vendor/outer/sdk/inner/src/lib.rs".into(),
            start: None,
            end: None,
        };
        let denied = tree.lookup(&lookup).await.unwrap();
        assert!(
            matches!(denied, Found::Unavailable { reason } if reason.contains("vendor/outer/sdk/inner"))
        );
        state.unreadable_files.clear();
        let forge = MockForge::with_state(state);
        let tree = ForgeTree::new(&forge, repo(), "head", "github.com")
            .allowing(&["acme/outer".to_string(), "acme/inner".to_string()]);
        assert!(
            matches!(tree.lookup(&lookup).await.unwrap(), Found::Text { text, .. } if text == "    1| nested pinned content")
        );
    }

    #[tokio::test]
    async fn nested_submodule_cycles_are_refused_before_reentering_a_repository() {
        let mut state = MockState::default();
        state.set_file(
            "head",
            ".gitmodules",
            "[submodule \"lib\"]\npath = lib\nurl = https://github.com/acme/lib\n",
        );
        state.set_submodule("head", "lib", "ignored", "pin");
        state.set_file(
            "pin",
            ".gitmodules",
            "[submodule \"self\"]\npath = self\nurl = https://github.com/acme/lib\n",
        );
        state.set_submodule("pin", "self", "ignored", "pin");
        state.set_unreadable_file("pin", "src/lib.rs");
        let forge = MockForge::with_state(state);
        let tree = ForgeTree::new(&forge, repo(), "head", "github.com")
            .allowing(&["acme/lib".to_string()]);
        let found = tree
            .lookup(&Lookup::Read {
                path: "lib/self/src/lib.rs".into(),
                start: None,
                end: None,
            })
            .await
            .unwrap();
        assert!(matches!(found, Found::Unavailable { .. }));
    }

    #[tokio::test]
    async fn nested_submodule_reads_stop_at_the_depth_limit() {
        let mut state = MockState::default();
        let mut allowed = Vec::new();
        for depth in 0..=MAX_SUBMODULE_DEPTH {
            let sha = if depth == 0 {
                "head".to_string()
            } else {
                format!("pin-{depth}")
            };
            let target = format!("acme/lib-{}", depth + 1);
            state.set_file(
                &sha,
                ".gitmodules",
                &format!("[submodule \"lib\"]\npath = lib\nurl = https://github.com/{target}\n"),
            );
            state.set_submodule(&sha, "lib", "ignored", &format!("pin-{}", depth + 1));
            allowed.push(target);
        }
        state.set_unreadable_file(&format!("pin-{}", MAX_SUBMODULE_DEPTH + 1), "src/lib.rs");
        let forge = MockForge::with_state(state);
        let tree = ForgeTree::new(&forge, repo(), "head", "github.com").allowing(&allowed);
        let found = tree
            .lookup(&Lookup::Read {
                path: format!("{}src/lib.rs", "lib/".repeat(MAX_SUBMODULE_DEPTH + 1)),
                start: None,
                end: None,
            })
            .await
            .unwrap();
        assert!(matches!(found, Found::Unavailable { .. }));
    }

    #[tokio::test]
    async fn forge_tree_refuses_to_read_a_dotenv_file() {
        let mut state = MockState::default();
        state.set_file("head", ".env", "AWS_SECRET=super-secret-value\n");
        let forge = MockForge::with_state(state);
        let tree = ForgeTree::new(&forge, repo(), "head", "github.com");

        let found = tree
            .lookup(&Lookup::Read {
                path: ".env".into(),
                start: None,
                end: None,
            })
            .await
            .unwrap();

        assert!(
            matches!(&found, Found::Unavailable { reason } if reason.contains("secret")),
            "a sensitive path must never be read: {found:?}"
        );
    }

    #[tokio::test]
    async fn forge_tree_search_never_returns_a_hit_inside_a_sensitive_path() {
        // This deployment cannot search the tree at all — `Lookup::Search`
        // is always `Unavailable` — so a sensitive path was never reachable
        // through it either. Pinned here so the invariant is documented next
        // to `DirTree`'s equivalent test rather than left implicit.
        let forge = MockForge::with_state(MockState::default());
        let tree = ForgeTree::new(&forge, repo(), "head", "github.com");

        let found = tree
            .lookup(&Lookup::Search {
                pattern: "needle".into(),
                glob: None,
            })
            .await
            .unwrap();

        assert!(matches!(found, Found::Unavailable { .. }), "{found:?}");
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
    #[tokio::test]
    async fn exploration_reads_snapshot_symbols_and_immutable_history_without_sensitive_paths() {
        let old = "a".repeat(40);
        let secret = format!("AKIA{}", "IOSFODNN7EXAMPLE");
        let mut state = MockState::default();
        state.set_tree("head", &["src/a.rs", ".env"]);
        state.set_file(
            "head",
            "src/a.rs",
            &format!("pub fn cursor() {{}}\nconst KEY: &str = \"{secret}\";"),
        );
        state.set_file("head", ".env", "cursor PASSWORD=hidden");
        state.set_file(&old, "src/a.rs", &format!("old cursor {secret}"));
        let forge = MockForge::with_state(state);
        let tree = ForgeTree::new(&forge, repo(), "head", "github.com");
        let list = tree
            .explore(&TreeQuery::List {
                path: ".".into(),
                limit: 10,
            })
            .await
            .unwrap();
        assert!(
            matches!(list, Found::Hits { hits, .. } if hits.len() == 1 && hits[0].path == "src/a.rs")
        );
        let symbols = tree
            .explore(&TreeQuery::Symbol {
                symbol: "cursor".into(),
                limit: 10,
            })
            .await
            .unwrap();
        assert!(
            matches!(symbols, Found::Hits { hits, .. } if hits.len() == 1 && hits[0].text == "pub fn cursor() {}")
        );
        let history = tree
            .explore(&TreeQuery::History {
                commit: old,
                path: "src/a.rs".into(),
                start: 1,
                end: 1,
            })
            .await
            .unwrap();
        assert!(matches!(&history, Found::Text { text, .. } if text.contains("old cursor")));
        assert!(!format!("{history:?}").contains(&secret));
        let invalid = tree
            .explore(&TreeQuery::History {
                commit: "main".into(),
                path: "src/a.rs".into(),
                start: 1,
                end: 1,
            })
            .await
            .unwrap();
        assert!(matches!(invalid, Found::Unavailable { .. }));
    }
}
