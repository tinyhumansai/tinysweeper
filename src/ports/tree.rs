//! The tree-reading port: what a reviewer may look up in the repository.
//!
//! Always compiled. This is the seam that turns a one-shot reviewer into one
//! that can check its assumptions. A lane used to be told "everything you can
//! see is in this prompt", and the instruction was honest: the diff of one
//! file was all it had. It was also the reason a reviewer that had the right
//! doubt — *does the cursor this bound is passed to treat it as exclusive?* —
//! had nowhere to take it, and filed nothing.
//!
//! The port is deliberately narrow. Two operations, both reads:
//!
//! - [`Lookup::Read`] — a range of lines from one file at the reviewed
//!   revision, and
//! - [`Lookup::Search`] — a literal pattern over the tree, answered as
//!   `path:line: text` hits.
//!
//! Nothing here runs anything. The security boundary says contributor code is
//! read and never executed, and a port whose only verbs are *read* and
//! *search* cannot be argued into building, installing or running. It also
//! holds no write credential: every implementation is built over a read
//! handle. That is why the reviewer can be given this and still not a shell.
//!
//! A backend that cannot do one of the two says so with
//! [`Found::Unavailable`] rather than an error, and the reason reaches the
//! model. A forge-only deployment reads files through the API and cannot
//! search; the reviewer is told, and asks for a path instead.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::error::Result;

/// The most lines one [`Lookup::Read`] returns.
///
/// A file read whole is a prompt the model skims; a range it asked for is one
/// it reads. The cap is generous enough for a function and its doc comment and
/// small enough that a model asking for "the file" has to say which part.
pub const MAX_READ_LINES: u32 = 200;

/// The most hits one [`Lookup::Search`] returns.
pub const MAX_SEARCH_HITS: usize = 30;

/// One thing a reviewer asked to see.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Lookup {
    /// Lines `start..=end` of `path` at the reviewed revision.
    ///
    /// Both bounds are 1-based and inclusive. An absent `start` is line 1; an
    /// absent `end` is `start + MAX_READ_LINES - 1`. A range wider than the cap
    /// is trimmed to it and the result says so.
    Read {
        /// Repository-relative path, as the diff spells it.
        path: String,
        /// First line, 1-based.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        start: Option<u32>,
        /// Last line, 1-based, inclusive.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        end: Option<u32>,
    },
    /// Every line in the tree containing `pattern`, literally.
    ///
    /// Literal rather than a regular expression: a reviewer looking for
    /// `fn read_before` should not have to escape anything, and a pattern
    /// nobody can read is a pattern nobody can audit in the cassette.
    Search {
        /// The literal text to find.
        pattern: String,
        /// A glob restricting which paths are searched, such as `src/**/*.rs`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        glob: Option<String>,
    },
}

impl Lookup {
    /// A stable, human-readable key for this lookup.
    ///
    /// The fixture records outcomes under it, and the lane dedupes repeated
    /// asks by it, so it has to be a pure function of the fields and read
    /// naturally in a JSON file.
    pub fn key(&self) -> String {
        match self {
            Lookup::Read { path, start, end } => {
                format!(
                    "read {path}:{}-{}",
                    start.map_or("1".to_string(), |s| s.to_string()),
                    end.map_or("".to_string(), |e| e.to_string())
                )
            }
            Lookup::Search { pattern, glob } => match glob {
                Some(glob) => format!("search {pattern:?} in {glob}"),
                None => format!("search {pattern:?}"),
            },
        }
    }

    /// The effective line range of a read, clamped to the cap.
    ///
    /// Returns `(start, end)`, 1-based inclusive. Shared by every backend so a
    /// range means the same thing whichever one answers.
    pub fn read_range(start: Option<u32>, end: Option<u32>) -> (u32, u32) {
        let start = start.unwrap_or(1).max(1);
        let end = end
            .unwrap_or(start + MAX_READ_LINES - 1)
            .max(start)
            .min(start + MAX_READ_LINES - 1);
        (start, end)
    }
}

/// One search hit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hit {
    /// Repository-relative path.
    pub path: String,
    /// 1-based line number.
    pub line: u32,
    /// The line's text, trimmed of its newline.
    pub text: String,
}

