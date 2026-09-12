//! Turning a repository, and what happened on its pull requests, into memory.
//!
//! Always compiled: everything here is a pure function from source, files and
//! forge types to [`MemoryItem`]s, plus one walker that runs against the
//! [`Memory`] port. So the whole ingest is tested offline against
//! [`MockMemory`](crate::memory::MockMemory).
//!
//! Three sources, three shapes:
//!
//! - **Code** is chunked exactly as the index chunks it, by symbol where a
//!   grammar allows it, so a recollection and a retrieved chunk name the same
//!   span and the reviewer is never shown two versions of one function.
//! - **Conventions** are the repository's own instruction files and guides,
//!   split one item per heading. A heading is the unit a maintainer wrote in
//!   and the unit a question is answered from: "which rule covers this?" wants
//!   the paragraph under *Security Boundary*, not the whole of `AGENTS.md`.
//! - **Review outcomes** are what the maintainers did with the reviewer's own
//!   findings, read back off the pull request's review threads. This is the
//!   half that makes reviews improve rather than merely repeat: a finding a
//!   human pushed back on is remembered *as pushed back on*, with their words.
//!
//! What is deliberately not here: the pull request's branch. Conventions are
//! read from a checkout of the default branch at ingest time, which is the
//! policy the repository committed to, not the one a pull request proposes.
//! They still reach the prompt as fenced data, because a merged file is still
//! prose somebody wrote.

use std::fmt::Write as _;
use std::path::Path;

use globset::{Glob, GlobSet, GlobSetBuilder};

use crate::chunk::{Chunker, Selector};
use crate::config::types::Memory as MemoryConfig;
use crate::error::{Error, Result};
use crate::findings::prior::{fingerprint_in, is_own_login, title_in};
use crate::findings::types::Finding;
use crate::forge::types::{ReviewComment, ReviewThread};
use crate::index::types::Chunk;
use crate::memory::types::{MemoryItem, MemoryKind, MemoryScope, RememberReport};
use crate::ports::memory::Memory;

/// How many items go to the engine in one `remember` call.
///
/// Bounds one request, not the run: an engine that waits for indexing before
/// it answers takes seconds per batch, and a batch the size of a monorepo
/// would hold one connection open for the whole of it.
pub const REMEMBER_BATCH: usize = 32;

/// How much of a human's reply is remembered with an outcome.
///
/// Enough to carry "this is intentional, the caller already checks it" and
/// not enough to carry an essay. It is quoted back into a prompt, so it is
/// also a bound on how much attacker-authored text an outcome can smuggle.
pub const MAX_REPLY_CHARS: usize = 400;

/// Build one memory item per code chunk.
///
/// The key is the chunk's span within its file, by symbol when there is one:
/// re-chunking an unchanged file offers the same keys with the same bodies
/// and the engine replays them all, so a re-ingest of an unchanged tree costs
/// the network and nothing else.
pub fn code_items(chunks: &[Chunk]) -> Vec<MemoryItem> {
    chunks
        .iter()
        .filter(|chunk| !chunk.text.trim().is_empty())
        .map(|chunk| {
            let span = match &chunk.symbol {
                Some(symbol) => symbol.clone(),
                None => format!("{}-{}", chunk.start_line, chunk.end_line),
            };
            let title = match &chunk.symbol {
                Some(symbol) => format!("{} — {symbol}", chunk.path),
                None => format!("{}:{}-{}", chunk.path, chunk.start_line, chunk.end_line),
            };
            let mut item = MemoryItem::new(
                format!("code:{}#{span}", chunk.path),
                MemoryKind::CodeChunk,
                title,
                chunk.text.clone(),
            )
            .at_path(chunk.path.clone());
            if let Some(symbol) = &chunk.symbol {
                item = item.at_symbol(symbol.clone());
            }
            if let Some(lang) = &chunk.lang {
                item = item.labelled(format!("lang:{lang}"));
            }
            item
        })
        .collect()
}

