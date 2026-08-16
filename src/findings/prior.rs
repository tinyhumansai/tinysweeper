//! What tinysweeper already said on a pull request, read back from the forge.
//!
//! tinysweeper posted 48 inline comments on one of its own pull requests
//! because it submitted a fresh review on every push and never looked at what
//! was already there. The machinery to stop that was all present —
//! [`ForgeRead::review_comments`](crate::ports::forge::ForgeRead::review_comments)
//! existed, and the apply path already stamped a `tinysweeper:fp=` marker into
//! every inline comment — and nothing read either of them back. This module is
//! the missing half.
//!
//! ## Comment bodies are untrusted
//!
//! Anyone who can open a pull request can also write `<!-- tinysweeper:fp=… -->`
//! into a review comment, and a naive reader would let them suppress a genuine
//! finding by guessing or copying its fingerprint. Three things stop that:
//!
//! 1. A marker is only honoured on a comment **tinysweeper itself wrote**, by
//!    login. A contributor cannot author a comment as the app.
//! 2. A marker must be a well-formed fingerprint — sixteen lowercase hex
//!    characters — so nothing else in a body is ever treated as one.
//! 3. Suppression only removes a *duplicate comment*. Check-run conclusions and
//!    the merge gate are computed before dedupe runs (see
//!    `crate::app::review::lane_proposal`), so even a marker that somehow got
//!    through hides a repeat comment and cannot unblock a merge.

use std::collections::{BTreeMap, BTreeSet};

use crate::config::types::{LaneId, Severity};
use crate::council::agree::LINE_TOLERANCE;
use crate::error::Result;
use crate::findings::types::Finding;
use crate::forge::types::RepoId;
use crate::ports::forge::ForgeRead;

/// The marker key carrying a finding's fingerprint.
const FINGERPRINT_KEY: &str = "fp=";

/// The marker key carrying the last reviewed head SHA.
const STATE_KEY: &str = "state v=1 sha=";

/// The login tinysweeper posts as, unless the deployment says otherwise.
///
/// Overridable because the GitHub App's slug decides the login, and a
/// self-hosted install will not be called `tinysweeper`. Getting this wrong
/// fails safe in the noisy direction — nothing is recognised as our own, so
/// nothing is deduped — rather than in the direction that lets a stranger
/// suppress findings.
const BOT_LOGIN_ENV: &str = "TINYSWEEPER_BOT_LOGIN";

/// Where an earlier finding was anchored, and which lane raised it.
///
/// Kept alongside the fingerprints because a fingerprint only recognises a
/// finding the model described *identically* twice. It hashes the model-authored
/// `rule` and the snippet the model chose to quote, and neither is stable: one
/// concern about `src/eval/runner.rs` was posted three times across two pushes —
/// at lines 146, then 145 and 171 — under three different fingerprints, because
/// the second run quoted a line either side of the first run's snippet. Dedupe
/// worked exactly as written and never fired. The anchor is the part that did
/// not move.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PostedAnchor {
    /// The lane that raised it, when the comment's badge names one.
    ///
    /// `None` disables lane matching for this anchor rather than matching every
    /// lane: a comment we cannot attribute is not evidence that some particular
    /// lane already spoke.
    pub lane: Option<LaneId>,
    /// The file it anchors to.
    pub path: String,
    /// The head-revision line, when GitHub still places the comment on one.
    pub line: Option<u64>,
    /// The first line, when the comment spans a range.
    pub start_line: Option<u64>,
    /// The title it was posted under, when the body has the renderer's shape.
    ///
    /// The content guard. Position alone says two findings are in the same
    /// place, which is not the same as saying they are the same finding — two
    /// defects three lines apart in one function would be one repeat and one
    /// deletion. `None` disables the anchor for this comment entirely: a body we
    /// cannot read a title out of is not evidence about anything.
    ///
    /// The title rather than the `rule`, on the evidence. The four repeats of
    /// one concern on `tinysweeper#86` carried the rules `untrusted-repo-rules`,
    /// `Pin third-party actions to a commit SHA`, and twice nothing at all,
    /// while the title was identical every time. Keying on the rule would leave
    /// the fallback catching nothing in the exact case it exists for.
    pub title: Option<String>,
}