/// What a lookup produced.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum Found {
    /// A range of one file.
    Text {
        /// The lines, joined with newlines, each prefixed by its number.
        text: String,
        /// The range actually returned, after clamping.
        start: u32,
        /// The last line returned.
        end: u32,
        /// How many lines the file has, so the model knows what it did not see.
        total: u32,
    },
    /// Search hits, possibly truncated.
    Hits {
        /// The hits, at most [`MAX_SEARCH_HITS`].
        hits: Vec<Hit>,
        /// Whether more matched than were returned.
        truncated: bool,
        /// Declared submodule paths this search could not look inside,
        /// because they are not checked out here.
        ///
        /// `checkout = true, submodules = false` leaves every gitlink an
        /// empty directory. A read under one already answers `Unavailable`
        /// rather than a false "not found", but a search silently walked
        /// past the empty directory and returned zero hits — indistinguishable
        /// from "genuinely nothing matches anywhere in the tree", which is
        /// exactly the vendored code this field exists to flag as unsearched
        /// rather than searched-and-empty. `#[serde(default)]` keeps an
        /// older fixture without this field deserializing, and a cassette
        /// only renders differently when the list is non-empty, so replay of
        /// an existing recording is unaffected.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        skipped: Vec<String>,
    },
    /// The path does not exist at this revision.
    NotFound,
    /// This backend cannot answer this kind of lookup; the reason is for the
    /// model, so it should say what to do instead.
    Unavailable {
        /// Why, in one sentence.
        reason: String,
    },
}

/// Read-only access to the reviewed tree.
#[async_trait]
pub trait TreeReader: Send + Sync {
    /// Answer one lookup.
    ///
    /// Errors are for the backend failing — a network error, a poisoned lock.
    /// "Not there" and "cannot do that here" are outcomes, not errors, because
    /// the model has to be told them.
    async fn lookup(&self, lookup: &Lookup) -> Result<Found>;

    /// One line describing what this reader can do, for the reviewer's
    /// instructions: whether search works, and any caveat.
    fn describe(&self) -> String;
}

/// Number and join lines `start..=end` of `content`, 1-based inclusive.
///
/// The one place the rendering of a read is decided, so the model sees the
/// same shape from every backend and the fixture can replay it byte for byte.
pub fn slice_lines(content: &str, start: u32, end: u32) -> Found {
    let lines: Vec<&str> = content.lines().collect();
    let total = u32::try_from(lines.len()).unwrap_or(u32::MAX);
    if start > total {
        return Found::Text {
            text: String::new(),
            start,
            end: start,
            total,
        };
    }
    let end = end.min(total);
    let text = lines[(start - 1) as usize..end as usize]
        .iter()
        .enumerate()
        .map(|(i, line)| format!("{:>5}| {line}", start + i as u32))
        .collect::<Vec<_>>()
        .join("\n");
    Found::Text {
        text,
        start,
        end,
        total,
    }
}

/// Search `content` for `pattern`, appending hits for `path`.
///
/// Returns whether the cap was reached. Shared by the in-memory and on-disk
/// readers so both produce identical hits for identical trees.
pub fn search_lines(path: &str, content: &str, pattern: &str, hits: &mut Vec<Hit>) -> bool {
    for (index, line) in content.lines().enumerate() {
        if line.contains(pattern) {
            if hits.len() >= MAX_SEARCH_HITS {
                return true;
            }
            hits.push(Hit {
                path: path.to_string(),
                line: index as u32 + 1,
                text: line.trim_end().to_string(),
            });
        }
    }
    false
}

/// Whether `path` matches `glob`, or there is no glob.
pub fn glob_matches(glob: Option<&str>, path: &str) -> bool {
    match glob {
        None => true,
        Some(glob) => globset::Glob::new(glob)
            .map(|g| g.compile_matcher().is_match(path))
            .unwrap_or(false),
    }
}