/// Split one markdown file into one item per heading.
///
/// The title is the heading path — `AGENTS.md › Security Boundary` — so a
/// recollection reads as a pointer into the file rather than as a loose
/// paragraph. Text before the first heading is filed under the file name.
/// A section longer than `max_chars` is split at paragraph boundaries, never
/// mid-sentence, and each part carries the same heading.
pub fn convention_items(path: &str, content: &str, max_chars: usize) -> Vec<MemoryItem> {
    let mut items = Vec::new();
    let mut stack: Vec<(usize, String)> = Vec::new();
    let mut body = String::new();
    let mut in_fence = false;

    let flush = |stack: &[(usize, String)], body: &mut String, items: &mut Vec<MemoryItem>| {
        let text = body.trim();
        if !text.is_empty() {
            let heading = stack
                .iter()
                .map(|(_, h)| h.as_str())
                .collect::<Vec<_>>()
                .join(" › ");
            let title = if heading.is_empty() {
                path.to_string()
            } else {
                format!("{path} › {heading}")
            };
            let slug = slug(&heading);
            for (index, part) in split_paragraphs(text, max_chars).into_iter().enumerate() {
                let key = if index == 0 {
                    format!("convention:{path}#{slug}")
                } else {
                    format!("convention:{path}#{slug}~{index}")
                };
                items.push(
                    MemoryItem::new(key, MemoryKind::Convention, title.clone(), part)
                        .at_path(path.to_string())
                        .labelled("source:instruction-file"),
                );
            }
        }
        body.clear();
    };

    for line in content.lines() {
        // A `#` inside a code fence is a comment, not a heading.
        if line.trim_start().starts_with("```") || line.trim_start().starts_with("~~~") {
            in_fence = !in_fence;
        }
        let heading = (!in_fence).then(|| heading_of(line)).flatten();
        match heading {
            Some((level, text)) => {
                flush(&stack, &mut body, &mut items);
                while stack.last().is_some_and(|(l, _)| *l >= level) {
                    stack.pop();
                }
                stack.push((level, text));
            }
            None => {
                body.push_str(line);
                body.push('\n');
            }
        }
    }
    flush(&stack, &mut body, &mut items);
    items
}

/// `(level, text)` when `line` is an ATX heading.
fn heading_of(line: &str) -> Option<(usize, String)> {
    let trimmed = line.trim_start();
    let level = trimmed.chars().take_while(|c| *c == '#').count();
    if level == 0 || level > 6 {
        return None;
    }
    let rest = &trimmed[level..];
    if !rest.starts_with(' ') && !rest.is_empty() {
        return None;
    }
    let text = rest.trim().trim_end_matches('#').trim();
    (!text.is_empty()).then(|| (level, text.to_string()))
}

/// Split `text` into parts of at most `max_chars`, at blank lines.
///
/// A single paragraph longer than the ceiling is kept whole rather than cut:
/// a truncated rule is a rule that says the opposite of what it said.
fn split_paragraphs(text: &str, max_chars: usize) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    for paragraph in text.split("\n\n") {
        let paragraph = paragraph.trim();
        if paragraph.is_empty() {
            continue;
        }
        if !current.is_empty() && current.len() + paragraph.len() + 2 > max_chars {
            parts.push(std::mem::take(&mut current));
        }
        if !current.is_empty() {
            current.push_str("\n\n");
        }
        current.push_str(paragraph);
    }
    if !current.is_empty() {
        parts.push(current);
    }
    parts
}

/// A lowercase, hyphenated form of `text` for keys.
fn slug(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut last_dash = true;
    for c in text.chars() {
        if c.is_alphanumeric() {
            out.extend(c.to_lowercase());
            last_dash = false;
        } else if !last_dash {
            out.push('-');
            last_dash = true;
        }
    }
    let out = out.trim_end_matches('-').to_string();
    if out.is_empty() { "top".into() } else { out }
}

/// One item per finding the reviewer is about to publish.
///
/// Remembered on the *read* side, before anything is posted, so the memory
/// reflects what the reviewer concluded even when the apply step later
/// decides not to post it. The outcome, when there is one, comes separately
/// from [`outcome_items`].
pub fn finding_items(repo: &str, number: u64, findings: &[Finding]) -> Vec<MemoryItem> {
    findings
        .iter()
        .map(|finding| {
            let mut body = String::new();
            let _ = writeln!(body, "Pull request {repo}#{number}");
            let _ = writeln!(
                body,
                "Lane: {}. Severity: {}. Rule: {}.",
                finding.lane.as_str(),
                finding.severity.as_str(),
                finding.rule
            );
            let _ = writeln!(
                body,
                "Location: {}{}",
                finding.path,
                finding
                    .line
                    .map(|l| format!(":{l}"))
                    .unwrap_or_default()
            );
            let _ = write!(body, "\n{}", finding.body.trim());
            MemoryItem::new(
                format!(
                    "finding:{repo}#{number}:{}",
                    finding.fingerprint(&finding.title)
                ),
                MemoryKind::ReviewFinding,
                finding.title.clone(),
                body,
            )
            .at_path(finding.path.clone())
            .labelled(format!("lane:{}", finding.lane.as_str()))
            .labelled(format!("severity:{}", finding.severity.as_str()))
            .labelled(format!("pr:{number}"))
        })
        .collect()
}

