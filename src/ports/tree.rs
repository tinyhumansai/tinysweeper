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
        // Saturating: a model may answer any integer the schema admits, and
        // `u32::MAX` as a start must clamp, not wrap the cap below it.
        let start = start.unwrap_or(1).max(1);
        let cap = start.saturating_add(MAX_READ_LINES - 1);
        let end = end.unwrap_or(cap).max(start).min(cap);
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

    /// The commit this reader reflects, when it knows. A review compares it
    /// to the head it is reviewing and refuses a tree from another commit.
    fn revision(&self) -> Option<String> {
        None
    }
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

/// The refusal every [`TreeReader`] backend gives for a path
/// [`crate::scan::is_sensitive_path`] names.
///
/// Shared so a `.env` file or a private key reads the same whichever backend
/// answers it. [`Lookup::Read`] returns this instead of the content;
/// [`Lookup::Search`] skips the path before it is ever scanned — a redacted
/// hit would still name the path and the line, which is exactly the shape a
/// secret's location must not reach a model.
///
/// Deliberately takes no path: the whole point of the guard is that the model
/// never learns *which* sensitive file exists here, and an attacker-controlled
/// filename has no way to inject text into a refusal reason that never quotes
/// it.
pub fn sensitive_path_refusal() -> Found {
    Found::Unavailable {
        reason: "this path is treated as a secret by its shape — an `.env` file, a private \
                 key, or similar — and is never read into a review regardless of what it \
                 contains"
            .into(),
    }
}