/// An in-memory tree, for tests and for fixtures.
///
/// Serves reads from `files` and searches them; and, for a fixture replay,
/// serves recorded outcomes by key first so a search over a partial tree
/// answers exactly what the live tree answered.
#[derive(Debug, Default, Clone)]
pub struct MockTree {
    files: std::collections::BTreeMap<String, String>,
    recorded: std::collections::BTreeMap<String, Found>,
    search: bool,
}

impl MockTree {
    /// A tree holding `files`, searchable.
    pub fn from_files<I, P, C>(files: I) -> Self
    where
        I: IntoIterator<Item = (P, C)>,
        P: Into<String>,
        C: Into<String>,
    {
        Self {
            files: files
                .into_iter()
                .map(|(p, c)| (p.into(), c.into()))
                .collect(),
            recorded: Default::default(),
            search: true,
        }
    }

    /// A tree that answers only from recorded outcomes.
    ///
    /// What `eval run --record` wrote, replayed: a lookup it never saw is
    /// `NotFound`, which the cassette will then miss on — loudly, which is the
    /// point.
    pub fn from_recorded(recorded: std::collections::BTreeMap<String, Found>) -> Self {
        Self {
            files: Default::default(),
            recorded,
            search: false,
        }
    }

    /// Whether anything is recorded or held.
    pub fn is_empty(&self) -> bool {
        self.files.is_empty() && self.recorded.is_empty()
    }
}

#[async_trait]
impl TreeReader for MockTree {
    async fn lookup(&self, lookup: &Lookup) -> Result<Found> {
        if let Some(found) = self.recorded.get(&lookup.key()) {
            return Ok(found.clone());
        }
        // A replay answers only what was recorded. "Not found" here would be
        // a claim about the repository the fixture never made, and a model
        // told a file does not exist reports it missing at confidence 1.0 —
        // which is what happened to a compose overlay's entrypoint script.
        if !self.search && self.files.is_empty() {
            return Ok(Found::Unavailable {
                reason: "this lookup was not recorded for the fixture; nothing can be \
                         concluded about whether the path or text exists"
                    .into(),
            });
        }
        Ok(match lookup {
            Lookup::Read { path, start, end } => match self.files.get(path) {
                Some(content) => {
                    let (start, end) = Lookup::read_range(*start, *end);
                    slice_lines(content, start, end)
                }
                None => Found::NotFound,
            },
            Lookup::Search { pattern, glob } => {
                if !self.search {
                    return Ok(Found::Unavailable {
                        reason: "search was not recorded for this lookup".into(),
                    });
                }
                let mut hits = Vec::new();
                let mut truncated = false;
                for (path, content) in &self.files {
                    if !glob_matches(glob.as_deref(), path) {
                        continue;
                    }
                    if search_lines(path, content, pattern, &mut hits) {
                        truncated = true;
                        break;
                    }
                }
                Found::Hits {
                    hits,
                    truncated,
                    skipped: Vec::new(),
                }
            }
        })
    }

    fn describe(&self) -> String {
        // Word for word what `DirTree` says: a fixture stands in for a
        // checkout on replay, and the description is in the prompt the
        // cassette was keyed on.
        DIR_DESCRIPTION.into()
    }
}

/// What a reader over a checkout can do, in the reviewer's instructions.
pub const DIR_DESCRIPTION: &str =
    "Files can be read by path and the tree searched by literal text.";

/// A reader that records what its inner reader answered.
///
/// Wraps the live reader during `eval run --record` so the outcomes can be
/// written into the fixture; the replay then serves them from a [`MockTree`]
/// and the second-turn prompt is byte-identical to the recorded one.
pub struct RecordingTree<'a> {
    inner: &'a dyn TreeReader,
    recorded: std::sync::Mutex<std::collections::BTreeMap<String, Found>>,
}

impl<'a> RecordingTree<'a> {
    /// Record everything `inner` answers.
    pub fn new(inner: &'a dyn TreeReader) -> Self {
        Self {
            inner,
            recorded: std::sync::Mutex::new(Default::default()),
        }
    }

    /// Everything recorded so far, by lookup key.
    pub fn recorded(&self) -> std::collections::BTreeMap<String, Found> {
        self.recorded
            .lock()
            .map(|r| r.clone())
            .unwrap_or_else(|p| p.into_inner().clone())
    }
}