/// Everything an earlier cycle left behind on the pull request.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PriorReview {
    /// Fingerprints of findings already posted as inline comments.
    pub posted: BTreeSet<String>,
    /// Titles of those findings, in the order they were found.
    ///
    /// This is prompt layer 4: what the model said last time, so it can verify
    /// each one against the current code instead of starting over.
    pub titles: Vec<String>,
    /// The severity each of those titles was reported at.
    ///
    /// Read back off the rendered priority badge, because that is the level the
    /// author is looking at. It goes back into the prompt beside the title so a
    /// re-review can be asked to keep the level it already gave — without it the
    /// model re-decides severity from nothing every cycle, and one unchanged
    /// concern on `tinysweeper#89` was reported medium, then high, then
    /// critical, then high again across four pushes.
    pub severities: BTreeMap<String, Severity>,
    /// Where each already-posted finding sits.
    pub anchors: Vec<PostedAnchor>,
    /// The head SHA of the last review, when a marker recorded one.
    pub last_sha: Option<String>,
}

impl PriorReview {
    /// Whether this finding has already been posted on the pull request.
    pub fn already_posted(&self, identity: &str) -> bool {
        self.posted.contains(identity)
    }

    /// The severity this finding carried when it was posted, if it was.
    ///
    /// Keyed on the title because that is what the model is shown and what it
    /// echoes back; the fingerprint is not something it can quote.
    pub fn severity_of(&self, title: &str) -> Option<Severity> {
        self.severities.get(title).copied()
    }

    /// Whether an earlier comment already sits where this finding anchors.
    ///
    /// Same lane, same file, same title, anchors within [`LINE_TOLERANCE`] —
    /// the positional half of the rule
    /// [`corroborates`](crate::council::agree::corroborates) uses for one defect
    /// seen by two reviewers, applied across pushes instead of across agents,
    /// with a content guard on top.
    ///
    /// It is deliberately the *second* thing dedupe asks. A fingerprint match is
    /// conclusive and this is not, so it only ever catches what the fingerprint
    /// missed.
    ///
    /// Every clause here is a way of refusing to suppress on thin evidence,
    /// which is the same principle `council::agree` applies to two unplaceable
    /// findings on one file:
    ///
    /// - **The title must match.** Position says two findings are in the same
    ///   place; it does not say they are the same finding. Two defects three
    ///   lines apart in one function would otherwise be one repeat and one
    ///   silent deletion — and a deleted finding can flip a verdict.
    /// - **Both must be placed.** An unplaceable finding never matches, because
    ///   `.github/workflows/eval.yml` readily produces two different unplaceable
    ///   problems and there is no positional evidence to tell them apart.
    /// - **The lane must match**, and a comment whose lane or title we cannot
    ///   read matches nothing rather than everything.
    pub fn covers_anchor(&self, finding: &Finding) -> bool {
        let Some((start, end)) = finding.range() else {
            return false;
        };
        self.anchors.iter().any(|anchor| {
            if anchor.path != finding.path || anchor.lane != Some(finding.lane) {
                return false;
            }
            if anchor.title.as_deref() != Some(finding.title.as_str()) {
                return false;
            }
            let Some(anchor_end) = anchor.line else {
                return false;
            };
            let anchor_start = anchor.start_line.unwrap_or(anchor_end);
            start <= anchor_end.saturating_add(LINE_TOLERANCE)
                && end >= anchor_start.saturating_sub(LINE_TOLERANCE)
        })
    }
}

/// Whether `login` is the account tinysweeper posts as.
///
/// A GitHub App comments as `<slug>[bot]`, so the suffix is stripped before
/// comparing. The comparison is exact rather than a prefix match: `starts_with`
/// would accept an account called `tinysweeper-evil`, which anyone can register
/// and which would then be able to forge suppression markers.
///
/// Returns false for empty login or empty configured bot login, failing safe
/// rather than treating an unconfigured bot as anything.
pub fn is_own_login(login: &str) -> bool {
    let expected = std::env::var(BOT_LOGIN_ENV).unwrap_or_else(|_| "tinysweeper".to_string());
    login_matches(login, &expected)
}

