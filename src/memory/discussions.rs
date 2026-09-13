//! Remembering what was said on a repository's issues and pull requests.
//!
//! Always compiled: the item builders are pure functions from forge types to
//! [`MemoryItem`]s, and the one walker runs against the [`ForgeRead`] and
//! [`Memory`] ports, so the whole pipeline is tested offline against
//! [`MockForge`](crate::forge::MockForge) and
//! [`MockMemory`](crate::memory::MockMemory).
//!
//! # What is remembered
//!
//! Every issue and every pull request, open **and closed**, as one item each;
//! and every remark anybody left on them — a comment in the conversation, an
//! inline comment on the diff, a submitted review — as one item each. Other
//! review agents are remembered exactly like people, labelled as bots: what
//! CodeRabbit flagged and a maintainer waved through is as much a fact about
//! this repository as anything a human wrote. The reviewer's own remarks are
//! the one exclusion. Its findings already live in the `reviews` section with
//! their outcomes, and remembering its own prose here would mean recalling an
//! echo of itself as if it were evidence.
//!
//! # Why one section, and why it ranks where it does
//!
//! Discussions are their own [`MemorySection`] so that "what was decided
//! about this file?" is answered from what people said, not drowned by code
//! chunks that mention the same identifiers. In the prompt they rank below
//! conventions — a convention is a rule, a discussion is evidence — and above
//! code, which the index already shows the lane. See `recall::assemble`.
//!
//! # Two feeds, one shape
//!
//! The server remembers a conversation when a webhook says it changed, and
//! the backfill walks the whole history for the time before the server was
//! listening. Both go through [`Discussions::remember_subject`], so a
//! comment remembered live and the same comment remembered by a later
//! backfill are byte-for-byte the same item, and the engine replays it
//! rather than storing two.
//!
//! # Edits
//!
//! An edited comment offers the same key with a new body, which the engine
//! stores as a new version beside the old one; both carry the same
//! `remark:` key in their header, and the later `observed_at` wins on
//! freshness. Nothing is forgotten on an edit, deliberately: a maintainer
//! softening "this is wrong" to "this is fine" is a fact worth keeping too.
//!
//! Everything here is untrusted input — anyone who can comment writes a body
//! — and it reaches a prompt only as fenced, labelled data. Bodies are cut at
//! `memory.discussion_chars`, which is also the bound on how much of a
//! stranger's text one remark can carry into a review.

use std::fmt::Write as _;

use crate::config::types::Memory as MemoryConfig;
use crate::error::Result;
use crate::findings::prior::is_own_login;
use crate::forge::types::{Issue, PullRequest, Remark, RemarkKind, RepoId, ReviewEvent};
use crate::memory::ingest::remember_all;
use crate::memory::types::{MemoryItem, MemoryKind, RememberReport};
use crate::ports::forge::ForgeRead;
use crate::ports::memory::Memory;

/// How many of an issue's labels are carried as memory labels.
///
/// An engine caps labels per item; the kind, number, author and state labels
/// come first because they are what a recall filters on, and a repository
/// with forty labels on one issue is not saying forty things about it.
const MAX_ISSUE_LABELS: usize = 8;

/// How much of a remark's first line becomes the item title.
const TITLE_CHARS: usize = 80;

/// How many issues and pull requests one backfill call walks by default.
///
/// The GitHub listing pages a hundred at a time and every entry costs one to
/// three more reads for its conversation, so this is a few thousand requests
/// at most — well inside an installation's hourly budget, and the CLI and
/// the admin route both take an explicit larger number when a repository
/// needs it.
pub const DEFAULT_BACKFILL_LIMIT: usize = 1000;

/// The thing a conversation hangs off.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Subject {
    /// An issue.
    Issue(Issue),
    /// A pull request.
    PullRequest(PullRequest),
}

impl Subject {
    /// The item number.
    pub fn number(&self) -> u64 {
        match self {
            Self::Issue(issue) => issue.number,
            Self::PullRequest(pr) => pr.number,
        }
    }

    /// The title.
    pub fn title(&self) -> &str {
        match self {
            Self::Issue(issue) => &issue.title,
            Self::PullRequest(pr) => &pr.title,
        }
    }

    /// Whether this is a pull request.
    pub fn is_pull_request(&self) -> bool {
        matches!(self, Self::PullRequest(_))
    }