#[async_trait]
impl TreeReader for RecordingTree<'_> {
    async fn lookup(&self, lookup: &Lookup) -> Result<Found> {
        let found = self.inner.lookup(lookup).await?;
        if let Ok(mut recorded) = self.recorded.lock() {
            recorded.insert(lookup.key(), found.clone());
        }
        Ok(found)
    }

    fn describe(&self) -> String {
        self.inner.describe()
    }
}

/// A tree on disk: a checkout, or the working directory `local-review` runs in.
///
/// Reads go through `std::fs`; search walks the tree in-process. Nothing is
/// spawned. The walk skips what the indexer skips — `.git`, build output,
/// dependency directories — except that a vendored directory the repository
/// itself tracks as a submodule is *not* skipped: for a repository whose core
/// library is a vendored crate, that is where the definitions the diff calls
/// into live.
pub struct DirTree {
    root: std::path::PathBuf,
    submodules: Vec<String>,
}

impl DirTree {
    /// A reader over `root`.
    pub fn new(root: impl Into<std::path::PathBuf>) -> Self {
        let root = root.into();
        let submodules = std::fs::read_to_string(root.join(".gitmodules"))
            .map(|text| submodule_paths(&text))
            .unwrap_or_default();
        Self { root, submodules }
    }

    fn skipped(&self, rel: &str) -> bool {
        let first = rel.split('/').next().unwrap_or("");
        let vendored = matches!(
            first,
            ".git"
                | "node_modules"
                | "target"
                | "vendor"
                | ".venv"
                | "dist"
                | "build"
                | "third_party"
        );
        // A vendored directory is kept when a tracked submodule is it, is
        // under it, or contains it — otherwise the walk never reaches the
        // submodule to search it.
        vendored
            && !self.submodules.iter().any(|s| {
                s == rel || s.starts_with(&format!("{rel}/")) || rel.starts_with(&format!("{s}/"))
            })
    }

    /// The submodule `path` lies under, when that submodule has no content.
    fn unfetched_submodule(&self, path: &str) -> Option<&str> {
        let sub = self
            .submodules
            .iter()
            .find(|s| path.starts_with(&format!("{s}/")))?;
        self.dir_is_empty(sub).then_some(sub.as_str())
    }

    /// Whether the directory a declared submodule path names is empty —
    /// checked out with `[lookup].checkout = true` but never fetched, since
    /// `[retrieval].submodules = false` or the fetch itself failed.
    fn dir_is_empty(&self, sub: &str) -> bool {
        std::fs::read_dir(self.root.join(sub))
            .map(|mut entries| entries.next().is_none())
            .unwrap_or(true)
    }

    /// Declared submodule paths that matched `glob` but have no content, so a
    /// search of them answered zero hits rather than searching them.
    fn unfetched_submodules_matching(&self, glob: Option<&str>) -> Vec<String> {
        self.submodules
            .iter()
            .filter(|s| glob_matches(glob, s) && self.dir_is_empty(s))
            .cloned()
            .collect()
    }

    /// Whether `path`, joined onto `root` and resolved, still lies inside
    /// `root`.
    ///
    /// `safe_relative` rejects a lexical `..` escape, but a tracked symlink
    /// such as `leak -> /proc/self/environ` never contains `..` and still
    /// leaves the checkout once the filesystem follows it. Canonicalizing
    /// both sides and requiring the prefix catches that. A path that does not
    /// exist yet — including one under an unfetched submodule, which is an
    /// empty directory — cannot be canonicalized either way; that is not an
    /// escape, so it is let through to the normal "not found" or "submodule
    /// unavailable" handling below.
    fn within_root(&self, path: &str) -> bool {
        let Ok(root) = self.root.canonicalize() else {
            return false;
        };
        match self.root.join(path).canonicalize() {
            Ok(resolved) => resolved.starts_with(&root),
            Err(_) => true,
        }
    }

