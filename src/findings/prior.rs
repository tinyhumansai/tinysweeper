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

use std::collections::BTreeSet;

use crate::error::Result;
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
    /// The head SHA of the last review, when a marker recorded one.
    pub last_sha: Option<String>,
}

impl PriorReview {
    /// Whether this finding has already been posted on the pull request.
    pub fn already_posted(&self, identity: &str) -> bool {
        self.posted.contains(identity)
    }
}

/// Whether `login` is the account tinysweeper posts as.
///
/// A GitHub App comments as `<slug>[bot]`, so the suffix is stripped before
/// comparing. The comparison is exact rather than a prefix match: `starts_with`
/// would accept an account called `tinysweeper-evil`, which anyone can register
/// and which would then be able to forge suppression markers.
pub fn is_own_login(login: &str) -> bool {
    let expected = std::env::var(BOT_LOGIN_ENV).unwrap_or_else(|_| "tinysweeper".to_string());
    let bare = login.strip_suffix("[bot]").unwrap_or(login);
    bare.eq_ignore_ascii_case(expected.trim())
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
            // A repeated fingerprint is normal — the same finding across two
            // reviews — so the title is only recorded the first time.
            if prior.posted.insert(fingerprint)
                && let Some(title) = title_in(&comment.body)
            {
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
fn fingerprint_in(body: &str) -> Option<String> {
    let value = marker_value(body, FINGERPRINT_KEY)?;
    is_fingerprint(value).then(|| value.to_string())
}

/// The value of the first `<!-- tinysweeper:<key><value> -->` marker in `body`.
fn marker_value<'a>(body: &'a str, key: &str) -> Option<&'a str> {
    let opener = format!("<!-- {}{key}", crate::MARKER_PREFIX);
    let start = body.find(&opener)? + opener.len();
    let rest = &body[start..];
    let end = rest.find("-->")?;
    Some(rest[..end].trim())
}

/// Whether `value` is one of our fingerprints and not merely marker-shaped
/// text. Sixteen lowercase hex characters, exactly — see [`Finding::fingerprint`].
///
/// [`Finding::fingerprint`]: crate::findings::Finding::fingerprint
fn is_fingerprint(value: &str) -> bool {
    value.len() == 16 && value.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Whether `value` looks like a git object id.
fn is_sha(value: &str) -> bool {
    (7..=40).contains(&value.len()) && value.bytes().all(|b| b.is_ascii_hexdigit())
}

/// The finding title out of a comment tinysweeper wrote.
///
/// The apply path renders `{badge} **{title}**`, so the title is the first
/// bold run. A body that does not match that shape simply has no title, which
/// costs a prompt layer-4 line and nothing else.
fn title_in(body: &str) -> Option<String> {
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
            line: 2,
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
        assert!(!load_from(vec![forged]).await.already_posted("0123456789abcdef"));
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
    fn a_title_is_the_first_bold_run() {
        assert_eq!(
            title_in("![high](x) **Guard the index** and more").as_deref(),
            Some("Guard the index")
        );
        assert_eq!(title_in("no bold here"), None);
        assert_eq!(title_in("**  **"), None);
    }
}