/// What became of a finding, as far as the thread it opened can tell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Resolved, and the code it pointed at changed.
    Fixed,
    /// Resolved without the code changing, after a human replied — the
    /// "this is fine" case, and the one worth the most to remember.
    Rejected,
    /// Resolved without the code changing and without a word: dismissed.
    Dismissed,
    /// A human replied and the thread is still open.
    Disputed,
}

impl Outcome {
    /// The word used in keys, labels and the prompt.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Fixed => "fixed",
            Self::Rejected => "rejected",
            Self::Dismissed => "dismissed",
            Self::Disputed => "disputed",
        }
    }
}

/// Classify a thread the reviewer opened, or `None` when nothing has
/// happened to it yet.
///
/// Only the deterministic signals GitHub carries are used — resolved,
/// outdated, and whether a human wrote back. No model is consulted: an
/// outcome is evidence about the maintainers' judgement, and inferring it
/// with a model would remember the model's judgement instead.
pub fn classify(thread: &ReviewThread) -> Option<Outcome> {
    let human_replied = thread
        .comments
        .iter()
        .skip(1)
        .any(|c| !c.bot && !is_own_login(&c.author));
    match (thread.is_resolved, thread.is_outdated, human_replied) {
        (true, true, _) => Some(Outcome::Fixed),
        (true, false, true) => Some(Outcome::Rejected),
        (true, false, false) => Some(Outcome::Dismissed),
        (false, _, true) => Some(Outcome::Disputed),
        (false, _, false) => None,
    }
}

/// One item per settled thread the reviewer opened on a pull request.
///
/// `comments` are the flat review comments, which is where the path lives —
/// a thread knows its comments but not its file. The two are paired by the
/// fingerprint marker the reviewer wrote, so a thread somebody else opened
/// contributes nothing however it resolved.
pub fn outcome_items(
    repo: &str,
    number: u64,
    threads: &[ReviewThread],
    comments: &[ReviewComment],
) -> Vec<MemoryItem> {
    let mut items = Vec::new();
    for thread in threads {
        let Some(opener) = thread.comments.first() else {
            continue;
        };
        if !is_own_login(&opener.author) {
            continue;
        }
        let Some(fingerprint) = fingerprint_in(&opener.body) else {
            continue;
        };
        let Some(outcome) = classify(thread) else {
            continue;
        };
        let title = title_in(&opener.body).unwrap_or_else(|| "untitled finding".into());
        let path = comments
            .iter()
            .find(|c| fingerprint_in(&c.body).as_deref() == Some(fingerprint.as_str()))
            .map(|c| c.path.clone());

        let mut body = String::new();
        let _ = writeln!(body, "Pull request {repo}#{number}");
        let _ = writeln!(body, "Finding: {title}");
        if let Some(path) = &path {
            let _ = writeln!(body, "Location: {path}");
        }
        let _ = writeln!(body, "Outcome: {}", outcome.as_str());
        let reply = thread
            .comments
            .iter()
            .skip(1)
            .filter(|c| !c.bot && !is_own_login(&c.author))
            .last();
        if let Some(reply) = reply {
            let _ = write!(
                body,
                "Maintainer's reply: {}",
                crate::memory::excerpt(&reply.body, MAX_REPLY_CHARS)
            );
        }

        let mut item = MemoryItem::new(
            format!("outcome:{repo}#{number}:{fingerprint}"),
            MemoryKind::ReviewOutcome,
            format!("{} — {title}", outcome.as_str()),
            body,
        )
        .labelled(format!("outcome:{}", outcome.as_str()))
        .labelled(format!("pr:{number}"));
        if let Some(path) = path {
            item = item.at_path(path);
        }
        items.push(item);
    }
    items
}

