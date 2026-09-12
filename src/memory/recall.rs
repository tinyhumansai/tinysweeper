//! Consulting memory for one review, and rendering what came back.
//!
//! Always compiled; runs against the [`Memory`] port and is tested offline
//! against [`MockMemory`](crate::memory::MockMemory).
//!
//! Two ways of asking, and the review does both:
//!
//! 1. **Recall by query.** The same bounded query `src/retrieve` composes from
//!    the pull request — title, paths, hunk headings, identifiers — is put to
//!    each section of the repository's memory. Conventions and review outcomes
//!    always; code only when the caller says the index is not already doing
//!    that job, because two copies of the same function is worse context than
//!    one.
//! 2. **Ask by question.** The configured questions are templated over the
//!    changed paths and put to the engine's grounded-answer route. This is the
//!    part a keyword store cannot do and the part worth an engine: "which rule
//!    applies here?" answered with a citation is a pointer a reviewer can act
//!    on, where the same rule as the seventh-ranked recollection is not.
//!
//! Everything comes back under one token budget, answers first — they are the
//! synthesis, and the recollections are the evidence — then outcomes, then
//! conventions, then code. Whatever the budget dropped is counted.
//!
//! Nothing here returns an error to the review. An unreachable engine, an empty
//! memory and a question with no grounded answer all produce a
//! [`MemoryContext`] whose [`MemoryStatus`] says which, and the review runs on
//! whatever else it has.

use std::collections::BTreeSet;
use std::fmt::Write as _;

use crate::config::types::Config;
use crate::evidence::diff::FileDiff;
use crate::forge::types::{ReviewComment, ReviewThread};
use crate::memory::ingest;
use crate::memory::types::{
    MemoryAnswer, MemoryItem, MemoryKind, MemoryScope, MemorySection, Recollection,
    RememberReport,
};
use crate::ports::memory::Memory;

/// How many changed paths a question names before the list is elided.
///
/// A question about three hundred paths is a question about nothing in
/// particular, and the engine's own recall does the narrowing better than a
/// path list can.
const MAX_QUESTION_PATHS: usize = 12;

/// The fence label the rendered block goes under in a prompt.
pub const FENCE_LABEL: &str = "repository-memory";

/// Why a review had the memory it had.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MemoryStatus {
    /// No engine is configured. Not a degradation.
    Off,
    /// The engine answered.
    Ready,
    /// The engine could not be reached, or refused. `reason` is the
    /// operator-facing sentence; it never carries a credential.
    Unavailable {
        /// What went wrong, in one line.
        reason: String,
    },
}

/// Everything memory contributed to one review.
#[derive(Debug, Clone, PartialEq)]
pub struct MemoryContext {
    /// Why this is or is not a review with memory.
    pub status: MemoryStatus,
    /// Grounded answers, in the order the questions were configured. Only the
    /// grounded ones: an engine that had nothing to say contributes nothing.
    pub answers: Vec<MemoryAnswer>,
    /// The recollections that reached the prompt, best first within kind.
    pub recollections: Vec<Recollection>,
    /// Candidates the budget or dedupe removed.
    pub dropped: usize,
    /// Tokens the retained block is estimated to cost.
    pub tokens: usize,
    /// What observing this pull request's threads wrote, when it did.
    pub observed: RememberReport,
}

impl Default for MemoryContext {
    fn default() -> Self {
        Self::off()
    }
}

impl MemoryContext {
    /// The context of a review with no memory attached.
    pub fn off() -> Self {
        Self {
            status: MemoryStatus::Off,
            answers: Vec::new(),
            recollections: Vec::new(),
            dropped: 0,
            tokens: 0,
            observed: RememberReport::default(),
        }
    }

    /// The context of a review whose engine could not be reached.
    pub fn unavailable(reason: impl Into<String>) -> Self {
        Self {
            status: MemoryStatus::Unavailable {
                reason: reason.into(),
            },
            ..Self::off()
        }
    }

    /// Whether the lane is shown nothing at all.
    pub fn renders_nothing(&self) -> bool {
        self.answers.is_empty() && self.recollections.is_empty()
    }

    /// How many recollections of each kind reached the prompt.
    pub fn counts(&self) -> (usize, usize, usize) {
        let of = |kind: MemoryKind| {
            self.recollections
                .iter()
                .filter(|r| r.item.kind == kind)
                .count()
        };
        (
            of(MemoryKind::ReviewOutcome),
            of(MemoryKind::Convention),
            of(MemoryKind::CodeChunk),
        )
    }