    fn walk(&self, dir: &std::path::Path, out: &mut Vec<String>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(rel) = path.strip_prefix(&self.root) else {
                continue;
            };
            let rel = rel.to_string_lossy().replace('\\', "/");
            if self.skipped(&rel) {
                continue;
            }
            // `symlink_metadata` does not follow the link, unlike `is_dir()`
            // below it used to call transitively through `path.is_dir()`. A
            // tracked symlink to an ancestor directory would otherwise recurse
            // forever, and one to a file outside the checkout would be walked
            // and searched as if it were tree content.
            let Ok(meta) = std::fs::symlink_metadata(&path) else {
                continue;
            };
            if meta.file_type().is_symlink() {
                continue;
            }
            if meta.is_dir() {
                self.walk(&path, out);
            } else {
                out.push(rel);
            }
        }
    }
}

/// The `path = ` entries of a `.gitmodules` file.
pub fn submodule_paths(gitmodules: &str) -> Vec<String> {
    gitmodules
        .lines()
        .filter_map(|line| line.trim().strip_prefix("path"))
        .filter_map(|rest| rest.trim().strip_prefix('='))
        .map(|p| p.trim().trim_end_matches('/').to_string())
        .filter(|p| !p.is_empty())
        .collect()
}

/// Reject a path that could leave the tree.
fn safe_relative(path: &str) -> bool {
    !path.is_empty()
        && !path.starts_with('/')
        && !path.contains("..")
        && !path.contains('\\')
        && !path.starts_with(".git/")
}

#[async_trait]
impl TreeReader for DirTree {
    async fn lookup(&self, lookup: &Lookup) -> Result<Found> {
        Ok(match lookup {
            Lookup::Read { path, start, end } => {
                if !safe_relative(path) || !self.within_root(path) {
                    return Ok(Found::NotFound);
                }
                match std::fs::read_to_string(self.root.join(path)) {
                    Ok(content) => {
                        let (start, end) = Lookup::read_range(*start, *end);
                        slice_lines(&content, start, end)
                    }
                    // Inside a submodule that was never fetched, "not found"
                    // would be a lie the reviewer acts on: it reported a
                    // manifest as missing because the checkout had an empty
                    // directory where the submodule belongs.
                    Err(_) => match self.unfetched_submodule(path) {
                        Some(sub) => Found::Unavailable {
                            reason: format!(
                                "the submodule at `{sub}` is not checked out here, so nothing \
                                 under it can be read; do not treat its files as missing"
                            ),
                        },
                        None => Found::NotFound,
                    },
                }
            }
            Lookup::Search { pattern, glob } => {
                let mut paths = Vec::new();
                self.walk(&self.root, &mut paths);
                paths.sort();
                let mut hits = Vec::new();
                let mut truncated = false;
                for rel in paths {
                    if !glob_matches(glob.as_deref(), &rel) {
                        continue;
                    }
                    let Ok(content) = std::fs::read_to_string(self.root.join(&rel)) else {
                        continue;
                    };
                    if search_lines(&rel, &content, pattern, &mut hits) {
                        truncated = true;
                        break;
                    }
                }
                let skipped = self.unfetched_submodules_matching(glob.as_deref());
                Found::Hits {
                    hits,
                    truncated,
                    skipped,
                }
            }
        })
    }

    fn describe(&self) -> String {
        DIR_DESCRIPTION.into()
    }
}

/// Try readers in order; the first that does not answer `NotFound` or
/// `Unavailable` wins.
///
/// A checkout that has no submodule content chained before a forge reader
/// that can resolve one, for instance.
pub struct ChainTree<'a> {
    readers: Vec<&'a dyn TreeReader>,
}

impl<'a> ChainTree<'a> {
    /// Chain `readers` in order.
    pub fn new(readers: Vec<&'a dyn TreeReader>) -> Self {
        Self { readers }
    }
}