/// What a checkout ingest wrote.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IngestReport {
    /// Files read for code.
    pub code_files: usize,
    /// Code chunks offered.
    pub code_items: usize,
    /// Files read for conventions.
    pub convention_files: usize,
    /// Convention sections offered.
    pub convention_items: usize,
    /// What the engine reported, over every batch.
    pub remembered: RememberReport,
    /// Files that could not be read, with the reason.
    pub unreadable: Vec<String>,
}

impl IngestReport {
    /// One line for a log or a CLI.
    pub fn summary(&self) -> String {
        format!(
            "{} code chunks from {} files, {} convention sections from {} files; \
             {} written, {} already remembered{}",
            self.code_items,
            self.code_files,
            self.convention_items,
            self.convention_files,
            self.remembered.written,
            self.remembered.replayed,
            if self.unreadable.is_empty() {
                String::new()
            } else {
                format!(", {} unreadable", self.unreadable.len())
            }
        )
    }
}

/// Walks a checkout and remembers it.
pub struct Ingestor<'a> {
    memory: &'a dyn Memory,
    config: &'a MemoryConfig,
    selector: Selector,
    chunker: Chunker,
    conventions: GlobSet,
}

impl<'a> Ingestor<'a> {
    /// An ingestor over `memory`, honouring `config` and `ignore` globs.
    pub fn new(memory: &'a dyn Memory, config: &'a MemoryConfig, ignore: &[String]) -> Result<Self> {
        let mut builder = GlobSetBuilder::new();
        for pattern in &config.convention_files {
            let glob = Glob::new(pattern).map_err(|err| {
                Error::config(format!("memory.convention_files `{pattern}`: {err}"))
            })?;
            builder.add(glob);
        }
        let conventions = builder
            .build()
            .map_err(|err| Error::config(format!("memory.convention_files: {err}")))?;
        Ok(Self {
            memory,
            config,
            selector: Selector::new(ignore)?,
            chunker: Chunker::new(),
            conventions,
        })
    }

    /// Whether `path` is one of the configured convention files.
    pub fn is_convention(&self, path: &str) -> bool {
        self.conventions.is_match(path)
    }

    /// Remember everything under `root` for `repo`.
    pub async fn ingest_checkout(&self, repo: &str, root: &Path) -> Result<IngestReport> {
        let selection = self.selector.walk(root)?;
        let mut report = IngestReport::default();
        let mut pending: Vec<MemoryItem> = Vec::new();

        for path in &selection.selected {
            let bytes = match std::fs::read(root.join(path)) {
                Ok(bytes) => bytes,
                Err(err) => {
                    report.unreadable.push(format!("{path}: {err}"));
                    continue;
                }
            };
            if self.config.ingest_conventions && self.is_convention(path) {
                let Ok(text) = std::str::from_utf8(&bytes) else {
                    report.unreadable.push(format!("{path}: not UTF-8"));
                    continue;
                };
                let items = convention_items(path, text, self.config.convention_section_chars);
                report.convention_files += 1;
                report.convention_items += items.len();
                pending.extend(items);
            } else if self.config.ingest_code {
                match self.chunker.chunk_bytes(repo, path, &bytes) {
                    Ok(chunks) => {
                        let items = code_items(&chunks);
                        report.code_files += 1;
                        report.code_items += items.len();
                        pending.extend(items);
                    }
                    Err(err) => report.unreadable.push(format!("{path}: {err}")),
                }
            }
            if pending.len() >= REMEMBER_BATCH {
                self.flush(repo, &mut pending, &mut report).await?;
            }
        }
        self.flush(repo, &mut pending, &mut report).await?;
        Ok(report)
    }

    /// Write `pending` in section-homogeneous batches.
    async fn flush(
        &self,
        repo: &str,
        pending: &mut Vec<MemoryItem>,
        report: &mut IngestReport,
    ) -> Result<()> {
        let items = std::mem::take(pending);
        // One `remember` per section: the port files by section scope, and a
        // batch that mixes them would have to be split anyway.
        let mut by_section: std::collections::BTreeMap<_, Vec<MemoryItem>> = Default::default();
        for item in items {
            by_section.entry(item.section()).or_default().push(item);
        }
        for (section, items) in by_section {
            let scope = MemoryScope::section(repo, section);
            for batch in items.chunks(REMEMBER_BATCH) {
                report.remembered.merge(self.memory.remember(&scope, batch).await?);
            }
        }
        Ok(())
    }
}