/// The comparison itself, with the configured login passed in.
///
/// Split from [`is_own_login`] so the security-critical half can be tested
/// without touching the process environment. `set_var` is unsafe in Rust 2024
/// precisely because it races every other thread reading the environment, and
/// this crate's tests run in parallel — a test that mutated the variable could
/// make an unrelated test read a login that was never configured for it.
fn login_matches(login: &str, expected: &str) -> bool {
    if login.is_empty() {
        return false;
    }
    let trimmed = expected.trim();
    if trimmed.is_empty() {
        return false;
    }
    let bare = login.strip_suffix("[bot]").unwrap_or(login);
    bare.eq_ignore_ascii_case(trimmed)
}

/// Read back every marker tinysweeper left on a pull request.
///
/// Errors are the caller's to decide about: this returns them rather than
/// swallowing them, and `review` degrades to "nothing known" so a forge outage
/// makes the bot repeat itself rather than go silent.
pub async fn load(read: &dyn ForgeRead, repo: &RepoId, number: u64) -> Result<PriorReview> {
    let mut prior = PriorReview::default();

    for comment in read.review_comments(repo, number).await? {
        if !is_own_login(&comment.author) {
            continue;
        }
        if let Some(fingerprint) = fingerprint_in(&comment.body) {
            // Recorded for every one of our comments, including the repeats: a
            // fingerprint we have seen before still tells us that this line is
            // spoken for, which is the whole reason the anchor is kept.
            prior.anchors.push(PostedAnchor {
                lane: lane_in(&comment.body),
                path: comment.path.clone(),
                line: comment.line,
                start_line: comment.start_line,
                title: title_in(&comment.body),
            });

            // A repeated fingerprint is normal — the same finding across two
            // reviews — so the title is only recorded the first time.
            if prior.posted.insert(fingerprint)
                && let Some(title) = title_in(&comment.body)
            {
                if let Some(severity) = severity_in(&comment.body) {
                    // First writer wins. Two comments for one title means the
                    // level already drifted; the earliest is the one the author
                    // has had longest, and re-pinning to it is what stops the
                    // drift rather than following it.
                    prior.severities.entry(title.clone()).or_insert(severity);
                }
                prior.titles.push(title);
            }
        }
    }

    // The issue comments carry the state marker when a durable comment is in
    // use. Absent one, `last_sha` stays `None` and the caller falls back to the
    // review-state store; neither is required for dedupe to work.
    for comment in read.comments(repo, number).await? {
        if is_own_login(&comment.author)
            && let Some(sha) = marker_value(&comment.body, STATE_KEY)
            && is_sha(sha)
        {
            prior.last_sha = Some(sha.to_string());
        }
    }

    Ok(prior)
}

/// The fingerprint marker in a comment body, if it carries a well-formed one.
///
/// Public because thread resolution reads the same marker back out of the
/// comment that opened a thread. It answers only "is there a well-formed
/// fingerprint here"; whether the body is one of *ours* is a separate question,
/// asked with [`is_own_login`], and both have to hold before anything acts.
pub fn fingerprint_in(body: &str) -> Option<String> {
    let value = marker_value(body, FINGERPRINT_KEY)?;
    is_fingerprint(value).then(|| value.to_string())
}

/// The value of the final `<!-- tinysweeper:<key><value> -->` marker in `body`.
///
/// The last marker is authoritative because `apply` appends the renderer-added
/// marker after model-generated body text. This prevents a contributor from
/// injecting a forged marker before the renderer footer.
fn marker_value<'a>(body: &'a str, key: &str) -> Option<&'a str> {
    let opener = format!("<!-- {}{key}", crate::MARKER_PREFIX);
    let start = body.rfind(&opener)? + opener.len();
    let rest = &body[start..];
    let end = rest.find("-->")?;
    Some(rest[..end].trim())
}