    /// `issue` or `pull request`, for prose.
    fn noun(&self) -> &'static str {
        if self.is_pull_request() {
            "pull request"
        } else {
            "issue"
        }
    }

    /// The label that names this subject on every item about it.
    fn label(&self) -> String {
        if self.is_pull_request() {
            format!("pr:{}", self.number())
        } else {
            format!("issue:{}", self.number())
        }
    }
}

/// The one item that stands for a subject itself.
///
/// The key is the subject's number, so an edited title or body is a new
/// version of the same memory rather than a second issue. The state goes in
/// the body — `open`, `closed`, `merged` — because a recall that turns up an
/// issue should say whether it is still a problem.
pub fn subject_item(repo: &str, subject: &Subject, max_chars: usize) -> MemoryItem {
    let number = subject.number();
    let mut body = String::new();
    let mut item = match subject {
        Subject::Issue(issue) => {
            let _ = writeln!(body, "Issue {repo}#{number}: {}", issue.title.trim());
            let state = if issue.open {
                "open".to_string()
            } else {
                match &issue.closed_at {
                    Some(at) => format!("closed on {at}"),
                    None => "closed".to_string(),
                }
            };
            let _ = writeln!(body, "State: {state}");
            let _ = writeln!(body, "Opened by {}", who(&issue.author, issue.author_is_bot, ""));
            if let Some(at) = &issue.created_at {
                let _ = writeln!(body, "Opened on {at}");
            }
            if !issue.labels.is_empty() {
                let _ = writeln!(body, "Labels: {}", issue.labels.join(", "));
            }
            if let Some(kind) = &issue.issue_type {
                let _ = writeln!(body, "Type: {kind}");
            }
            let mut item = MemoryItem::new(
                format!("issue:{repo}#{number}"),
                MemoryKind::Issue,
                format!("Issue #{number}: {}", issue.title.trim()),
                String::new(),
            )
            .labelled(format!("state:{}", if issue.open { "open" } else { "closed" }))
            .labelled(format!("author:{}", issue.author));
            for label in issue.labels.iter().take(MAX_ISSUE_LABELS) {
                item = item.labelled(format!("label:{label}"));
            }
            if let Some(at) = issue.updated_at.as_ref().or(issue.created_at.as_ref()) {
                item = item.observed(at.clone());
            }
            item
        }
        Subject::PullRequest(pr) => {
            let _ = writeln!(body, "Pull request {repo}#{number}: {}", pr.title.trim());
            let state = if pr.merged {
                "merged"
            } else if pr.open {
                "open"
            } else {
                "closed without merging"
            };
            let _ = writeln!(body, "State: {state}");
            let _ = writeln!(
                body,
                "Opened by {}; {} into {}",
                who(&pr.author, pr.author_is_bot, ""),
                pr.head_ref,
                pr.base_ref
            );
            if !pr.labels.is_empty() {
                let _ = writeln!(body, "Labels: {}", pr.labels.join(", "));
            }
            let mut item = MemoryItem::new(
                format!("pr:{repo}#{number}"),
                MemoryKind::PullRequest,
                format!("Pull request #{number}: {}", pr.title.trim()),
                String::new(),
            )
            .labelled(format!("state:{state}").replace(' ', "-"))
            .labelled(format!("author:{}", pr.author));
            for label in pr.labels.iter().take(MAX_ISSUE_LABELS) {
                item = item.labelled(format!("label:{label}"));
            }
            item
        }
    };
    let text = match subject {
        Subject::Issue(issue) => issue.body.as_str(),
        Subject::PullRequest(pr) => pr.body.as_str(),
    };
    if !text.trim().is_empty() {
        let _ = write!(body, "\n{}", crate::memory::excerpt(text, max_chars));
    }
    item.body = body;
    item.labelled(subject.label())
        .labelled(if subject_is_bot(subject) {
            "bot"
        } else {
            "human"
        })
}