/// Drop any search hit inside a sensitive path from an already-produced
/// [`Found`].
///
/// Used on a [`MockTree`] recorded outcome, which can predate whichever push
/// first filtered sensitive paths out of a live search. Applied
/// unconditionally on replay so an old cassette gets the same guard a fresh
/// search gives: everything but [`Found::Hits`] passes through untouched,
/// since a `Read` recorded for a sensitive path is already refused before
/// this runs.
fn strip_sensitive_hits(found: Found) -> Found {
    match found {
        Found::Hits {
            hits,
            truncated,
            skipped,
        } => Found::Hits {
            hits: hits
                .into_iter()
                .filter(|hit| !crate::scan::is_sensitive_path(&hit.path))
                .collect(),
            truncated,
            skipped,
        },
        other => other,
    }
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
        // Checked before the recorded map and before `self.files`: a
        // cassette can carry a `Lookup::Read` recorded before this guard
        // existed, or before whichever live backend produced it filtered
        // sensitive paths out. `DirTree`, `GitTree` and `ForgeTree` all
        // refuse before consulting anything; a fixture standing in for one
        // of them on replay has to refuse the same path the same way, or a
        // cassette becomes the one place the invariant does not hold.
        if let Lookup::Read { path, .. } = lookup
            && crate::scan::is_sensitive_path(path)
        {
            return Ok(sensitive_path_refusal());
        }

        if let Some(found) = self.recorded.get(&lookup.key()) {
            return Ok(strip_sensitive_hits(found.clone()));
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
                    if !glob_matches(glob.as_deref(), path) || crate::scan::is_sensitive_path(path)
                    {
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

/// Masks scanner-detected credentials out of whatever `inner` answers.
///
/// [`sensitive_path_refusal`] refuses a whole file by *name* — `.env`, a
/// private key — but an ordinary path like `src/config.rs` that merely
/// gained a credential in this diff has no such guard: a `read` or `search`
/// lookup fetches its content fresh, outside `evidence::redact::mask`
/// entirely, and would otherwise hand back exactly the value the diff view
/// already masked. This is the one place every backend's answer passes
/// through before a lane sees it — wrap the final composed tree once, in
/// `crate::app::review`, rather than teach `DirTree`, `GitTree`, `ForgeTree`
/// and `MockTree` to each redact their own content.
pub struct RedactingTree<'a> {
    inner: &'a dyn TreeReader,
}

impl<'a> RedactingTree<'a> {
    /// Redact everything `inner` answers.
    pub fn new(inner: &'a dyn TreeReader) -> Self {
        Self { inner }
    }
}

#[async_trait]
impl TreeReader for RedactingTree<'_> {
    async fn lookup(&self, lookup: &Lookup) -> Result<Found> {
        let found = self.inner.lookup(lookup).await?;
        // A requested range can begin in the body of an otherwise ordinary
        // source file's PEM block. Establish the state from the preceding
        // lines before redacting the returned range, or the body has neither
        // armour nor an assignment/rulepack shape to trigger a mask of its
        // own. The probe is deliberately bounded by the reader's normal 200
        // line cap: private-key PEM bodies are far shorter, and an unbounded
        // hidden read would let one lookup silently turn into a whole-file
        // model-adjacent operation.
        let in_key_block = match (lookup, &found) {
            (Lookup::Read { path, .. }, Found::Text { start, .. }) if *start > 1 => {
                let prefix = self
                    .inner
                    .lookup(&Lookup::Read {
                        path: path.clone(),
                        start: Some(start.saturating_sub(MAX_READ_LINES)),
                        end: Some(start - 1),
                    })
                    .await?;
                private_key_state(&prefix)
            }
            _ => false,
        };
        Ok(redact_found(found, in_key_block))
    }

    fn describe(&self) -> String {
        self.inner.describe()
    }

    fn revision(&self) -> Option<String> {
        self.inner.revision()
    }
}

/// Apply the deterministic rulepack, entropy-assignment and private-key-body
/// masking to a [`Found`] — the same path-independent passes
/// [`crate::evidence::redact::mask`] applies to a fresh diff, via
/// [`crate::scan::redact_stream_line`].
///
/// [`Found::Text`]'s lines are numbered `{n:>5}| {text}` by [`slice_lines`];
/// the anchor is split off so masking only ever touches the source text.
/// Each [`Hit`] is one line with no such prefix and no cross-line context, so
/// a private-key marker on its own is left as-is — a boundary line alone
/// names no secret, and a hit is never wide enough to carry an armour body.
fn redact_found(found: Found, mut in_key_block: bool) -> Found {
    match found {
        Found::Text {
            text,
            start,
            end,
            total,
        } => {
            let mut out = String::with_capacity(text.len());
            for (index, line) in text.split('\n').enumerate() {
                if index > 0 {
                    out.push('\n');
                }
                let (prefix, body) = match line.find("| ") {
                    Some(offset) if offset <= 6 => line.split_at(offset + 2),
                    _ => ("", line),
                };
                out.push_str(prefix);
                out.push_str(&crate::scan::redact_stream_line(body, &mut in_key_block));
            }
            Found::Text {
                text: out,
                start,
                end,
                total,
            }
        }
        Found::Hits {
            hits,
            truncated,
            skipped,
        } => Found::Hits {
            hits: hits
                .into_iter()
                .map(|hit| Hit {
                    text: crate::scan::redact_stream_line(&hit.text, &mut false),
                    ..hit
                })
                .collect(),
            truncated,
            skipped,
        },
        other => other,
    }
}

/// Whether the final line of a preceding read leaves us inside a PEM block.
///
/// The probe never reaches a model, so walking it through the same stream
/// redactor is safe and avoids duplicating the marker state machine here.
fn private_key_state(found: &Found) -> bool {
    let Found::Text { text, .. } = found else {
        return false;
    };
    let mut in_key_block = false;
    for line in text.split('\n') {
        let body = match line.find("| ") {
            Some(offset) if offset <= 6 => &line[offset + 2..],
            _ => line,
        };
        let _ = crate::scan::redact_stream_line(body, &mut in_key_block);
    }
    in_key_block
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
    /// The only paths that may be read or searched, when set.
    ///
    /// `local-review` runs in a developer's working directory, which holds
    /// files git never tracks — `.env`, a private key — and a lookup's result
    /// is sent to a remote model. The reader is handed the set git reports as
    /// tracked or untracked-and-not-ignored, and reads nothing else.
    allowed: Option<std::collections::BTreeSet<String>>,
    /// The commit this tree reflects, when the caller knows it.
    revision: Option<String>,
}

impl DirTree {
    /// A reader over `root`.
    pub fn new(root: impl Into<std::path::PathBuf>) -> Self {
        let root = root.into();
        let submodules = std::fs::read_to_string(root.join(".gitmodules"))
            .map(|text| submodule_paths(&text))
            .unwrap_or_default();
        Self {
            root,
            submodules,
            allowed: None,
            revision: None,
        }
    }

    /// Restrict reads and searches to `paths`, repository-relative.
    pub fn allowing(mut self, paths: impl IntoIterator<Item = String>) -> Self {
        self.allowed = Some(paths.into_iter().collect());
        self
    }

    /// Record the commit this tree was checked out at.
    pub fn at_revision(mut self, revision: &str) -> Self {
        self.revision = Some(revision.to_string());
        self
    }

    fn skipped(&self, rel: &str) -> bool {
        // The rule applies to the path *relative to the nearest submodule
        // root*, so a fetched submodule's own `.git`, `target` and
        // `node_modules` are skipped exactly as the superproject's are. Only
        // the directories leading to and including a submodule root are
        // exempt — otherwise the walk never reaches it — and a search that
        // read a submodule's packfiles was the alternative.
        let inner = self
            .submodules
            .iter()
            .filter_map(|s| rel.strip_prefix(&format!("{s}/")))
            .max_by_key(|inner| rel.len() - inner.len())
            .unwrap_or(rel);
        let first = inner.split('/').next().unwrap_or("");
        let skip_dir = matches!(
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
        // And only for a submodule that is one on disk — a `.git` inside it,
        // which a fetch leaves and a `.gitmodules` entry alone cannot
        // conjure — so a declared-but-ordinary `vendor/large` stays skipped.
        let leads_to_submodule = inner.len() == rel.len()
            && self.submodules.iter().any(|s| {
                (s == rel || s.starts_with(&format!("{rel}/")))
                    && self.root.join(s).join(".git").exists()
            });
        skip_dir && !leads_to_submodule
    }

    /// Whether `rel` may be read or searched at all under the allow-list.
    fn allowed(&self, rel: &str) -> bool {
        self.allowed
            .as_ref()
            .is_none_or(|allowed| allowed.contains(rel))
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
        // A fetch that got as far as `git init` and then failed leaves a
        // `.git` behind and nothing else; that is still no content.
        std::fs::read_dir(self.root.join(sub))
            .map(|entries| entries.flatten().all(|entry| entry.file_name() == ".git"))
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
        let joined = self.root.join(path);
        // A symlink is never read, wherever it points: a tracked `link ->
        // .env` is on git's own file list and resolves inside the root, and
        // would read the one file the allow-list exists to keep out.
        if std::fs::symlink_metadata(&joined).is_ok_and(|m| m.file_type().is_symlink()) {
            return false;
        }
        match joined.canonicalize() {
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
            } else if self.allowed(&rel) {
                out.push(rel);
            }
        }
    }
}

/// The `path = ` entries of a `.gitmodules` file.
pub fn submodule_paths(gitmodules: &str) -> Vec<String> {
    git_config_lines(gitmodules)
        .iter()
        .filter_map(|line| git_config_key(line.trim(), "path"))
        .filter_map(|p| canonical_submodule_path(p.trim()))
        // Two declarations of one directory are one directory.
        .fold(Vec::new(), |mut paths, path| {
            if !paths.contains(&path) {
                paths.push(path);
            }
            paths
        })
}

/// The value part of `line` when its key is `key` (case-insensitive, as
/// git-config keys are), or `None`. The key must end where the `=` or the
/// whitespace before it begins: `pathology = x` is not a `path`.
pub fn git_config_key<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let head = line.get(..key.len())?;
    if !head.eq_ignore_ascii_case(key) {
        return None;
    }
    let rest = line[key.len()..].trim_start();
    if rest.len() == line[key.len()..].len() && !rest.starts_with('=') {
        // No whitespace after the key and no `=`: a longer key.
        return None;
    }
    rest.strip_prefix('=')
}