/// Whether `value` is one of our fingerprints and not merely marker-shaped
/// text. Sixteen lowercase hex characters, exactly — see [`Finding::fingerprint`].
///
/// [`Finding::fingerprint`]: crate::findings::Finding::fingerprint
fn is_fingerprint(value: &str) -> bool {
    value.len() == 16
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Whether `value` looks like a git object id.
fn is_sha(value: &str) -> bool {
    (7..=40).contains(&value.len()) && value.bytes().all(|b| b.is_ascii_hexdigit())
}

/// The severity a comment tinysweeper wrote was posted at.
///
/// Read off the priority badge, whose alt text `apply` renders as
/// `![priority high](…)`. The badge rather than the colour or the URL: the alt
/// text is the one part a reader and a parser agree on, and it is the same
/// string [`Severity::parse`] round-trips.
pub fn severity_in(body: &str) -> Option<Severity> {
    const OPENER: &str = "![priority ";
    let start = body.find(OPENER)? + OPENER.len();
    let rest = &body[start..];
    let end = rest.find(']')?;
    Severity::parse(&rest[..end])
}

/// The lane that raised a comment tinysweeper wrote.
///
/// Taken from the lane/confidence badge, which `render` builds as
/// `![tests likely](https://img.shields.io/badge/tests-likely-…)`. Scanning the
/// badge URLs rather than the alt text keeps this from matching a lane name that
/// happens to appear in the model's own prose: the priority badge is skipped
/// because no severity is also a lane id, and the first badge whose first
/// segment parses as a lane is the lane badge.
fn lane_in(body: &str) -> Option<LaneId> {
    const BADGE: &str = "img.shields.io/badge/";
    body.match_indices(BADGE)
        .filter_map(|(at, _)| {
            let rest = &body[at + BADGE.len()..];
            let end = rest.find('-')?;
            LaneId::parse(&rest[..end])
        })
        .next()
}

/// The finding title rendered in a comment body, when it has the shape that
/// [`crate::app::apply`] writes.
///
/// The apply path renders `{badge} **{title}**`, so the title is the first bold
/// run. A body that does not match that shape simply has no title, which costs a
/// prompt layer-5 line and nothing else.
///
/// Thread resolution uses the same title that the review agent receives as
/// prior context. A body that does not match this renderer-owned shape has no
/// trustworthy identity to match and therefore remains open.
pub fn title_in(body: &str) -> Option<String> {
    let start = body.find("**")? + 2;
    let rest = &body[start..];
    let end = rest.find("**")?;
    let title = rest[..end].trim();
    (!title.is_empty()).then(|| title.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::forge::types::{IssueComment, ReviewComment};
    use crate::forge::{MockForge, MockState};

    fn repo() -> RepoId {
        RepoId::parse("tinyhumansai/tinysweeper").unwrap()
    }

    fn ours(fingerprint: &str, title: &str) -> ReviewComment {
        ReviewComment {
            path: "src/main.rs".into(),
            line: Some(2),
            start_line: None,
            author: "tinysweeper[bot]".into(),
            body: format!(
                "![high](x) **{title}**\n\nbody\n\n<sub>critique · x · <!-- tinysweeper:fp={fingerprint} --></sub>"
            ),
        }
    }

    async fn load_from(comments: Vec<ReviewComment>) -> PriorReview {
        let mut state = MockState::default();
        state.review_comments.insert(7, comments);
        load(&MockForge::with_state(state), &repo(), 7)
            .await
            .expect("loads")
    }

    #[tokio::test]
    async fn our_own_markers_are_read_back() {
        let prior = load_from(vec![ours("0123456789abcdef", "Guard the index")]).await;
        assert!(prior.already_posted("0123456789abcdef"));
        assert_eq!(prior.titles, vec!["Guard the index"]);
    }

    #[tokio::test]
    async fn a_forged_marker_from_a_contributor_is_ignored() {
        // The attack this defends against: a contributor copies the marker out
        // of a real comment (or guesses one) so the next review suppresses the
        // finding. Authorship is the thing they cannot forge.
        let mut forged = ours("0123456789abcdef", "Guard the index");
        forged.author = "helpful-contributor".into();
        let prior = load_from(vec![forged]).await;

        assert!(
            !prior.already_posted("0123456789abcdef"),
            "a marker in someone else's comment must not suppress anything"
        );
        assert!(prior.titles.is_empty());
    }

    #[tokio::test]
    async fn a_lookalike_login_is_not_us() {
        // `starts_with("tinysweeper")` would have accepted this, and anyone can
        // register the account.
        let mut forged = ours("0123456789abcdef", "Guard the index");
        forged.author = "tinysweeper-evil".into();
        assert!(
            !load_from(vec![forged])
                .await
                .already_posted("0123456789abcdef")
        );
    }

    #[tokio::test]
    async fn marker_shaped_text_that_is_not_a_fingerprint_is_ignored() {
        let mut junk = ours("0123456789abcdef", "t");
        junk.body = "<!-- tinysweeper:fp=../../etc/passwd -->".into();
        assert!(load_from(vec![junk]).await.posted.is_empty());

        let mut short = ours("0123456789abcdef", "t");
        short.body = "<!-- tinysweeper:fp=abc -->".into();
        assert!(load_from(vec![short]).await.posted.is_empty());
    }

    #[tokio::test]
    async fn the_same_finding_posted_twice_is_one_title() {
        let prior = load_from(vec![
            ours("0123456789abcdef", "Guard the index"),
            ours("0123456789abcdef", "Guard the index"),
        ])
        .await;
        assert_eq!(prior.titles.len(), 1);
        assert_eq!(prior.posted.len(), 1);
    }

    #[tokio::test]
    async fn a_state_marker_in_our_own_issue_comment_carries_the_last_sha() {
        let mut state = MockState::default();
        state.comments.insert(
            7,
            vec![
                IssueComment {
                    id: Some(1),
                    author: "someone".into(),
                    body: "<!-- tinysweeper:state v=1 sha=deadbeefdeadbeef -->".into(),
                },
                IssueComment {
                    id: Some(2),
                    author: "tinysweeper".into(),
                    body: "reviewed\n<!-- tinysweeper:state v=1 sha=abc1234 -->".into(),
                },
            ],
        );
        let prior = load(&MockForge::with_state(state), &repo(), 7)
            .await
            .expect("loads");

        assert_eq!(
            prior.last_sha.as_deref(),
            Some("abc1234"),
            "only our own state marker counts"
        );
    }

    #[test]
    fn only_our_own_login_is_recognised() {
        assert!(is_own_login("tinysweeper"));
        assert!(is_own_login("tinysweeper[bot]"));
        assert!(is_own_login("TinySweeper"));
        assert!(!is_own_login("tinysweeper-evil"));
        assert!(!is_own_login("nottinysweeper"));
        assert!(!is_own_login(""));
    }

    #[test]
    fn empty_or_whitespace_configured_bot_login_is_rejected() {
        // A bot login configured to empty or whitespace must match nothing, so
        // a misconfigured install recognises none of its own comments and
        // simply repeats itself, rather than accepting anyone's marker.
        for configured in ["", "   \n\t  "] {
            assert!(!login_matches("tinysweeper", configured));
            assert!(!login_matches("tinysweeper[bot]", configured));
            assert!(!login_matches("", configured));
        }
    }

    #[test]
    fn a_lookalike_login_is_not_our_own() {
        // The whole point of comparing exactly rather than by prefix: anyone
        // can register `tinysweeper-evil`, and a prefix match would let them
        // forge suppression markers.
        assert!(login_matches("tinysweeper", "tinysweeper"));
        assert!(login_matches("tinysweeper[bot]", "tinysweeper"));
        assert!(!login_matches("tinysweeper-evil", "tinysweeper"));
        assert!(!login_matches("tinysweeper-evil[bot]", "tinysweeper"));
    }

    #[test]
    fn a_title_is_the_first_bold_run() {
        assert_eq!(
            title_in("![high](x) **Guard the index** and more").as_deref(),
            Some("Guard the index")
        );
        assert_eq!(title_in("no bold here"), None);
        assert_eq!(title_in("**  **"), None);
    }

    /// A comment body in the shape `apply` actually renders.
    ///
    /// Built from the renderer's own badge helpers rather than a hand-written
    /// lookalike: these parsers read a format another module owns, so the test
    /// that proves they can read it has to break when that module changes its
    /// mind. A fixture string would keep passing while production stopped
    /// parsing.
    fn rendered(severity: Severity, lane: LaneId, title: &str, fingerprint: &str) -> ReviewComment {
        use crate::findings::render::{lane_confidence_badge, priority_badge};
        ReviewComment {
            path: "src/main.rs".into(),
            line: Some(40),
            start_line: None,
            author: "tinysweeper[bot]".into(),
            body: format!(
                "{}  {}\n\n**{title}**\n\nwhy it matters\n\n`rule` · <!-- tinysweeper:fp={fingerprint} -->",
                priority_badge(severity),
                lane_confidence_badge(lane, 0.9),
            ),
        }
    }

    #[tokio::test]
    async fn the_severity_a_finding_was_posted_at_is_read_back() {
        let prior = load_from(vec![rendered(
            Severity::High,
            LaneId::Tests,
            "Guard the index",
            "0123456789abcdef",
        )])
        .await;

        assert_eq!(prior.severity_of("Guard the index"), Some(Severity::High));
        assert_eq!(prior.severity_of("Something else"), None);
    }

    #[tokio::test]
    async fn a_level_that_already_drifted_is_pinned_to_the_earliest() {
        // Two comments, one title, two levels — the state this whole change
        // exists to stop. Following the newest would ratify the drift; the
        // author has had the first one longest, so that is the one that holds.
        let prior = load_from(vec![
            rendered(
                Severity::Medium,
                LaneId::Critique,
                "Guard the index",
                "0123456789abcdef",
            ),
            rendered(
                Severity::Critical,
                LaneId::Critique,
                "Guard the index",
                "fedcba9876543210",
            ),
        ])
        .await;

        assert_eq!(prior.severity_of("Guard the index"), Some(Severity::Medium));
    }

    #[test]
    fn the_lane_is_taken_from_the_badge_and_not_from_the_prose() {
        // `critique` in the body text must not win over the real lane badge:
        // the model writes the body, so anything read out of it is untrusted.
        let comment = rendered(
            Severity::Low,
            LaneId::Security,
            "Escape the interpolation",
            "0123456789abcdef",
        );
        let with_prose = format!("{}\n\nthe critique lane would say tests too", comment.body);

        assert_eq!(lane_in(&with_prose), Some(LaneId::Security));
        assert_eq!(lane_in("no badges at all"), None);
    }

    #[test]
    fn a_body_without_a_priority_badge_has_no_severity() {
        assert_eq!(severity_in("**Guard the index**"), None);
        assert_eq!(severity_in("![priority urgent](x)"), None);
    }

    #[tokio::test]
    async fn a_repeat_of_a_finding_that_moved_a_line_is_recognised_by_its_anchor() {
        // The failure this closes. `src/eval/runner.rs` carried one concern
        // posted three times over two pushes — lines 146, then 145 and 171 —
        // because each run quoted a slightly different snippet and hashed to a
        // fresh fingerprint. The anchor is what did not move.
        let prior = load_from(vec![rendered(
            Severity::Medium,
            LaneId::Tests,
            "Track cost from failed cases",
            "0123456789abcdef",
        )])
        .await;

        let mut moved = finding(LaneId::Tests, "src/main.rs", Some(42));
        moved.title = "Track cost from failed cases".into();
        moved.identity = Some("aaaaaaaaaaaaaaaa".into());
        assert!(
            prior.covers_anchor(&moved),
            "two lines from a comment we already posted is the same finding"
        );

        let mut far_away = moved.clone();
        far_away.line = Some(400);
        assert!(!prior.covers_anchor(&far_away));

        let mut other_file = moved.clone();
        other_file.path = "src/other.rs".into();
        assert!(!prior.covers_anchor(&other_file));

        // A different lane looking at the same line is a different reviewer
        // with a different job, not a repeat.
        let mut other_lane = moved.clone();
        other_lane.lane = LaneId::Security;
        assert!(!prior.covers_anchor(&other_lane));
    }

    #[tokio::test]
    async fn a_different_finding_on_a_nearby_line_is_not_suppressed() {
        // Position says two findings are in the same place. It does not say they
        // are the same finding, and two defects a few lines apart in one
        // function are ordinary. Suppressing on proximity alone would post the
        // first and silently delete the second — and a deleted finding can flip
        // a verdict, which is the failure this whole branch is about.
        let prior = load_from(vec![rendered(
            Severity::Medium,
            LaneId::Critique,
            "Guard the index before dereferencing",
            "0123456789abcdef",
        )])
        .await;

        let mut neighbour = finding(LaneId::Critique, "src/main.rs", Some(41));
        neighbour.title = "Close the file handle on the error path".into();
        assert!(
            !prior.covers_anchor(&neighbour),
            "a different defect one line away was deleted as a duplicate"
        );

        // The same title one line away is the repeat this exists to catch.
        let mut repeat = neighbour.clone();
        repeat.title = "Guard the index before dereferencing".into();
        assert!(prior.covers_anchor(&repeat));
    }

    #[tokio::test]
    async fn a_comment_with_no_readable_title_anchors_nothing() {
        // Fails towards repeating ourselves rather than towards deleting a
        // finding: a body we cannot parse is not evidence about anything.
        let mut unreadable = rendered(
            Severity::Medium,
            LaneId::Critique,
            "Guard the index before dereferencing",
            "0123456789abcdef",
        );
        unreadable.body = "no bold run here <!-- tinysweeper:fp=0123456789abcdef -->".into();
        let prior = load_from(vec![unreadable]).await;

        let mut anything = finding(LaneId::Critique, "src/main.rs", Some(40));
        anything.title = "Guard the index before dereferencing".into();
        assert!(!prior.covers_anchor(&anything));
    }

    #[tokio::test]
    async fn an_unplaceable_finding_is_never_suppressed_by_an_anchor() {
        // No line means no positional evidence, and suppressing on none of it
        // deletes a finding rather than de-duplicating one.
        let prior = load_from(vec![rendered(
            Severity::Medium,
            LaneId::Tests,
            "Pin the action",
            "0123456789abcdef",
        )])
        .await;

        assert!(!prior.covers_anchor(&finding(LaneId::Tests, "src/main.rs", None)));
    }

    /// A finding placed at `line`, with nothing else that matters here set.
    fn finding(lane: LaneId, path: &str, line: Option<u64>) -> Finding {
        Finding {
            lane,
            severity: Severity::Medium,
            confidence: 0.9,
            path: path.into(),
            line,
            end_line: None,
            rule: "some-rule".into(),
            title: "t".into(),
            body: "b".into(),
            suggestion: None,
            applicable: None,
            late: false,
            identity: None,
            corroboration: 1,
        }
    }

    #[tokio::test]
    async fn an_injected_marker_before_the_renderer_footer_is_ignored() {
        // Attack: a contributor injects a valid marker before the renderer
        // footer, hoping to suppress a finding. The renderer appends the
        // authoritative marker, and we read the last one.
        let mut injected = ours("0123456789abcdef", "Guard the index");
        injected.body =
            "<!-- tinysweeper:fp=badf00dbadf00dba -->\n\nbody\n\n<sub>critique · x · <!-- tinysweeper:fp=0123456789abcdef --></sub>"
                .to_string();
        let prior = load_from(vec![injected]).await;

        assert!(
            prior.already_posted("0123456789abcdef"),
            "the final marker (renderer-appended) must be trusted"
        );
        assert!(
            !prior.already_posted("badf00dbadf00dba"),
            "an injected marker before the footer must be ignored"
        );
    }
}