#[async_trait]
impl TreeReader for ChainTree<'_> {
    async fn lookup(&self, lookup: &Lookup) -> Result<Found> {
        // "Unavailable" outranks "not found" when nobody answered: one reader
        // saying the truth is unknown is not undone by a later one that could
        // not see the path either. A fixture that recorded nothing, chained
        // before a forge that holds two files, was answering "no such file"
        // for the whole repository.
        let mut last = Found::NotFound;
        for reader in &self.readers {
            let found = reader.lookup(lookup).await?;
            match found {
                Found::NotFound => {}
                Found::Unavailable { .. } => last = found,
                answered => return Ok(answered),
            }
        }
        Ok(last)
    }

    fn describe(&self) -> String {
        self.readers
            .first()
            .map(|r| r.describe())
            .unwrap_or_else(|| "No repository access.".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_read_range_is_clamped_to_the_cap_and_never_inverted() {
        assert_eq!(Lookup::read_range(None, None), (1, MAX_READ_LINES));
        assert_eq!(Lookup::read_range(Some(10), Some(5)), (10, 10));
        assert_eq!(
            Lookup::read_range(Some(10), Some(10_000)),
            (10, 10 + MAX_READ_LINES - 1)
        );
        assert_eq!(Lookup::read_range(Some(0), Some(3)), (1, 3));
    }

    #[test]
    fn lines_are_numbered_and_the_total_is_reported() {
        let found = slice_lines("a\nb\nc\nd", 2, 3);
        assert_eq!(
            found,
            Found::Text {
                text: "    2| b\n    3| c".into(),
                start: 2,
                end: 3,
                total: 4
            }
        );
        let past = slice_lines("a\nb", 5, 9);
        assert!(matches!(past, Found::Text { total: 2, .. }));
    }

    #[tokio::test]
    async fn the_mock_reads_and_searches_and_prefers_recordings() {
        let tree = MockTree::from_files([
            ("src/a.rs", "fn read_before() {}\nlet x = 1;"),
            ("vendor/b.rs", "fn read_before() {}"),
        ]);

        let found = tree
            .lookup(&Lookup::Search {
                pattern: "read_before".into(),
                glob: Some("src/**".into()),
            })
            .await
            .unwrap();
        match found {
            Found::Hits { hits, truncated, .. } => {
                assert_eq!(hits.len(), 1);
                assert_eq!(hits[0].path, "src/a.rs");
                assert!(!truncated);
            }
            other => panic!("{other:?}"),
        }

        let missing = tree
            .lookup(&Lookup::Read {
                path: "nope.rs".into(),
                start: None,
                end: None,
            })
            .await
            .unwrap();
        assert_eq!(missing, Found::NotFound);

        let key = Lookup::Read {
            path: "x".into(),
            start: Some(1),
            end: Some(2),
        };
        let recorded =
            MockTree::from_recorded([(key.key(), Found::NotFound)].into_iter().collect());
        assert_eq!(recorded.lookup(&key).await.unwrap(), Found::NotFound);
        let unrecorded = recorded
            .lookup(&Lookup::Read {
                path: "never-asked.rs".into(),
                start: None,
                end: None,
            })
            .await
            .unwrap();
        assert!(
            matches!(unrecorded, Found::Unavailable { .. }),
            "an unrecorded lookup is unavailable, not a claim the path is absent"
        );
    }

    #[test]
    fn gitmodules_paths_are_parsed_and_unsafe_paths_refused() {
        let text = "[submodule \"x\"]\n\tpath = vendor/x\n\turl = https://e/x.git\n[submodule \"y\"]\n path=vendor/y/\n";
        assert_eq!(submodule_paths(text), vec!["vendor/x", "vendor/y"]);
        assert!(!safe_relative("../etc/passwd"));
        assert!(!safe_relative("/etc/passwd"));
        assert!(safe_relative("src/lib.rs"));
    }

    #[tokio::test]
    async fn a_dir_tree_reads_and_keeps_vendored_submodules_searchable() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::create_dir_all(dir.path().join("vendor/lib")).unwrap();
        std::fs::create_dir_all(dir.path().join("vendor/other")).unwrap();
        std::fs::write(
            dir.path().join(".gitmodules"),
            "[submodule \"lib\"]\n\tpath = vendor/lib\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("src/a.rs"), "needle one\n").unwrap();
        std::fs::write(dir.path().join("vendor/lib/b.rs"), "needle two\n").unwrap();
        std::fs::write(dir.path().join("vendor/other/c.rs"), "needle three\n").unwrap();

        let tree = DirTree::new(dir.path());
        let found = tree
            .lookup(&Lookup::Search {
                pattern: "needle".into(),
                glob: None,
            })
            .await
            .unwrap();
        let Found::Hits { hits, .. } = found else {
            panic!()
        };
        let paths: Vec<&str> = hits.iter().map(|h| h.path.as_str()).collect();
        assert_eq!(paths, vec!["src/a.rs", "vendor/lib/b.rs"]);

        let read = tree
            .lookup(&Lookup::Read {
                path: "vendor/lib/b.rs".into(),
                start: None,
                end: None,
            })
            .await
            .unwrap();
        assert!(matches!(read, Found::Text { total: 1, .. }));

        // A declared submodule with nothing in it answers "unavailable", so
        // a reviewer cannot conclude a file there is missing.
        std::fs::write(
            dir.path().join(".gitmodules"),
            "[submodule \"lib\"]\n\tpath = vendor/lib\n[submodule \"empty\"]\n\tpath = vendor/empty\n",
        )
        .unwrap();
        std::fs::create_dir_all(dir.path().join("vendor/empty")).unwrap();
        let tree = DirTree::new(dir.path());
        let unfetched = tree
            .lookup(&Lookup::Read {
                path: "vendor/empty/Cargo.toml".into(),
                start: None,
                end: None,
            })
            .await
            .unwrap();
        assert!(
            matches!(unfetched, Found::Unavailable { .. }),
            "{unfetched:?}"
        );

        // A search does not have the read path's per-lookup "unavailable" to
        // fall back on: it walks the empty directory and finds nothing, which
        // reads exactly like "nothing in the whole tree matches" unless the
        // unfetched submodule is named separately.
        let found = tree
            .lookup(&Lookup::Search {
                pattern: "needle".into(),
                glob: None,
            })
            .await
            .unwrap();
        let Found::Hits { skipped, .. } = found else {
            panic!("{found:?}")
        };
        assert_eq!(
            skipped,
            vec!["vendor/empty".to_string()],
            "the unfetched submodule must be named, not silently searched as empty"
        );
    }

    #[tokio::test]
    async fn a_symlink_out_of_the_checkout_is_refused_not_followed() {
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret.txt"), "s3cr3t\n").unwrap();

        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/a.rs"), "needle\n").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(outside.path().join("secret.txt"), dir.path().join("leak"))
            .unwrap();

        let tree = DirTree::new(dir.path());

        // A read through the symlink must not escape the checkout.
        #[cfg(unix)]
        {
            let read = tree
                .lookup(&Lookup::Read {
                    path: "leak".into(),
                    start: None,
                    end: None,
                })
                .await
                .unwrap();
            assert_eq!(read, Found::NotFound, "a symlink out of the root was followed");
        }

        // A search must not walk through the symlink either, so the outside
        // file's content never reaches a hit.
        let found = tree
            .lookup(&Lookup::Search {
                pattern: "s3cr3t".into(),
                glob: None,
            })
            .await
            .unwrap();
        let Found::Hits { hits, .. } = found else {
            panic!()
        };
        assert!(hits.is_empty(), "search followed a symlink out of the root");
    }

    #[tokio::test]
    async fn a_chain_falls_through_not_found_and_unavailable() {
        let empty = MockTree::from_recorded(Default::default());
        let full = MockTree::from_files([("a.rs", "x")]);
        let chain = ChainTree::new(vec![&empty, &full]);
        let found = chain
            .lookup(&Lookup::Read {
                path: "a.rs".into(),
                start: None,
                end: None,
            })
            .await
            .unwrap();
        assert!(matches!(found, Found::Text { .. }));

        let unknown = chain
            .lookup(&Lookup::Read {
                path: "b.rs".into(),
                start: None,
                end: None,
            })
            .await
            .unwrap();
        assert!(
            matches!(unknown, Found::Unavailable { .. }),
            "an unrecorded lookup stays unavailable past a reader that lacks the path: {unknown:?}"
        );
    }
}