/// A git-config value as git reads it: `"quoted"` up to the closing quote,
/// ignoring what follows; unquoted up to a `#` or `;` comment; trimmed.
///
/// Git concatenates quoted and unquoted runs — `"libs/core"suffix` is
/// `libs/coresuffix` — so this walks the value rather than splitting it:
/// inside quotes everything is literal (with `\"` and `\\` escapes); outside
/// them a `#` or `;` ends the value.
pub fn git_config_value(raw: &str) -> String {
    // Each character with whether it came from inside quotes, because only
    // the unquoted whitespace at either end is git's to drop: `" vendor/x "`
    // keeps its spaces.
    let mut out: Vec<(char, bool)> = Vec::with_capacity(raw.len());
    let mut quoted = false;
    let mut chars = raw.chars();
    while let Some(c) = chars.next() {
        match (quoted, c) {
            (_, '"') => quoted = !quoted,
            // Git's escapes: `\n`, `\t`, `\b`, and a backslash before
            // anything else (`\"`, `\\`) is that character.
            (true, '\\') => match chars.next() {
                Some('n') => out.push(('\n', true)),
                Some('t') => out.push(('\t', true)),
                Some('b') => out.push(('\u{8}', true)),
                Some(escaped) => out.push((escaped, true)),
                None => {}
            },
            (false, '#' | ';') => break,
            // Unquoted whitespace is kept in count but spelled as spaces,
            // which is how git reads it; quoted whitespace is kept as written.
            (false, c) if c.is_whitespace() => out.push((' ', false)),
            (_, c) => out.push((c, quoted)),
        }
    }
    let unquoted_space = |&(c, quoted): &(char, bool)| !quoted && c.is_whitespace();
    let start = out.iter().position(|item| !unquoted_space(item));
    let end = out.iter().rposition(|item| !unquoted_space(item));
    match (start, end) {
        (Some(start), Some(end)) => out[start..=end].iter().map(|(c, _)| c).collect(),
        _ => String::new(),
    }
}