    /// The block handed to a lane, or an empty string when there is nothing.
    ///
    /// Prompt **suffix** material only: it varies with the pull request and
    /// it is prose the operator did not write. See `crate::harness::prompt`.
    pub fn render(&self) -> String {
        if self.renders_nothing() {
            return String::new();
        }
        let mut out = String::with_capacity(self.tokens * 4 + 256);
        if !self.answers.is_empty() {
            out.push_str("### Answers from memory\n\n");
            for answer in &self.answers {
                let _ = writeln!(out, "Q: {}", answer.question.trim());
                let _ = writeln!(out, "A: {}", answer.answer.trim());
                let cited: Vec<&str> = answer
                    .citations
                    .iter()
                    .filter_map(|c| c.path.as_deref())
                    .collect::<BTreeSet<_>>()
                    .into_iter()
                    .collect();
                if !cited.is_empty() {
                    let _ = writeln!(out, "Cites: {}", cited.join(", "));
                }
                out.push('\n');
            }
        }
        let mut last_kind = None;
        for recollection in &self.recollections {
            let item = &recollection.item;
            if last_kind != Some(item.kind) {
                let heading = match item.kind {
                    MemoryKind::ReviewOutcome => "### Earlier findings and what became of them",
                    MemoryKind::ReviewFinding => "### Earlier findings",
                    MemoryKind::Convention => "### Conventions the repository states",
                    MemoryKind::CodeChunk => "### Remembered code",
                };
                let _ = writeln!(out, "{heading}\n");
                last_kind = Some(item.kind);
            }
            out.push_str(&render_item(item));
            out.push('\n');
        }
        out
    }

    /// The sentence a check-run summary carries when memory was not whole.
    pub fn note(&self) -> Option<String> {
        match &self.status {
            MemoryStatus::Off => None,
            MemoryStatus::Ready if self.renders_nothing() => {
                Some("Memory held nothing relevant to this change.".into())
            }
            MemoryStatus::Ready => None,
            MemoryStatus::Unavailable { reason } => Some(format!(
                "Memory was unavailable ({reason}), so this review ran without it."
            )),
        }
    }
}

/// One recollection as it appears in the prompt.
fn render_item(item: &MemoryItem) -> String {
    let mut out = String::with_capacity(item.body.len() + 96);
    match item.kind {
        MemoryKind::CodeChunk => {
            let _ = write!(out, "// {}", item.path.as_deref().unwrap_or(&item.title));
            if let Some(symbol) = &item.symbol {
                let _ = write!(out, " ({symbol})");
            }
            out.push('\n');
            out.push_str(&item.body);
            if !item.body.ends_with('\n') {
                out.push('\n');
            }
        }
        _ => {
            let _ = writeln!(out, "- **{}**", item.title.trim());
            for line in item.body.trim().lines() {
                let _ = writeln!(out, "  {line}");
            }
        }
    }
    out
}

/// Estimated tokens of one rendered item.
fn item_tokens(item: &MemoryItem) -> usize {
    crate::harness::pricing::estimate_tokens(&render_item(item)) as usize
}

/// Estimated tokens of one rendered answer.
fn answer_tokens(answer: &MemoryAnswer) -> usize {
    crate::harness::pricing::estimate_tokens(&format!(
        "Q: {}\nA: {}\n",
        answer.question, answer.answer
    )) as usize
}

/// The `{paths}` substitution: the changed paths, bounded.
pub fn paths_clause(paths: &[String]) -> String {
    if paths.is_empty() {
        return "the changed files".into();
    }
    let shown: Vec<String> = paths
        .iter()
        .take(MAX_QUESTION_PATHS)
        .map(|p| format!("`{p}`"))
        .collect();
    let mut out = shown.join(", ");
    if paths.len() > MAX_QUESTION_PATHS {
        let _ = write!(out, " and {} more", paths.len() - MAX_QUESTION_PATHS);
    }
    out
}

/// Fill a configured question's placeholders.
pub fn fill_question(template: &str, title: &str, paths: &[String]) -> String {
    template
        .replace("{paths}", &paths_clause(paths))
        .replace("{title}", title.trim())
}

/// Consults memory for one review.
pub struct Recaller<'a> {
    memory: &'a dyn Memory,
}

impl<'a> Recaller<'a> {
    /// A recaller over `memory`.
    pub fn new(memory: &'a dyn Memory) -> Self {
        Self { memory }
    }