/// One item per remark on `subject`, in the order given, skipping the
/// reviewer's own and anything with nothing in it.
///
/// The key carries the remark's kind as well as its id because GitHub
/// numbers issue comments and review comments from different sequences; the
/// same integer can name one of each.
pub fn remark_items(
    repo: &str,
    subject: &Subject,
    remarks: &[Remark],
    max_chars: usize,
) -> Vec<MemoryItem> {
    let number = subject.number();
    remarks
        .iter()
        .filter(|remark| !is_own_login(&remark.author))
        .filter(|remark| !remark.body.trim().is_empty() || remark.verdict.is_some())
        .map(|remark| {
            let mut body = String::new();
            let what = match remark.kind {
                RemarkKind::Comment => "Comment",
                RemarkKind::ReviewComment => "Inline review comment",
                RemarkKind::Review => "Review",
            };
            let _ = writeln!(
                body,
                "{what} on {} {repo}#{number} ({})",
                subject.noun(),
                subject.title().trim()
            );
            let _ = writeln!(body, "By {}", who(&remark.author, remark.bot, &remark.association));
            if let Some(at) = &remark.created_at {
                let _ = writeln!(body, "On {at}");
            }
            if let Some(path) = &remark.path {
                let _ = writeln!(
                    body,
                    "Location: {path}{}",
                    remark.line.map(|l| format!(":{l}")).unwrap_or_default()
                );
            }
            if let Some(verdict) = remark.verdict {
                let _ = writeln!(body, "Verdict: {}", verdict_word(verdict));
            }
            if let Some(parent) = remark.in_reply_to {
                let _ = writeln!(body, "In reply to comment {parent}");
            }
            let excerpt = crate::memory::excerpt(&remark.body, max_chars);
            if !excerpt.is_empty() {
                let _ = write!(body, "\n{excerpt}");
            }

            let headline = remark
                .body
                .lines()
                .map(str::trim)
                .find(|line| !line.is_empty())
                .map(|line| crate::memory::excerpt(line, TITLE_CHARS))
                .or_else(|| remark.verdict.map(|v| verdict_word(v).to_string()))
                .unwrap_or_default();
            let mut item = MemoryItem::new(
                format!("remark:{repo}#{number}:{}:{}", remark.kind.as_str(), remark.id),
                MemoryKind::Remark,
                format!("{} on #{number}: {headline}", remark.author),
                body,
            )
            .labelled(subject.label())
            .labelled(format!("remark:{}", remark.kind.as_str()))
            .labelled(format!("author:{}", remark.author))
            .labelled(if remark.bot { "bot" } else { "human" });
            if !remark.association.is_empty() {
                item = item.labelled(format!("association:{}", remark.association));
            }
            if let Some(verdict) = remark.verdict {
                item = item.labelled(format!("verdict:{}", verdict_word(verdict).replace(' ', "-")));
            }
            if let Some(path) = &remark.path {
                item = item.at_path(path.clone());
            }
            if let Some(at) = &remark.created_at {
                item = item.observed(at.clone());
            }
            item
        })
        .collect()
}

/// Everything to remember about one conversation: the subject, then its
/// remarks.
pub fn discussion_items(
    repo: &str,
    subject: &Subject,
    remarks: &[Remark],
    max_chars: usize,
) -> Vec<MemoryItem> {
    let mut items = vec![subject_item(repo, subject, max_chars)];
    items.extend(remark_items(repo, subject, remarks, max_chars));
    items
}

/// `login`, with `(bot)` and the association when they say something.
fn who(login: &str, bot: bool, association: &str) -> String {
    let mut out = login.to_string();
    if bot {
        out.push_str(" (bot)");
    }
    if !association.is_empty() && association != "none" {
        let _ = write!(out, ", {association}");
    }
    out
}

fn subject_is_bot(subject: &Subject) -> bool {
    match subject {
        Subject::Issue(issue) => issue.author_is_bot,
        Subject::PullRequest(pr) => pr.author_is_bot,
    }
}

/// A verdict in the words a reviewer would use.
fn verdict_word(verdict: ReviewEvent) -> &'static str {
    match verdict {
        ReviewEvent::Approve => "approved",
        ReviewEvent::RequestChanges => "changes requested",
        ReviewEvent::Comment => "commented",
    }
}

/// What a discussion ingest did.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DiscussionReport {
    /// Issues and pull requests whose conversations were read.
    pub subjects: usize,
    /// Remarks offered to the engine, the reviewer's own excluded.
    pub remarks: usize,
    /// What the engine reported, over every batch.
    pub remembered: RememberReport,
    /// Subjects that could not be read or written, as `#number: reason`.
    /// A backfill continues past them; a live remember has at most one.
    pub failed: Vec<String>,
    /// The `updated_at` of the last subject a backfill walked, which is
    /// where the next incremental backfill resumes from.
    pub resume_from: Option<String>,
}