/// A git-config file as logical lines: a physical line ending in an
/// unescaped `\` continues on the next one.
pub fn git_config_lines(text: &str) -> Vec<String> {
    let mut lines = Vec::new();
    let mut current = String::new();
    // Whether the logical line so far ends inside quotes — carried across a
    // continuation with the escapes already accounted for, rather than
    // recounted from the text, where `\"` would read as a delimiter.
    let mut quoted = false;
    for physical in text.lines() {
        // A comment ends at the newline whatever it ends with: a `\` inside
        // one continues nothing. Quotes are tracked across the logical line
        // so a `#` inside them is not a comment.
        let mut in_comment = false;
        let mut chars = physical.chars();
        while let Some(c) = chars.next() {
            match c {
                '"' if !in_comment => quoted = !quoted,
                '\\' if quoted => {
                    chars.next();
                }
                '#' | ';' if !quoted => in_comment = true,
                _ => {}
            }
        }
        let trailing_backslashes = physical.chars().rev().take_while(|c| *c == '\\').count();
        if !in_comment && trailing_backslashes % 2 == 1 {
            current.push_str(&physical[..physical.len() - 1]);
            continue;
        }
        current.push_str(physical);
        lines.push(std::mem::take(&mut current));
        quoted = false;
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines
}

/// The one spelling of a submodule path, or `None` for one nobody may declare.
///
/// Git resolves `./libs/core`, `libs//core` and `libs/./core` to the same
/// gitlink; the selector, the manifest and the fetch all say `libs/core`.
/// One canonical form for every reader, or the same directory is several
/// paths and a policy applied to one of them misses the rest. `.gitmodules`
/// is contributor-controlled: a path that leaves the tree, is absolute, or
/// names git's own directory is refused rather than repaired.
pub fn canonical_submodule_path(raw: &str) -> Option<String> {
    let raw = git_config_value(raw);
    let raw = raw.as_str();
    if raw.is_empty() || raw.starts_with('/') || raw.contains('\\') {
        return None;
    }
    let parts: Vec<&str> = raw
        .split('/')
        .filter(|part| !part.is_empty() && *part != ".")
        .collect();
    if parts.is_empty() || parts.iter().any(|part| *part == ".." || *part == ".git") {
        return None;
    }
    Some(parts.join("/"))
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
                if !safe_relative(path) || !self.within_root(path) || !self.allowed(path) {
                    return Ok(Found::NotFound);
                }
                if crate::scan::is_sensitive_path(path) {
                    return Ok(sensitive_path_refusal());
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
                    if !glob_matches(glob.as_deref(), &rel) || crate::scan::is_sensitive_path(&rel)
                    {
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

    fn revision(&self) -> Option<String> {
        self.revision.clone()
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
        assert_eq!(
            Lookup::read_range(Some(u32::MAX), None),
            (u32::MAX, u32::MAX),
            "a start at the top of the range clamps rather than wrapping"
        );
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
            Found::Hits {
                hits, truncated, ..
            } => {
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

    /// Regression for a tinysweeper finding on #166: the sensitive-path guard
    /// was added only to `DirTree`. `MockTree::from_files` stands in for a
    /// live checkout in tests and fixtures, and its `Lookup::Read` used to
    /// serve `.env` content straight from `self.files` with no equivalent
    /// refusal.
    #[tokio::test]
    async fn the_mock_refuses_to_read_a_sensitive_path_too() {
        let tree = MockTree::from_files([(".env", "AWS_SECRET=super-secret-value")]);

        let found = tree
            .lookup(&Lookup::Read {
                path: ".env".into(),
                start: None,
                end: None,
            })
            .await
            .unwrap();

        let Found::Unavailable { reason } = found else {
            panic!("a sensitive path must never be read: {found:?}")
        };
        assert!(reason.contains("secret"), "{reason}");
    }

    /// Regression for a Codex finding on #166: `is_sensitive_path` refuses a
    /// whole file by *name*, but an ordinary path like `src/config.rs` that
    /// merely gained a credential in this diff had no guard at all on a
    /// `read` lookup — the diff view masks it, but a tree read fetches the
    /// content fresh and would hand it straight back.
    #[tokio::test]
    async fn redacting_tree_masks_a_recognisable_credential_in_a_read() {
        let key = format!("{}{}", "AKIA", "IOSFODNN7EXAMPLE");
        let inner = MockTree::from_files([(
            "src/config.rs",
            format!("const KEY: &str = \"{key}\";\nfn main() {{}}\n"),
        )]);
        let tree = RedactingTree::new(&inner);

        let found = tree
            .lookup(&Lookup::Read {
                path: "src/config.rs".into(),
                start: None,
                end: None,
            })
            .await
            .unwrap();

        let Found::Text { text, .. } = found else {
            panic!("{found:?}")
        };
        assert!(!text.contains("IOSFODNN7EXAMPLE"), "{text}");
        assert!(text.contains("const KEY"), "{text}");
        assert!(
            text.contains("1|"),
            "the line-number anchor survives: {text}"
        );
    }

    /// Regression for a Codex finding on #166: `redact_stream_line` used to
    /// apply only the rulepack, so a scanner-flagged high-entropy assignment
    /// with no vendor prefix — no `AKIA`, no `ghp_` — reached a tree read
    /// fresh from the head, even though the identical value in the diff
    /// itself would have been masked by `evidence::redact::mask`'s
    /// finding-anchored fallback.
    #[tokio::test]
    async fn redacting_tree_masks_a_high_entropy_assignment_in_a_read() {
        let value = format!("{}{}", "f3Kq9zR2", "mW7pL4xN8vB1cY6tH0jD5sG");
        let inner = MockTree::from_files([(
            "src/config.rs",
            format!("let secret_token = \"{value}\";\nfn main() {{}}\n"),
        )]);
        let tree = RedactingTree::new(&inner);

        let found = tree
            .lookup(&Lookup::Read {
                path: "src/config.rs".into(),
                start: None,
                end: None,
            })
            .await
            .unwrap();

        let Found::Text { text, .. } = found else {
            panic!("{found:?}")
        };
        assert!(!text.contains(&value), "{text}");
        assert!(text.contains("let secret_token ="), "{text}");
    }

    /// Regression for the same finding, on the search path: a hit line is
    /// exactly what a model reads back verbatim, so a credential on the same
    /// line as a search match must not survive into it either.
    #[tokio::test]
    async fn redacting_tree_masks_a_recognisable_credential_in_a_search_hit() {
        let key = format!("{}{}", "AKIA", "IOSFODNN7EXAMPLE");
        let inner = MockTree::from_files([(
            "src/config.rs",
            format!("const KEY: &str = \"{key}\"; // needle\n"),
        )]);
        let tree = RedactingTree::new(&inner);

        let found = tree
            .lookup(&Lookup::Search {
                pattern: "needle".into(),
                glob: None,
            })
            .await
            .unwrap();

        let Found::Hits { hits, .. } = found else {
            panic!("{found:?}")
        };
        assert_eq!(hits.len(), 1, "{hits:#?}");
        assert!(!hits[0].text.contains("IOSFODNN7EXAMPLE"), "{hits:#?}");
        assert!(hits[0].text.contains("needle"), "{hits:#?}");
    }

    /// A private key's body carries no rulepack shape on its own lines;
    /// `RedactingTree` has to track the armour block across the numbered
    /// lines `slice_lines` produces, the same as `evidence::redact::mask`
    /// does across a diff's hunk lines.
    #[tokio::test]
    async fn redacting_tree_masks_a_private_key_body_in_a_read() {
        let begin = format!("-----BEGIN {}-----", "RSA PRIVATE KEY");
        let end = format!("-----END {}-----", "RSA PRIVATE KEY");
        let body = "MIIEowIBAAKCAQEAthisisadeadbeefexamplebodyforatestcase1234567890";
        let inner = MockTree::from_files([("src/config.rs", format!("{begin}\n{body}\n{end}\n"))]);
        let tree = RedactingTree::new(&inner);

        let found = tree
            .lookup(&Lookup::Read {
                path: "src/config.rs".into(),
                start: None,
                end: None,
            })
            .await
            .unwrap();

        let Found::Text { text, .. } = found else {
            panic!("{found:?}")
        };
        assert!(!text.contains(body), "{text}");
        assert!(text.contains(&begin), "{text}");
    }

    #[tokio::test]
    async fn redacting_tree_masks_a_range_that_starts_inside_a_private_key() {
        let begin = format!("-----BEGIN {}-----", "RSA PRIVATE KEY");
        let end = format!("-----END {}-----", "RSA PRIVATE KEY");
        // Short on purpose: this proves the preceding-range probe establishes
        // PEM state instead of relying on the body-only base64 classifier.
        let body = "short-private-key-body";
        let inner = MockTree::from_files([("src/config.rs", format!("{begin}\n{body}\n{end}\n"))]);
        let tree = RedactingTree::new(&inner);

        let found = tree
            .lookup(&Lookup::Read {
                path: "src/config.rs".into(),
                start: Some(2),
                end: Some(2),
            })
            .await
            .unwrap();

        let Found::Text { text, .. } = found else {
            panic!("{found:?}")
        };
        assert!(!text.contains(body), "{text}");
        assert!(text.contains("<redacted"), "{text}");
    }

    /// Regression for the same finding: a `MockTree` recorded outcome
    /// predates whichever push first wrapped its live backend in
    /// `RedactingTree`, so a cassette can still carry a raw `Found::Text`.
    /// The wrapper has to redact on replay too, not just a fresh read.
    #[tokio::test]
    async fn redacting_tree_masks_a_recorded_outcome_on_replay() {
        let key = format!("{}{}", "AKIA", "IOSFODNN7EXAMPLE");
        let lookup = Lookup::Read {
            path: "src/config.rs".into(),
            start: None,
            end: None,
        };
        let recorded = Found::Text {
            text: format!("    1| const KEY: &str = \"{key}\";"),
            start: 1,
            end: 1,
            total: 1,
        };
        let inner = MockTree::from_recorded([(lookup.key(), recorded)].into_iter().collect());
        let tree = RedactingTree::new(&inner);

        let found = tree.lookup(&lookup).await.unwrap();

        let Found::Text { text, .. } = found else {
            panic!("{found:?}")
        };
        assert!(!text.contains("IOSFODNN7EXAMPLE"), "{text}");
    }

    /// Regression for the same finding: a `RedactingTree` still refuses a
    /// sensitive path outright — it wraps the inner reader, and does not
    /// replace the name-based guard with a weaker content-only one.
    #[tokio::test]
    async fn redacting_tree_still_refuses_a_sensitive_path() {
        let inner = MockTree::from_files([(".env", "AWS_SECRET=super-secret-value")]);
        let tree = RedactingTree::new(&inner);

        let found = tree
            .lookup(&Lookup::Read {
                path: ".env".into(),
                start: None,
                end: None,
            })
            .await
            .unwrap();

        assert!(matches!(found, Found::Unavailable { .. }), "{found:?}");
    }

    /// Regression for the same finding: a search over `MockTree::from_files`
    /// used to walk every file including a sensitive one, so a `.env` value
    /// could come back as a hit — the path and the line, exactly the shape
    /// [`sensitive_path_refusal`]'s doc says must never reach a model.
    #[tokio::test]
    async fn the_mock_never_returns_a_search_hit_inside_a_sensitive_path() {
        let tree = MockTree::from_files([
            (".env", "AWS_SECRET=needle"),
            ("src/a.rs", "// needle, but not a secret"),
        ]);

        let found = tree
            .lookup(&Lookup::Search {
                pattern: "needle".into(),
                glob: None,
            })
            .await
            .unwrap();

        let Found::Hits { hits, .. } = found else {
            panic!("{found:?}")
        };
        assert_eq!(hits.len(), 1, "{hits:#?}");
        assert_eq!(hits[0].path, "src/a.rs");
    }

    /// Regression for the same finding, on the replay path: a cassette
    /// recorded before the guard existed can carry a `Found::Hits` naming a
    /// sensitive path, and `MockTree::from_recorded` used to hand that back
    /// verbatim since the recorded map is consulted before anything else.
    #[tokio::test]
    async fn a_recorded_search_hit_inside_a_sensitive_path_is_stripped_on_replay() {
        let key = Lookup::Search {
            pattern: "needle".into(),
            glob: None,
        };
        let recorded = Found::Hits {
            hits: vec![
                Hit {
                    path: ".env".into(),
                    line: 1,
                    text: "AWS_SECRET=needle".into(),
                },
                Hit {
                    path: "src/a.rs".into(),
                    line: 2,
                    text: "// needle".into(),
                },
            ],
            truncated: false,
            skipped: Vec::new(),
        };
        let tree = MockTree::from_recorded([(key.key(), recorded)].into_iter().collect());

        let found = tree.lookup(&key).await.unwrap();

        let Found::Hits { hits, .. } = found else {
            panic!("{found:?}")
        };
        assert_eq!(hits.len(), 1, "{hits:#?}");
        assert_eq!(hits[0].path, "src/a.rs");
    }

    #[test]
    fn gitmodules_paths_are_parsed_and_unsafe_paths_refused() {
        let text = "[submodule \"x\"]\n\tpath = vendor/x\n\turl = https://e/x.git\n[submodule \"y\"]\n Path=vendor/y/\n[submodule \"x2\"]\n\tpath = ./vendor/x\n";
        assert_eq!(
            submodule_paths(text),
            vec!["vendor/x", "vendor/y"],
            "two spellings of one directory are one entry"
        );
        // Every spelling git resolves to one gitlink is one path here too.
        for spelled in [
            "./vendor/x",
            "vendor//x",
            "vendor/./x/",
            "./vendor/./x//",
            "\"vendor/x\"",
            "\"./vendor/x/\"",
            "vendor/x # the note git ignores",
            "vendor/x ; and this one",
            "\"vendor/x\" # quoted, then a note",
            "\"vendor/\"x",
        ] {
            assert_eq!(
                canonical_submodule_path(spelled).as_deref(),
                Some("vendor/x"),
                "{spelled}"
            );
        }
        // A decoded escape is a real character in the path: a tab is a tab.
        assert_eq!(git_config_value("\"vendor\\tcore\""), "vendor\tcore");
        // Quoted whitespace is git's to keep; unquoted whitespace at the
        // ends is not, and unquoted whitespace inside is kept in count.
        assert_eq!(git_config_value("  \" vendor/x \"  "), " vendor/x ");
        assert_eq!(git_config_value("vendor/  core\t"), "vendor/  core");
        assert_eq!(git_config_key("path = x", "path"), Some(" x"));
        assert_eq!(git_config_key("PATH=x", "path"), Some("x"));
        assert_eq!(git_config_key("pathology = x", "path"), None);
        assert_eq!(
            git_config_lines("path = vendor/\\\nx\nurl = u\\\\\n"),
            vec!["path = vendor/x".to_string(), "url = u\\\\".to_string()],
            "a trailing backslash continues the line; an escaped one does not"
        );
        assert_eq!(
            git_config_lines("path = a # note\\\nurl = u\n"),
            vec!["path = a # note\\".to_string(), "url = u".to_string()],
            "a comment ends at the newline, backslash or not"
        );
        // An escaped quote is not a delimiter, across a continuation too:
        // `"libs/\"one\` + `#two\` + `bar"` is one quoted value.
        let joined = git_config_lines("path = \"libs/\\\"one\\\n#two\\\nbar\"\n");
        assert_eq!(joined, vec!["path = \"libs/\\\"one#twobar\"".to_string()]);
        assert_eq!(git_config_value(&joined[0][7..]), "libs/\"one#twobar");
        assert_eq!(git_config_value("\"vendor/x\\\"\""), "vendor/x\"");
        for refused in [
            "../x",
            "vendor/../x",
            "/vendor/x",
            ".git",
            "vendor/.git/x",
            "",
            ".",
        ] {
            assert_eq!(canonical_submodule_path(refused), None, "{refused}");
        }
        assert!(!safe_relative("../etc/passwd"));
        assert!(!safe_relative("/etc/passwd"));
        assert!(safe_relative("src/lib.rs"));
    }

    #[tokio::test]
    async fn a_dir_tree_reads_and_keeps_vendored_submodules_searchable() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::create_dir_all(dir.path().join("vendor/lib/.git")).unwrap();
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
    async fn dir_tree_refuses_to_read_a_dotenv_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".env"), "AWS_SECRET=super-secret-value\n").unwrap();

        let tree = DirTree::new(dir.path());
        let found = tree
            .lookup(&Lookup::Read {
                path: ".env".into(),
                start: None,
                end: None,
            })
            .await
            .unwrap();

        let Found::Unavailable { reason } = found else {
            panic!("a sensitive path must never be read: {found:?}")
        };
        assert!(
            reason.contains("secret"),
            "the reason should say why: {reason}"
        );
    }

    /// Regression for a tinysweeper finding on #166: the refusal used to
    /// interpolate the path it was refusing, disclosing the sensitive file's
    /// location to the model the guard exists to keep it from — and handing
    /// an attacker-controlled filename a way to inject text into a
    /// model-facing reason.
    #[tokio::test]
    async fn the_sensitive_path_refusal_never_names_the_path() {
        // A directory component distinctive enough that it could only appear
        // in the reason if the path itself were interpolated into it — the
        // doc's own generic examples ("an `.env` file") mention the shape,
        // not this specific path, so asserting against `.env` alone would
        // pass even with the old, path-naming behaviour.
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("keys-for-prod-9f3a")).unwrap();
        std::fs::write(
            dir.path().join("keys-for-prod-9f3a/.env"),
            "AWS_SECRET=super-secret-value\n",
        )
        .unwrap();
        let found = DirTree::new(dir.path())
            .lookup(&Lookup::Read {
                path: "keys-for-prod-9f3a/.env".into(),
                start: None,
                end: None,
            })
            .await
            .unwrap();

        let Found::Unavailable { reason } = found else {
            panic!("a sensitive path must never be read: {found:?}")
        };
        assert!(!reason.contains("keys-for-prod-9f3a"), "{reason}");
        assert!(reason.contains("secret"), "{reason}");
    }

    #[tokio::test]
    async fn dir_tree_search_never_returns_a_hit_inside_a_sensitive_path() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".env"), "AWS_SECRET=needle\n").unwrap();
        std::fs::write(dir.path().join("src.rs"), "let needle = 1;\n").unwrap();

        let tree = DirTree::new(dir.path());
        let found = tree
            .lookup(&Lookup::Search {
                pattern: "needle".into(),
                glob: None,
            })
            .await
            .unwrap();

        let Found::Hits { hits, .. } = found else {
            panic!("{found:?}")
        };
        let paths: Vec<&str> = hits.iter().map(|h| h.path.as_str()).collect();
        assert_eq!(
            paths,
            vec!["src.rs"],
            "a redacted hit still leaks shape and location, so the sensitive path is \
             skipped entirely rather than searched: {hits:?}"
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
            assert_eq!(
                read,
                Found::NotFound,
                "a symlink out of the root was followed"
            );
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
    async fn skip_rules_hold_inside_a_fetched_submodule_and_an_allow_list_holds_everywhere() {
        let dir = tempfile::tempdir().unwrap();
        for d in [
            "src",
            "vendor/lib/src",
            "vendor/lib/target",
            "vendor/lib/.git",
        ] {
            std::fs::create_dir_all(dir.path().join(d)).unwrap();
        }
        std::fs::write(
            dir.path().join(".gitmodules"),
            "[submodule \"lib\"]\n\tpath = vendor/lib\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("src/a.rs"), "needle\n").unwrap();
        std::fs::write(dir.path().join(".env"), "needle SECRET\n").unwrap();
        std::fs::write(dir.path().join("vendor/lib/src/b.rs"), "needle\n").unwrap();
        std::fs::write(dir.path().join("vendor/lib/target/c.rs"), "needle\n").unwrap();
        std::fs::write(dir.path().join("vendor/lib/.git/packed"), "needle\n").unwrap();

        let search = Lookup::Search {
            pattern: "needle".into(),
            glob: None,
        };
        let Found::Hits { hits, .. } = DirTree::new(dir.path()).lookup(&search).await.unwrap()
        else {
            panic!()
        };
        let paths: Vec<&str> = hits.iter().map(|h| h.path.as_str()).collect();
        assert_eq!(
            paths,
            vec!["src/a.rs", "vendor/lib/src/b.rs"],
            "the submodule's own target and .git are skipped, and so is .env — a sensitive \
             path is never searched, however the pattern matches"
        );

        let tracked = DirTree::new(dir.path())
            .allowing(["src/a.rs".to_string(), "vendor/lib/src/b.rs".to_string()]);
        let Found::Hits { hits, .. } = tracked.lookup(&search).await.unwrap() else {
            panic!()
        };
        let paths: Vec<&str> = hits.iter().map(|h| h.path.as_str()).collect();
        assert_eq!(
            paths,
            vec!["src/a.rs", "vendor/lib/src/b.rs"],
            "an ignored file is invisible"
        );
        let env = tracked
            .lookup(&Lookup::Read {
                path: ".env".into(),
                start: None,
                end: None,
            })
            .await
            .unwrap();
        assert_eq!(env, Found::NotFound);
        assert_eq!(
            DirTree::new(dir.path())
                .at_revision("abc")
                .revision()
                .as_deref(),
            Some("abc")
        );

        // A tracked symlink to the ignored file is on git's list; it is still
        // never read.
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(dir.path().join(".env"), dir.path().join("link")).unwrap();
            let linked = DirTree::new(dir.path()).allowing(["link".to_string()]);
            let via_link = linked
                .lookup(&Lookup::Read {
                    path: "link".into(),
                    start: None,
                    end: None,
                })
                .await
                .unwrap();
            assert_eq!(via_link, Found::NotFound);
        }

        // A submodule whose fetch failed after `git init` holds only `.git`,
        // and is still reported as not checked out.
        std::fs::write(
            dir.path().join(".gitmodules"),
            "[submodule \"lib\"]\n\tpath = vendor/lib\n[submodule \"half\"]\n\tpath = vendor/half\n[submodule \"bad\"]\n\tpath = .git\n",
        )
        .unwrap();
        std::fs::create_dir_all(dir.path().join("vendor/half/.git")).unwrap();
        let tree = DirTree::new(dir.path());
        assert_eq!(
            tree.submodules,
            vec!["vendor/lib", "vendor/half"],
            "`.git` is refused"
        );
        let half = tree
            .lookup(&Lookup::Read {
                path: "vendor/half/src/x.rs".into(),
                start: None,
                end: None,
            })
            .await
            .unwrap();
        assert!(matches!(half, Found::Unavailable { .. }), "{half:?}");
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