/// Remember `items` for `repo`, batched by section. Best-effort: the first
/// failure is returned, and everything before it stays written.
pub async fn remember_all(
    memory: &dyn Memory,
    repo: &str,
    items: &[MemoryItem],
) -> Result<RememberReport> {
    let mut report = RememberReport::default();
    let mut by_section: std::collections::BTreeMap<_, Vec<&MemoryItem>> = Default::default();
    for item in items {
        by_section.entry(item.section()).or_default().push(item);
    }
    for (section, items) in by_section {
        let scope = MemoryScope::section(repo, section);
        for batch in items.chunks(REMEMBER_BATCH) {
            let owned: Vec<MemoryItem> = batch.iter().map(|i| (*i).clone()).collect();
            report.merge(memory.remember(&scope, &owned).await?);
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::forge::types::ThreadComment;
    use crate::memory::MockMemory;
    use crate::memory::types::MemorySection;

    #[test]
    fn conventions_split_one_item_per_heading_with_the_heading_path_as_title() {
        let md = "\
Intro line.

# Repository Guidelines

## Security Boundary

The model never holds a write token.

```sh
# not a heading
cargo test
```

## Testing

Tests live in-crate.

# Other

Tail.
";
        let items = convention_items("AGENTS.md", md, 2000);
        let titles: Vec<&str> = items.iter().map(|i| i.title.as_str()).collect();
        assert_eq!(
            titles,
            [
                "AGENTS.md",
                "AGENTS.md › Repository Guidelines › Security Boundary",
                "AGENTS.md › Repository Guidelines › Testing",
                "AGENTS.md › Other",
            ]
        );
        assert!(items[1].body.contains("# not a heading"));
        assert_eq!(
            items[1].key,
            "convention:AGENTS.md#repository-guidelines-security-boundary"
        );
        assert!(items.iter().all(|i| i.kind == MemoryKind::Convention));
        assert!(items.iter().all(|i| i.path.as_deref() == Some("AGENTS.md")));
    }

    #[test]
    fn long_sections_split_at_paragraphs_and_share_a_title() {
        let para = "x".repeat(150);
        let md = format!("# H\n\n{para}\n\n{para}\n\n{para}\n");
        let items = convention_items("CLAUDE.md", &md, 320);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].title, items[1].title);
        assert_eq!(items[0].key, "convention:CLAUDE.md#h");
        assert_eq!(items[1].key, "convention:CLAUDE.md#h~1");
        assert!(items[0].body.len() <= 320);
    }

    #[test]
    fn a_paragraph_over_the_ceiling_is_kept_whole() {
        let para = "y".repeat(500);
        let items = convention_items("CLAUDE.md", &format!("# H\n\n{para}\n"), 200);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].body, para);
    }

    #[test]
    fn code_items_key_on_the_symbol_and_carry_the_path() {
        let chunks = Chunker::new().chunk("o/r", "src/a.rs", "fn alpha() {}\n\nfn beta() {}\n");
        let items = code_items(&chunks);
        assert!(!items.is_empty());
        assert!(items.iter().all(|i| i.kind == MemoryKind::CodeChunk));
        assert!(items.iter().all(|i| i.path.as_deref() == Some("src/a.rs")));
        assert!(items.iter().all(|i| i.key.starts_with("code:src/a.rs#")));
    }

    fn thread(ours: &str, replies: &[(&str, bool)], resolved: bool, outdated: bool) -> ReviewThread {
        let mut comments = vec![ThreadComment {
            author: "tinysweeper[bot]".into(),
            body: format!("**Title here**\n\nbody\n\n<!-- tinysweeper:fp={ours} -->"),
            bot: true,
        }];
        comments.extend(replies.iter().map(|(body, bot)| ThreadComment {
            author: if *bot { "other-bot[bot]" } else { "alice" }.into(),
            body: (*body).into(),
            bot: *bot,
        }));
        ReviewThread {
            id: "t".into(),
            is_resolved: resolved,
            is_outdated: outdated,
            comments,
        }
    }

    const FP: &str = "0123456789abcdef0123456789abcdef";

    #[test]
    fn outcomes_follow_the_deterministic_signals() {
        assert_eq!(classify(&thread(FP, &[], true, true)), Some(Outcome::Fixed));
        assert_eq!(
            classify(&thread(FP, &[("fine", false)], true, false)),
            Some(Outcome::Rejected)
        );
        assert_eq!(
            classify(&thread(FP, &[], true, false)),
            Some(Outcome::Dismissed)
        );
        assert_eq!(
            classify(&thread(FP, &[("no", false)], false, false)),
            Some(Outcome::Disputed)
        );
        assert_eq!(classify(&thread(FP, &[], false, false)), None);
        // A bot's reply is not a human's.
        assert_eq!(
            classify(&thread(FP, &[("beep", true)], false, false)),
            None
        );
    }

    #[test]
    fn outcome_items_pair_the_path_by_fingerprint_and_quote_the_reply() {
        let threads = vec![thread(
            FP,
            &[("This is intentional; see the caller.", false)],
            true,
            false,
        )];
        let comments = vec![ReviewComment {
            author: "tinysweeper[bot]".into(),
            body: format!("x <!-- tinysweeper:fp={FP} -->"),
            path: "src/lib.rs".into(),
            ..ReviewComment::default()
        }];
        let items = outcome_items("o/r", 7, &threads, &comments);
        assert_eq!(items.len(), 1);
        let item = &items[0];
        assert_eq!(item.kind, MemoryKind::ReviewOutcome);
        assert_eq!(item.path.as_deref(), Some("src/lib.rs"));
        assert_eq!(item.title, "rejected — Title here");
        assert!(item.body.contains("Maintainer's reply: This is intentional"));
        assert!(item.labels.contains(&"outcome:rejected".to_string()));
        assert_eq!(item.key, format!("outcome:o/r#7:{FP}"));
    }

    #[test]
    fn threads_opened_by_others_and_unsettled_threads_contribute_nothing() {
        let mut theirs = thread(FP, &[("x", false)], true, false);
        theirs.comments[0].author = "alice".into();
        theirs.comments[0].bot = false;
        let open = thread(FP, &[], false, false);
        assert!(outcome_items("o/r", 1, &[theirs, open], &[]).is_empty());
    }

    #[test]
    fn a_long_reply_is_cut_to_the_ceiling() {
        let long = "z".repeat(2000);
        let threads = vec![thread(FP, &[(long.as_str(), false)], true, false)];
        let items = outcome_items("o/r", 1, &threads, &[]);
        assert!(items[0].body.len() < 700, "{}", items[0].body.len());
    }

    #[tokio::test]
    async fn ingesting_a_checkout_files_code_and_conventions_apart() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/lib.rs"), "pub fn f() {}\n").unwrap();
        std::fs::write(
            dir.path().join("AGENTS.md"),
            "# Rules\n\n## Errors\n\nNever unwrap.\n",
        )
        .unwrap();
        let memory = MockMemory::new();
        let config: crate::config::Config = crate::config::DEFAULTS
            .parse::<toml::Table>()
            .unwrap()
            .try_into()
            .unwrap();
        let config = config.memory;
        let ingestor = Ingestor::new(&memory, &config, &[]).unwrap();
        let report = ingestor.ingest_checkout("o/r", dir.path()).await.unwrap();
        assert_eq!(report.convention_files, 1);
        assert_eq!(report.code_files, 1);
        assert!(report.remembered.written > 0);
        let code = memory.remembered(&MemoryScope::section("o/r", MemorySection::Code));
        let conventions =
            memory.remembered(&MemoryScope::section("o/r", MemorySection::Conventions));
        assert!(code.iter().all(|i| i.path.as_deref() == Some("src/lib.rs")));
        assert_eq!(conventions.len(), 1);
        assert_eq!(conventions[0].title, "AGENTS.md › Rules › Errors");

        // A second pass replays everything and writes nothing.
        let again = ingestor.ingest_checkout("o/r", dir.path()).await.unwrap();
        assert_eq!(again.remembered.written, 0);
        assert_eq!(again.remembered.replayed, report.remembered.written);
    }

    #[test]
    fn slugs_are_lowercase_hyphenated_and_never_empty() {
        assert_eq!(slug("Security Boundary"), "security-boundary");
        assert_eq!(slug("  A › B  "), "a-b");
        assert_eq!(slug("!!!"), "top");
    }
}