impl DiscussionReport {
    /// One line for a log or a CLI.
    pub fn summary(&self) -> String {
        let mut out = format!(
            "{} conversation(s), {} remark(s); {} written, {} already remembered",
            self.subjects, self.remarks, self.remembered.written, self.remembered.replayed
        );
        if !self.failed.is_empty() {
            let _ = write!(out, ", {} failed", self.failed.len());
        }
        if let Some(at) = &self.resume_from {
            let _ = write!(out, "; resume from {at}");
        }
        out
    }

    fn absorb(&mut self, other: DiscussionReport) {
        self.subjects += other.subjects;
        self.remarks += other.remarks;
        self.remembered.merge(other.remembered);
        self.failed.extend(other.failed);
    }
}

/// Reads conversations off a forge and remembers them.
pub struct Discussions<'a> {
    memory: &'a dyn Memory,
    forge: &'a dyn ForgeRead,
    config: &'a MemoryConfig,
}

impl<'a> Discussions<'a> {
    /// A pipeline over `memory` fed from `forge`, bounded by `config`.
    pub fn new(memory: &'a dyn Memory, forge: &'a dyn ForgeRead, config: &'a MemoryConfig) -> Self {
        Self {
            memory,
            forge,
            config,
        }
    }

    /// Remember one issue or pull request by number, reading it first.
    ///
    /// `pull_request` says which it is, because the forge has to be asked
    /// differently for each and a webhook already knows. The wrong answer is
    /// harmless — a pull request read as an issue is remembered without its
    /// review comments, and the next delivery for it corrects that.
    pub async fn remember_number(
        &self,
        repo: &RepoId,
        number: u64,
        pull_request: bool,
    ) -> Result<DiscussionReport> {
        let subject = if pull_request {
            Subject::PullRequest(self.forge.pull_request(repo, number).await?)
        } else {
            Subject::Issue(self.forge.issue(repo, number).await?)
        };
        self.remember_subject(repo, &subject).await
    }

    /// Remember `subject` and everything said on it.
    ///
    /// The one path both feeds share — see the module docs.
    pub async fn remember_subject(
        &self,
        repo: &RepoId,
        subject: &Subject,
    ) -> Result<DiscussionReport> {
        let remarks = self
            .forge
            .remarks(repo, subject.number(), subject.is_pull_request())
            .await?;
        let items = discussion_items(
            &repo.to_string(),
            subject,
            &remarks,
            self.config.discussion_chars,
        );
        let remembered = remember_all(self.memory, &repo.to_string(), &items).await?;
        Ok(DiscussionReport {
            subjects: 1,
            remarks: items.len() - 1,
            remembered,
            failed: Vec::new(),
            resume_from: None,
        })
    }