    /// The engine behind this recaller.
    pub fn memory(&self) -> &'a dyn Memory {
        self.memory
    }

    /// Remember what became of the reviewer's earlier findings on this pull
    /// request. Best-effort: a failure is logged and reported as zero writes.
    pub async fn observe(
        &self,
        repo: &str,
        number: u64,
        threads: &[ReviewThread],
        comments: &[ReviewComment],
    ) -> RememberReport {
        let items = ingest::outcome_items(repo, number, threads, comments);
        if items.is_empty() {
            return RememberReport::default();
        }
        match ingest::remember_all(self.memory, repo, &items).await {
            Ok(report) => report,
            Err(err) => {
                tracing::warn!(%err, repo, number, "could not remember review outcomes");
                RememberReport::default()
            }
        }
    }

    /// Consult memory for a pull request.
    ///
    /// `include_code` says whether to recall code chunks too; a caller with a
    /// live index passes `false`, because retrieval already shows the lane
    /// the code that reads like the diff.
    pub async fn recall(
        &self,
        config: &Config,
        repo: &str,
        title: &str,
        diffs: &[FileDiff],
        include_code: bool,
    ) -> MemoryContext {
        let settings = &config.memory;
        let paths: Vec<String> = diffs.iter().map(|d| d.path.clone()).collect();
        let query = crate::retrieve::query::build_retrieval_query(
            title,
            diffs,
            config.retrieval.query_chars.max(512),
        );

        let mut sections = vec![MemorySection::Reviews, MemorySection::Conventions];
        if include_code {
            sections.push(MemorySection::Code);
        }

        let mut candidates: Vec<Recollection> = Vec::new();
        if settings.max_recollections > 0 && !query.trim().is_empty() {
            for section in sections {
                let scope = MemoryScope::section(repo, section);
                match self
                    .memory
                    .recall(&scope, &query, settings.max_recollections)
                    .await
                {
                    Ok(hits) => candidates.extend(hits),
                    Err(err) => {
                        tracing::warn!(%err, %scope, "memory recall failed");
                        return MemoryContext::unavailable(sanitize(&err.to_string()));
                    }
                }
            }
        }

        let mut answers = Vec::new();
        if settings.ask {
            let scope = MemoryScope::repo(repo);
            for template in &settings.questions {
                let question = fill_question(template, title, &paths);
                match self
                    .memory
                    .answer(&scope, &question, Some(ANSWER_INSTRUCTIONS))
                    .await
                {
                    Ok(answer) if answer.is_grounded() => {
                        let mut answer = answer;
                        answer.answer = crate::memory::excerpt(&answer.answer, settings.answer_chars);
                        answers.push(answer);
                    }
                    Ok(_) => {}
                    Err(err) => {
                        tracing::warn!(%err, %scope, "memory answer failed");
                        return MemoryContext::unavailable(sanitize(&err.to_string()));
                    }
                }
            }
        }

        assemble(answers, candidates, settings.context_tokens)
    }
}

/// How the engine is asked to shape an answer.
///
/// Constant, and never carries repository text: the question is the
/// operator's template over path names, and this is the operator's too.
const ANSWER_INSTRUCTIONS: &str = "Answer in at most one short paragraph. Quote the rule or the \
    maintainer's words where possible and name the file or pull request each comes from. If the \
    memory holds nothing relevant, say exactly: nothing relevant is remembered.";

/// Rank, dedupe and budget what came back.
fn assemble(
    answers: Vec<MemoryAnswer>,
    candidates: Vec<Recollection>,
    budget_tokens: usize,
) -> MemoryContext {
    let mut context = MemoryContext {
        status: MemoryStatus::Ready,
        ..MemoryContext::off()
    };
    // The engine's "nothing relevant" sentence is a grounded answer with a
    // citation, and it is not worth a line of prompt.
    let answers: Vec<MemoryAnswer> = answers
        .into_iter()
        .filter(|a| !a.answer.to_lowercase().contains("nothing relevant is remembered"))
        .collect();
    let mut remaining = budget_tokens;
    for answer in answers {
        let cost = answer_tokens(&answer);
        if cost > remaining {
            context.dropped += 1;
            continue;
        }
        remaining -= cost;
        context.tokens += cost;
        context.answers.push(answer);
    }

    // Outcomes first: they are the reason this exists. Then conventions, then
    // code, each in the engine's own order.
    let order = |kind: MemoryKind| match kind {
        MemoryKind::ReviewOutcome => 0,
        MemoryKind::ReviewFinding => 1,
        MemoryKind::Convention => 2,
        MemoryKind::CodeChunk => 3,
    };
    let mut ranked: Vec<(usize, Recollection)> = candidates.into_iter().enumerate().collect();
    ranked.sort_by_key(|(position, r)| (order(r.item.kind), *position));

    let mut seen: BTreeSet<String> = BTreeSet::new();
    for (_, recollection) in ranked {
        if !seen.insert(recollection.item.key.clone()) {
            context.dropped += 1;
            continue;
        }
        let cost = item_tokens(&recollection.item);
        if cost > remaining {
            context.dropped += 1;
            continue;
        }
        remaining -= cost;
        context.tokens += cost;
        context.recollections.push(recollection);
    }
    context
}