    /// Walk every issue and pull request touched since `since`, oldest
    /// change first, and remember each, up to `limit` of them.
    ///
    /// One subject failing — a deleted pull request, a conversation past the
    /// page bound, an engine hiccup — is recorded and skipped rather than
    /// ending the walk: a backfill is a sweep over history, and stopping at
    /// the first bad entry would leave everything after it unremembered
    /// with no way to say so. `resume_from` is set only when every subject
    /// succeeded, so a resumed backfill never skips past a failure.
    pub async fn backfill(
        &self,
        repo: &RepoId,
        since: Option<&str>,
        limit: usize,
    ) -> Result<DiscussionReport> {
        let listing = self.forge.issues_updated_since(repo, since, limit).await?;
        let mut report = DiscussionReport::default();
        let mut last_seen: Option<String> = None;
        for entry in &listing {
            let number = entry.number;
            let outcome = if entry.pull_request {
                // The listing renders a pull request as an issue and does not
                // know whether it merged; the pull request itself does.
                match self.forge.pull_request(repo, number).await {
                    Ok(pr) => self.remember_subject(repo, &Subject::PullRequest(pr)).await,
                    Err(err) => Err(err),
                }
            } else {
                self.remember_subject(repo, &Subject::Issue(entry.clone()))
                    .await
            };
            match outcome {
                Ok(one) => {
                    report.absorb(one);
                    last_seen = entry.updated_at.clone().or(last_seen);
                }
                Err(err) => {
                    tracing::warn!(%repo, number, %err, "could not remember a conversation");
                    report.failed.push(format!("#{number}: {err}"));
                }
            }
        }
        if report.failed.is_empty() {
            report.resume_from = last_seen;
        }
        Ok(report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::forge::MockForge;
    use crate::memory::MockMemory;
    use crate::memory::types::{MemoryScope, MemorySection};

    fn issue(number: u64, open: bool) -> Issue {
        Issue {
            number,
            title: "Retry storms when mongo is slow".into(),
            body: "The server drops deliveries when claim_delivery takes over ten seconds.".into(),
            author: "maintainer".into(),
            labels: vec!["bug".into(), "server".into()],
            open,
            created_at: Some("2026-08-13T10:00:00Z".into()),
            updated_at: Some("2026-08-14T10:00:00Z".into()),
            closed_at: (!open).then(|| "2026-08-14T10:00:00Z".to_string()),
            ..Issue::default()
        }
    }

    fn pull_request(number: u64) -> PullRequest {
        PullRequest {
            number,
            title: "fix(server): acknowledge before claiming".into(),
            body: "Closes #1.".into(),
            author: "contributor".into(),
            author_is_bot: false,
            draft: false,
            base_ref: "main".into(),
            base_sha: "b".repeat(40),
            head_ref: "ack-first".into(),
            head_sha: "h".repeat(40),
            from_fork: true,
            labels: vec![],
            mergeable: Some(true),
            open: false,
            merged: true,
            approvals: 1,
            age_days: 3,
            quiet_days: 1,
        }
    }

    fn remark(id: u64, kind: RemarkKind, author: &str, body: &str) -> Remark {
        Remark {
            id,
            kind,
            author: author.into(),
            bot: author.ends_with("[bot]"),
            association: if author == "maintainer" {
                "owner".into()
            } else {
                "none".into()
            },
            body: body.into(),
            created_at: Some(format!("2026-08-14T10:{id:02}:00Z")),
            path: None,
            line: None,
            in_reply_to: None,
            verdict: None,
        }
    }

    #[test]
    fn an_issue_becomes_one_item_that_says_how_it_ended() {
        let item = subject_item("o/r", &Subject::Issue(issue(7, false)), 2000);
        assert_eq!(item.key, "issue:o/r#7");
        assert_eq!(item.kind, MemoryKind::Issue);
        assert_eq!(item.section(), MemorySection::Discussions);
        assert!(item.body.contains("State: closed on 2026-08-14T10:00:00Z"), "{}", item.body);
        assert!(item.body.contains("Labels: bug, server"));
        assert!(item.body.contains("claim_delivery takes over ten seconds"));
        assert!(item.labels.contains(&"issue:7".to_string()));
        assert!(item.labels.contains(&"state:closed".to_string()));
        assert!(item.labels.contains(&"label:bug".to_string()));
        assert_eq!(item.observed_at.as_deref(), Some("2026-08-14T10:00:00Z"));
    }

    #[test]
    fn a_merged_pull_request_is_remembered_as_merged_not_closed() {
        let item = subject_item("o/r", &Subject::PullRequest(pull_request(9)), 2000);
        assert_eq!(item.key, "pr:o/r#9");
        assert_eq!(item.kind, MemoryKind::PullRequest);
        assert!(item.body.contains("State: merged"), "{}", item.body);
        assert!(item.body.contains("ack-first into main"));
        assert!(item.labels.contains(&"pr:9".to_string()));
        assert!(item.labels.contains(&"state:merged".to_string()));

        let mut closed = pull_request(9);
        closed.merged = false;
        let item = subject_item("o/r", &Subject::PullRequest(closed), 2000);
        assert!(item.body.contains("State: closed without merging"));
        assert!(item.labels.contains(&"state:closed-without-merging".to_string()));
    }

    #[test]
    fn remarks_from_other_agents_are_kept_and_the_reviewers_own_are_not() {
        let subject = Subject::PullRequest(pull_request(9));
        let mut inline = remark(
            3,
            RemarkKind::ReviewComment,
            "coderabbitai[bot]",
            "Consider bounding this loop.",
        );
        inline.path = Some("src/server/routes.rs".into());
        inline.line = Some(42);
        let mut review = remark(4, RemarkKind::Review, "maintainer", "");
        review.verdict = Some(ReviewEvent::Approve);
        let remarks = vec![
            remark(1, RemarkKind::Comment, "tinysweeper", "## Change map\n..."),
            remark(2, RemarkKind::Comment, "maintainer", "Intentional: the caller checks.\nMore."),
            inline,
            review,
            remark(5, RemarkKind::Comment, "someone", "   "),
        ];
        let items = remark_items("o/r", &subject, &remarks, 2000);
        let keys: Vec<&str> = items.iter().map(|i| i.key.as_str()).collect();
        assert_eq!(
            keys,
            [
                "remark:o/r#9:comment:2",
                "remark:o/r#9:review-comment:3",
                "remark:o/r#9:review:4"
            ],
            "own comment and the blank one are skipped"
        );

        let human = &items[0];
        assert_eq!(human.kind, MemoryKind::Remark);
        assert_eq!(human.title, "maintainer on #9: Intentional: the caller checks.");
        assert!(human.body.contains("By maintainer, owner"), "{}", human.body);
        assert!(human.labels.contains(&"association:owner".to_string()));
        assert!(human.labels.contains(&"human".to_string()));
        assert_eq!(human.observed_at.as_deref(), Some("2026-08-14T10:02:00Z"));

        let bot = &items[1];
        assert_eq!(bot.path.as_deref(), Some("src/server/routes.rs"));
        assert!(bot.body.contains("Location: src/server/routes.rs:42"));
        assert!(bot.body.contains("By coderabbitai[bot] (bot)"));
        assert!(bot.labels.contains(&"bot".to_string()));
        assert!(bot.labels.contains(&"remark:review-comment".to_string()));

        let verdict = &items[2];
        assert_eq!(verdict.title, "maintainer on #9: approved");
        assert!(verdict.body.contains("Verdict: approved"));
        assert!(verdict.labels.contains(&"verdict:approved".to_string()));
    }

    #[test]
    fn a_long_body_is_cut_at_the_configured_ceiling() {
        let subject = Subject::Issue(issue(1, true));
        let long = "x".repeat(5000);
        let items = remark_items(
            "o/r",
            &subject,
            &[remark(1, RemarkKind::Comment, "someone", &long)],
            100,
        );
        let quoted = items[0].body.rsplit("\n\n").next().unwrap();
        assert_eq!(quoted.chars().count(), 100);
        assert!(quoted.ends_with('…'));
    }

    #[test]
    fn the_same_remark_offered_twice_is_one_memory() {
        let subject = Subject::Issue(issue(1, true));
        let remarks = [remark(1, RemarkKind::Comment, "someone", "hello")];
        let a = remark_items("o/r", &subject, &remarks, 2000);
        let b = remark_items("o/r", &subject, &remarks, 2000);
        assert_eq!(a[0].content_id(), b[0].content_id());

        let mut edited = remarks[0].clone();
        edited.body = "hello, edited".into();
        let c = remark_items("o/r", &subject, &[edited], 2000);
        assert_eq!(a[0].key, c[0].key, "an edit is the same remark");
        assert_ne!(a[0].content_id(), c[0].content_id(), "with a new body");
    }

    fn config() -> MemoryConfig {
        let config: crate::config::Config = crate::config::DEFAULTS
            .parse::<toml::Table>()
            .unwrap()
            .try_into()
            .unwrap();
        config.memory
    }

    #[tokio::test]
    async fn remembering_a_pull_request_reads_its_whole_conversation() {
        let forge = MockForge::new()
            .with_pull_request(pull_request(9), vec![], vec![])
            .with_remarks(
                9,
                vec![
                    remark(1, RemarkKind::Comment, "maintainer", "Looks right."),
                    remark(2, RemarkKind::ReviewComment, "greptile-apps[bot]", "Nit."),
                ],
            );
        let memory = MockMemory::new();
        let config = config();
        let repo = RepoId::parse("o/r").unwrap();
        let report = Discussions::new(&memory, &forge, &config)
            .remember_number(&repo, 9, true)
            .await
            .unwrap();
        assert_eq!(report.subjects, 1);
        assert_eq!(report.remarks, 2);
        assert_eq!(report.remembered.written, 3);

        let held = memory.remembered(&MemoryScope::section("o/r", MemorySection::Discussions));
        assert_eq!(held.len(), 3);
        assert!(held.iter().any(|i| i.kind == MemoryKind::PullRequest));
        assert!(
            memory
                .remembered(&MemoryScope::section("o/r", MemorySection::Reviews))
                .is_empty(),
            "discussions never write into the reviews section"
        );

        // Again: everything replays, nothing is written twice.
        let again = Discussions::new(&memory, &forge, &config)
            .remember_number(&repo, 9, true)
            .await
            .unwrap();
        assert_eq!(again.remembered.written, 0);
        assert_eq!(again.remembered.replayed, 3);
    }

    #[tokio::test]
    async fn a_backfill_walks_closed_issues_and_pull_requests_and_reports_where_to_resume() {
        let mut older = issue(1, false);
        older.updated_at = Some("2026-08-01T00:00:00Z".into());
        let mut newer = issue(2, true);
        newer.updated_at = Some("2026-08-20T00:00:00Z".into());
        let mut pr_listing = issue(9, false);
        pr_listing.pull_request = true;
        pr_listing.updated_at = Some("2026-08-10T00:00:00Z".into());

        let forge = MockForge::new()
            .with_issue(older)
            .with_issue(newer)
            .with_issue(pr_listing)
            .with_pull_request(pull_request(9), vec![], vec![])
            .with_remarks(1, vec![remark(1, RemarkKind::Comment, "someone", "me too")])
            .with_remarks(9, vec![remark(2, RemarkKind::Review, "maintainer", "LGTM")]);
        let memory = MockMemory::new();
        let config = config();
        let repo = RepoId::parse("o/r").unwrap();

        let report = Discussions::new(&memory, &forge, &config)
            .backfill(&repo, None, 100)
            .await
            .unwrap();
        assert_eq!(report.subjects, 3, "{}", report.summary());
        assert_eq!(report.remarks, 2);
        assert!(report.failed.is_empty());
        assert_eq!(report.resume_from.as_deref(), Some("2026-08-20T00:00:00Z"));

        let held = memory.remembered(&MemoryScope::section("o/r", MemorySection::Discussions));
        let keys: std::collections::BTreeSet<&str> = held.iter().map(|i| i.key.as_str()).collect();
        assert!(keys.contains("issue:o/r#1"), "closed issue remembered");
        assert!(keys.contains("issue:o/r#2"));
        assert!(keys.contains("pr:o/r#9"), "the listing's pull request is read as one");
        assert!(held.iter().any(|i| i.key == "pr:o/r#9" && i.body.contains("State: merged")));

        // Resuming from the report walks only what changed after it.
        let resumed = Discussions::new(&memory, &forge, &config)
            .backfill(&repo, report.resume_from.as_deref(), 100)
            .await
            .unwrap();
        assert_eq!(resumed.subjects, 0);
        assert_eq!(resumed.resume_from, None);
    }

    #[tokio::test]
    async fn a_backfill_continues_past_a_subject_it_cannot_read_and_does_not_offer_a_resume_point()
    {
        let mut missing = issue(9, false);
        missing.pull_request = true;
        missing.updated_at = Some("2026-08-10T00:00:00Z".into());
        let mut fine = issue(2, true);
        fine.updated_at = Some("2026-08-20T00:00:00Z".into());
        // No `with_pull_request(9)`: the forge 404s the pull request read.
        let forge = MockForge::new().with_issue(missing).with_issue(fine);
        let memory = MockMemory::new();
        let config = config();
        let repo = RepoId::parse("o/r").unwrap();

        let report = Discussions::new(&memory, &forge, &config)
            .backfill(&repo, None, 100)
            .await
            .unwrap();
        assert_eq!(report.subjects, 1);
        assert_eq!(report.failed.len(), 1);
        assert!(report.failed[0].starts_with("#9: "), "{:?}", report.failed);
        assert_eq!(
            report.resume_from, None,
            "a resume point past a failure would skip it forever"
        );
    }

    #[tokio::test]
    async fn an_engine_failure_is_an_error_on_a_live_remember() {
        let forge = MockForge::new().with_issue(issue(1, true));
        let memory = MockMemory::new();
        memory.fail_with("engine down");
        let config = config();
        let repo = RepoId::parse("o/r").unwrap();
        let err = Discussions::new(&memory, &forge, &config)
            .remember_number(&repo, 1, false)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("engine down"), "{err}");
    }
}