/// An error message fit for a check-run summary.
///
/// Errors from an HTTP adapter can quote a URL with a query string or a
/// response body; neither belongs on a pull request. Keep the first line,
/// bounded.
fn sanitize(message: &str) -> String {
    crate::memory::excerpt(message.lines().next().unwrap_or_default(), 160)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::MockMemory;
    use crate::memory::types::{MemoryItem, MemoryKind};

    fn config() -> Config {
        let mut config: Config = crate::config::DEFAULTS
            .parse::<toml::Table>()
            .unwrap()
            .try_into()
            .unwrap();
        config.memory.enabled = true;
        config
    }

    fn diff(path: &str) -> FileDiff {
        crate::evidence::diff::parse_file_patch(
            path,
            "@@ -1,1 +1,2 @@ fn ports_trait\n fn ports_trait() {}\n+fn added_ports_fn() {}\n",
        )
    }

    async fn seeded() -> MockMemory {
        let memory = MockMemory::new();
        let repo = MemoryScope::repo("o/r");
        memory
            .remember(
                &repo,
                &[
                    MemoryItem::new(
                        "convention:AGENTS.md#ports",
                        MemoryKind::Convention,
                        "AGENTS.md › Ports",
                        "Every port in src/ports is one trait in one file.",
                    )
                    .at_path("src/ports"),
                    MemoryItem::new(
                        "outcome:o/r#3:abc",
                        MemoryKind::ReviewOutcome,
                        "rejected — Split the ports trait",
                        "Maintainer's reply: the ports trait is deliberately wide.",
                    )
                    .at_path("src/ports/forge.rs"),
                    MemoryItem::new(
                        "code:src/ports/forge.rs#ForgeRead",
                        MemoryKind::CodeChunk,
                        "src/ports/forge.rs — ForgeRead",
                        "pub trait ForgeRead {}",
                    )
                    .at_path("src/ports/forge.rs")
                    .at_symbol("ForgeRead"),
                    MemoryItem::new(
                        "convention:docs/x.md#unrelated",
                        MemoryKind::Convention,
                        "docs/x.md › Deploy",
                        "Deploy with the gated workflow.",
                    )
                    .at_path("docs/x.md"),
                ],
            )
            .await
            .unwrap();
        memory
    }

    #[tokio::test]
    async fn recall_orders_outcomes_before_conventions_and_skips_code_when_told() {
        let memory = seeded().await;
        let recaller = Recaller::new(&memory);
        let diffs = vec![diff("src/ports/forge.rs")];
        let context = recaller
            .recall(&config(), "o/r", "ports change", &diffs, false)
            .await;
        assert_eq!(context.status, MemoryStatus::Ready);
        let kinds: Vec<MemoryKind> = context.recollections.iter().map(|r| r.item.kind).collect();
        assert_eq!(kinds, [MemoryKind::ReviewOutcome, MemoryKind::Convention]);
        let rendered = context.render();
        assert!(rendered.contains("Earlier findings and what became of them"));
        assert!(rendered.contains("deliberately wide"));
        assert!(!rendered.contains("pub trait ForgeRead"));
        assert!(!rendered.contains("Deploy with"));
    }

    #[tokio::test]
    async fn code_is_recalled_when_no_index_is_doing_that_job() {
        let memory = seeded().await;
        let recaller = Recaller::new(&memory);
        let diffs = vec![diff("src/ports/forge.rs")];
        let context = recaller
            .recall(&config(), "o/r", "ports change", &diffs, true)
            .await;
        assert!(context.render().contains("pub trait ForgeRead"));
        let (outcomes, conventions, code) = context.counts();
        assert_eq!((outcomes, conventions, code), (1, 1, 1));
    }

    #[tokio::test]
    async fn grounded_answers_lead_the_block_and_ungrounded_ones_are_omitted() {
        let memory = seeded()
            .await
            .with_answer("conventions", "One trait per file, per AGENTS.md.");
        let recaller = Recaller::new(&memory);
        let mut config = config();
        config.memory.questions = vec![
            "Which conventions apply to {paths}?".into(),
            "Who owns {paths}?".into(),
        ];
        let context = recaller
            .recall(&config, "o/r", "t", &[diff("src/ports/forge.rs")], false)
            .await;
        assert_eq!(context.answers.len(), 1);
        let rendered = context.render();
        assert!(rendered.starts_with("### Answers from memory"));
        assert!(rendered.contains("Q: Which conventions apply to `src/ports/forge.rs`?"));
        assert!(rendered.contains("A: One trait per file"));
    }

    #[tokio::test]
    async fn the_engines_nothing_relevant_sentence_is_not_rendered() {
        let memory = seeded()
            .await
            .with_answer("conventions", "Nothing relevant is remembered.");
        let recaller = Recaller::new(&memory);
        let mut config = config();
        config.memory.questions = vec!["Which conventions apply to {paths}?".into()];
        let context = recaller
            .recall(&config, "o/r", "t", &[diff("src/ports/forge.rs")], false)
            .await;
        assert!(context.answers.is_empty());
    }

    #[tokio::test]
    async fn an_unreachable_engine_degrades_to_a_stated_status() {
        let memory = MockMemory::new();
        memory.fail_with("connection refused to http://127.0.0.1:1?key=secret");
        let recaller = Recaller::new(&memory);
        let context = recaller
            .recall(&config(), "o/r", "t", &[diff("src/a.rs")], false)
            .await;
        assert!(matches!(context.status, MemoryStatus::Unavailable { .. }));
        assert!(context.renders_nothing());
        let note = context.note().unwrap();
        assert!(note.contains("unavailable"), "{note}");
    }

    #[tokio::test]
    async fn the_budget_drops_and_counts() {
        let memory = seeded().await;
        let recaller = Recaller::new(&memory);
        let mut config = config();
        config.memory.context_tokens = 25;
        let context = recaller
            .recall(&config, "o/r", "ports change", &[diff("src/ports/forge.rs")], true)
            .await;
        assert!(context.tokens <= 25);
        assert!(context.dropped >= 1);
    }

    #[tokio::test]
    async fn observing_threads_writes_outcomes_and_reports_the_count() {
        use crate::forge::types::{ReviewThread, ThreadComment};
        let memory = MockMemory::new();
        let recaller = Recaller::new(&memory);
        let fp = "0123456789abcdef";
        let threads = vec![ReviewThread {
            id: "t".into(),
            is_resolved: true,
            is_outdated: false,
            comments: vec![
                ThreadComment {
                    author: "tinysweeper[bot]".into(),
                    body: format!("**Use the crate error**\n\nx\n\n<!-- tinysweeper:fp={fp} -->"),
                    bot: true,
                },
                ThreadComment {
                    author: "alice".into(),
                    body: "Intentional here.".into(),
                    bot: false,
                },
            ],
        }];
        let report = recaller.observe("o/r", 9, &threads, &[]).await;
        assert_eq!(report.written, 1);
        let again = recaller.observe("o/r", 9, &threads, &[]).await;
        assert_eq!(again.replayed, 1);
        let remembered = memory.remembered(&MemoryScope::section("o/r", MemorySection::Reviews));
        assert_eq!(remembered.len(), 1);
        assert!(remembered[0].body.contains("Intentional here."));
    }

    #[test]
    fn questions_are_templated_and_long_path_lists_are_elided() {
        let paths: Vec<String> = (0..20).map(|i| format!("src/f{i}.rs")).collect();
        let q = fill_question("Rules for {paths} in {title}?", "T", &paths);
        assert!(q.starts_with("Rules for `src/f0.rs`, "));
        assert!(q.contains("and 8 more in T?"));
        assert_eq!(
            fill_question("{paths}", "", &[]),
            "the changed files"
        );
    }

    #[test]
    fn notes_say_what_happened() {
        assert!(MemoryContext::off().note().is_none());
        let ready = MemoryContext {
            status: MemoryStatus::Ready,
            ..MemoryContext::off()
        };
        assert!(ready.note().unwrap().contains("nothing relevant"));
        assert!(
            MemoryContext::unavailable("down")
                .note()
                .unwrap()
                .contains("down")
        );
    }
}
