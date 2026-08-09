//! Webhook signature verification and event parsing.
//!
//! The signature check is the entire security boundary of the server: anyone
//! on the internet can POST here, and the only thing separating a real GitHub
//! delivery from a forged one is the HMAC. It is therefore checked before the
//! body is parsed, not after, and compared in constant time.

use hmac::{Hmac, KeyInit, Mac};
use serde::Deserialize;
use sha2::Sha256;
use subtle::ConstantTimeEq;

use crate::error::{Error, Result};

type HmacSha256 = Hmac<Sha256>;

/// Verify GitHub's `X-Hub-Signature-256` header against the raw body.
///
/// `signature` is the full header value, including the `sha256=` prefix.
pub fn verify(secret: &str, body: &[u8], signature: &str) -> Result<()> {
    let provided = signature
        .strip_prefix("sha256=")
        .ok_or_else(|| Error::Forge("signature header is not sha256=…".into()))?;

    let provided = decode_hex(provided)
        .ok_or_else(|| Error::Forge("signature header is not valid hex".into()))?;

    let mut mac = HmacSha256::new_from_slice(secret.as_bytes())
        .map_err(|err| Error::Forge(format!("invalid webhook secret: {err}")))?;
    mac.update(body);
    let expected = mac.finalize().into_bytes();

    // Constant time: a byte-at-a-time comparison leaks how much of a forged
    // signature was correct, which is enough to forge one given patience.
    if expected.ct_eq(provided.as_slice()).into() {
        Ok(())
    } else {
        Err(Error::Forge("webhook signature does not match".into()))
    }
}

fn decode_hex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).ok())
        .collect()
}

/// The parts of a webhook payload the server acts on.
#[derive(Debug, Clone, Deserialize)]
pub struct Payload {
    /// The event's action, e.g. `opened` or `synchronize`.
    #[serde(default)]
    pub action: String,
    /// The repository it concerns.
    #[serde(default)]
    pub repository: Option<Repository>,
    /// The pull request, on pull-request events.
    #[serde(default)]
    pub pull_request: Option<PullRequestRef>,
    /// The issue, on issue and comment events.
    #[serde(default)]
    pub issue: Option<IssueRef>,
    /// The comment, on comment events.
    #[serde(default)]
    pub comment: Option<CommentRef>,
    /// The installation, on any event from an installed app.
    #[serde(default)]
    pub installation: Option<InstallationRef>,
    /// The sender.
    #[serde(default)]
    pub sender: Option<UserRef>,
    /// The check run, on `check_run` events.
    #[serde(default)]
    pub check_run: Option<CheckRef>,
    /// The check suite, on `check_suite` events.
    #[serde(default)]
    pub check_suite: Option<CheckRef>,
}

/// A check run or check suite, reduced to the pull requests it reports on.
///
/// Only `pull_requests` is read. The conclusion carried alongside it is
/// deliberately ignored: auto-merge re-reads every check on the head SHA
/// through the forge anyway, and a decision made from the one check that
/// happened to arrive last would be a decision made from a fifth of the
/// evidence.
#[derive(Debug, Clone, Deserialize)]
pub struct CheckRef {
    /// The pull requests this check reports on.
    ///
    /// Empty on a check for a commit that belongs to no open pull request, and
    /// — a real GitHub quirk — on a check for a pull request from a fork,
    /// which is why an empty list is ignored rather than treated as an error.
    #[serde(default)]
    pub pull_requests: Vec<PullRequestNumberRef>,
}

/// A pull request reduced to its number.
///
/// Separate from [`PullRequestRef`] because the objects GitHub nests inside a
/// check payload carry no `user` and no `draft`, so deserialising them as a
/// `PullRequestRef` would fail on every check event.
#[derive(Debug, Clone, Deserialize)]
pub struct PullRequestNumberRef {
    /// Its number.
    pub number: u64,
}

/// A repository reference.
#[derive(Debug, Clone, Deserialize)]
pub struct Repository {
    /// `owner/name`.
    pub full_name: String,
}

/// A pull request reference.
#[derive(Debug, Clone, Deserialize)]
pub struct PullRequestRef {
    /// Its number.
    pub number: u64,
    /// Whether it is a draft.
    #[serde(default)]
    pub draft: bool,
    /// Its head.
    pub head: HeadRef,
    /// Its author.
    pub user: UserRef,
}

/// A head reference.
#[derive(Debug, Clone, Deserialize)]
pub struct HeadRef {
    /// The commit.
    pub sha: String,
}

/// An issue reference.
#[derive(Debug, Clone, Deserialize)]
pub struct IssueRef {
    /// Its number.
    pub number: u64,
    /// Present when the issue is really a pull request.
    #[serde(default)]
    pub pull_request: Option<serde_json::Value>,
    /// Its author.
    pub user: UserRef,
}

/// A comment reference.
#[derive(Debug, Clone, Deserialize)]
pub struct CommentRef {
    /// Its body.
    #[serde(default)]
    pub body: String,
    /// Its author.
    pub user: UserRef,
    /// The comment this one replies to, on an inline review comment.
    ///
    /// Present only on a reply, which is how a reply to one of tinysweeper's
    /// findings is told from somebody starting a thread of their own.
    #[serde(default)]
    pub in_reply_to_id: Option<u64>,
}

/// A user reference.
#[derive(Debug, Clone, Deserialize)]
pub struct UserRef {
    /// GitHub login.
    pub login: String,
    /// Account type: `User` or `Bot`.
    #[serde(default, rename = "type")]
    pub kind: String,
}

/// An installation reference.
#[derive(Debug, Clone, Deserialize)]
pub struct InstallationRef {
    /// Its id.
    pub id: u64,
}

/// What the server decided to do about a delivery.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Review this pull request.
    Review {
        /// `owner/name`.
        repo: String,
        /// Pull request number.
        number: u64,
        /// Who to attribute the review to.
        author: String,
        /// The installation that can act on it.
        installation: u64,
    },
    /// Triage this issue: label it, and consider closing it.
    ///
    /// Separate from [`Action::Review`] because it is a different job on a
    /// different subject, and folding the two would mean a pull request review
    /// and an issue triage sharing a lease key.
    TriageIssue {
        /// `owner/name`.
        repo: String,
        /// Issue number.
        number: u64,
        /// Who opened it. The close gate needs it to protect maintainers.
        author: String,
        /// The installation that can act on it.
        installation: u64,
    },
    /// Re-evaluate this pull request against the auto-merge policy.
    ///
    /// Carries no author. Auto-merge reads no model, spends no money and
    /// attributes nothing to anybody — it is arithmetic over state the forge
    /// already holds — so the contributor record has nothing to record.
    AutoMerge {
        /// `owner/name`.
        repo: String,
        /// The pull requests to re-evaluate.
        ///
        /// A list rather than a number because one commit can be the head of
        /// several open pull requests, and a check payload names all of them.
        /// Taking the first would silently strand the rest.
        numbers: Vec<u64>,
        /// The installation that can act on it.
        installation: u64,
    },
    /// Nothing to do, with a reason for the log.
    Ignore(&'static str),
}

/// Decide what a delivery means.
///
/// Deliberately conservative: anything not explicitly understood is ignored,
/// because the failure mode of a wrong guess is spending money and posting
/// comments on something nobody asked about.
pub fn route(event: &str, payload: &Payload) -> Action {
    let Some(repository) = &payload.repository else {
        return Action::Ignore("no repository");
    };
    let Some(installation) = &payload.installation else {
        return Action::Ignore("no installation");
    };

    // Auto-merge is decided before the bot guard below, and deliberately.
    //
    // Every event that can make a pull request mergeable is sent by a bot: the
    // check runs are ours, and the approval that clears a previous objection is
    // ours too. Applying the guard here would mean the trigger never fires on
    // the events that matter, which is how a gate ends up looking implemented
    // and never running.
    //
    // It is safe to exempt because the loop the guard exists to stop cannot
    // form: auto-merge writes nothing but a merge, a merge closes the pull
    // request, and a closed pull request is refused by the first check in the
    // policy. A refusal writes nothing at all.
    if let Some(numbers) = automerge_trigger(event, payload) {
        return Action::AutoMerge {
            repo: repository.full_name.clone(),
            numbers,
            installation: installation.id,
        };
    }

    // A bot's own activity must never wake it up. Without this, posting a
    // review comment triggers a delivery that triggers a review that posts a
    // comment, and the loop is only bounded by the rate limiter.
    if let Some(sender) = &payload.sender
        && sender.kind == "Bot"
    {
        return Action::Ignore("sender is a bot");
    }

    match event {
        "pull_request" => {
            let Some(pr) = &payload.pull_request else {
                return Action::Ignore("no pull request");
            };
            match payload.action.as_str() {
                "opened" | "synchronize" | "reopened" | "ready_for_review" | "edited" => {
                    Action::Review {
                        repo: repository.full_name.clone(),
                        number: pr.number,
                        author: pr.user.login.clone(),
                        installation: installation.id,
                    }
                }
                _ => Action::Ignore("uninteresting pull request action"),
            }
        }
        "issue_comment" => {
            // GitHub delivers `issue_comment` for `created`, `edited`, and
            // `deleted`, unlike the `pull_request` branch above which already
            // whitelists actions. Without this, editing a comment to add
            // `@tinysweeper` after the fact — or any edit to a comment that
            // already mentioned it — queues another paid review every time.
            if payload.action != "created" {
                return Action::Ignore("comment action is not `created`");
            }
            let Some(issue) = &payload.issue else {
                return Action::Ignore("no issue");
            };
            if issue.pull_request.is_none() {
                return Action::Ignore("comment is on an issue, not a pull request");
            }
            let asked = payload
                .comment
                .as_ref()
                .map(|c| c.body.trim_start().starts_with("@tinysweeper"))
                .unwrap_or(false);
            if !asked {
                return Action::Ignore("comment is not addressed to tinysweeper");
            }
            // Attributed to whoever asked, not to whoever opened the pull
            // request. The contributor record is a measure of the work someone
            // caused; billing a maintainer's `@tinysweeper review` to the
            // author would quietly distort every trust signal built on it.
            let asker = payload
                .comment
                .as_ref()
                .map(|c| c.user.login.clone())
                .unwrap_or_else(|| issue.user.login.clone());

            Action::Review {
                repo: repository.full_name.clone(),
                number: issue.number,
                author: asker,
                installation: installation.id,
            }
        }
        "issues" => {
            let Some(issue) = &payload.issue else {
                return Action::Ignore("no issue");
            };
            // GitHub sends `issues` for a pull request too on some app
            // configurations. Triaging one as an issue would label it from the
            // wrong vocabulary and, worse, offer it to the close gate.
            if issue.pull_request.is_some() {
                return Action::Ignore("the issue is a pull request");
            }
            match payload.action.as_str() {
                // `opened` and `reopened` are the moments triage is useful.
                // `edited` is included because a report is often only
                // classifiable after the author fills in the template — and
                // deliberately not `labeled`, which tinysweeper's own labelling
                // would otherwise trigger in a loop.
                "opened" | "reopened" | "edited" => Action::TriageIssue {
                    repo: repository.full_name.clone(),
                    number: issue.number,
                    author: issue.user.login.clone(),
                    installation: installation.id,
                },
                _ => Action::Ignore("uninteresting issue action"),
            }
        }

        "pull_request_review_comment" => {
            // Only `created`, for the same reason `issue_comment` filters:
            // GitHub delivers `edited` and `deleted` here too, and reacting to
            // them would queue a paid run every time somebody fixed a typo in
            // their own reply.
            if payload.action != "created" {
                return Action::Ignore("review comment action is not `created`");
            }
            let Some(pr) = &payload.pull_request else {
                return Action::Ignore("no pull request");
            };
            let Some(comment) = &payload.comment else {
                return Action::Ignore("no comment");
            };
            // A reply, not a new thread. A fresh inline comment starts somebody
            // else's conversation, which thread resolution never touches, and
            // reacting to one would mean a run per commented line.
            if comment.in_reply_to_id.is_none() {
                return Action::Ignore("review comment is not a reply to a thread");
            }

            // Attributed to whoever replied: the same reasoning as a commanded
            // review, which is that the contributor record measures the work
            // somebody caused.
            Action::Review {
                repo: repository.full_name.clone(),
                number: pr.number,
                author: comment.user.login.clone(),
                installation: installation.id,
            }
        }
        _ => Action::Ignore("uninteresting event"),
    }
}

/// The pull requests a delivery invites auto-merge to reconsider, if any.
///
/// Every arm is a moment at which a refusal the policy made earlier might have
/// stopped being true, and nothing else. There is no arm for `opened` or
/// `synchronize`: a pull request that has just appeared or just moved has no
/// checks on its new head yet, so evaluating it can only produce
/// `CheckPending`, and the `check_suite` that follows a moment later is the
/// same trigger with the evidence attached.
///
/// `None` means "not an auto-merge trigger" and leaves the delivery to the
/// rest of [`route`]. An empty list is never returned, so a check event naming
/// no pull request falls through to the ordinary path rather than queueing a
/// job with nothing to do.
fn automerge_trigger(event: &str, payload: &Payload) -> Option<Vec<u64>> {
    let numbers = match event {
        // A check finishing is the commonest reason a `CheckPending` or
        // `CheckFailing` refusal has just expired.
        "check_run" | "check_suite" if payload.action == "completed" => {
            let check = match event {
                "check_run" => payload.check_run.as_ref(),
                _ => payload.check_suite.as_ref(),
            }?;
            check
                .pull_requests
                .iter()
                .map(|pr| pr.number)
                .collect::<Vec<_>>()
        }
        // An approval arriving, or a changes-request being dismissed or
        // superseded. This is the event tinysweeper's own approving review
        // produces, which is what makes the bot-guard exemption above matter.
        "pull_request_review" if matches!(payload.action.as_str(), "submitted" | "dismissed") => {
            vec![payload.pull_request.as_ref()?.number]
        }
        // Labels are the human opt-in and the human veto: `allow_labels` and
        // `block_labels` are both evaluated from them, so adding or removing
        // one is a direct instruction to reconsider.
        //
        // `ready_for_review` is deliberately absent even though it clears the
        // `Draft` refusal: it is already a review trigger, and lanes skip a
        // draft, so a pull request leaving draft needs reviewing before it
        // needs merging. The approval that review produces brings it back here.
        "pull_request" if matches!(payload.action.as_str(), "labeled" | "unlabeled") => {
            vec![payload.pull_request.as_ref()?.number]
        }
        _ => return None,
    };

    (!numbers.is_empty()).then_some(numbers)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &str = "it's a secret to everybody";

    fn sign(secret: &str, body: &[u8]) -> String {
        let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(body);
        format!(
            "sha256={}",
            mac.finalize()
                .into_bytes()
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        )
    }

    #[test]
    fn a_correctly_signed_body_verifies() {
        let body = b"{\"action\":\"opened\"}";
        verify(SECRET, body, &sign(SECRET, body)).expect("verifies");
    }

    #[test]
    fn a_tampered_body_is_rejected() {
        let body = b"{\"action\":\"opened\"}";
        let signature = sign(SECRET, body);
        verify(SECRET, b"{\"action\":\"closed\"}", &signature)
            .expect_err("must reject a body that does not match");
    }

    #[test]
    fn a_wrong_secret_is_rejected() {
        let body = b"{}";
        verify(SECRET, body, &sign("wrong", body)).expect_err("must reject");
    }

    #[test]
    fn a_malformed_signature_header_is_rejected_rather_than_ignored() {
        let body = b"{}";
        for bad in ["", "deadbeef", "sha256=zzzz", "sha1=abcd", "sha256=abc"] {
            verify(SECRET, body, bad).expect_err(bad);
        }
    }

    fn payload(json: serde_json::Value) -> Payload {
        serde_json::from_value(json).expect("parses")
    }

    fn pr_payload(action: &str) -> Payload {
        payload(serde_json::json!({
            "action": action,
            "repository": {"full_name": "tinyhumansai/tinysweeper"},
            "installation": {"id": 152184043},
            "sender": {"login": "someone", "type": "User"},
            "pull_request": {
                "number": 7,
                "draft": false,
                "head": {"sha": "abc123"},
                "user": {"login": "someone", "type": "User"}
            }
        }))
    }

    fn issue_payload(action: &str) -> Payload {
        payload(serde_json::json!({
            "action": action,
            "repository": {"full_name": "tinyhumansai/tinysweeper"},
            "installation": {"id": 152184043},
            "sender": {"login": "reporter", "type": "User"},
            "issue": {
                "number": 42,
                "user": {"login": "reporter", "type": "User"}
            }
        }))
    }

    #[test]
    fn a_new_issue_is_triaged() {
        assert_eq!(
            route("issues", &issue_payload("opened")),
            Action::TriageIssue {
                repo: "tinyhumansai/tinysweeper".into(),
                number: 42,
                author: "reporter".into(),
                installation: 152184043,
            }
        );
    }

    #[test]
    fn an_edited_issue_is_triaged_again() {
        assert!(matches!(
            route("issues", &issue_payload("edited")),
            Action::TriageIssue { .. }
        ));
    }

    #[test]
    fn labelling_an_issue_does_not_trigger_triage() {
        // tinysweeper's own labelling delivers `labeled`. Acting on it would be
        // a loop bounded only by the rate limiter.
        assert_eq!(
            route("issues", &issue_payload("labeled")),
            Action::Ignore("uninteresting issue action")
        );
    }

    #[test]
    fn a_pull_request_delivered_as_an_issue_is_not_triaged() {
        let mut payload = issue_payload("opened");
        payload.issue.as_mut().expect("an issue").pull_request =
            Some(serde_json::json!({"url": "https://example.invalid/pulls/42"}));
        assert_eq!(
            route("issues", &payload),
            Action::Ignore("the issue is a pull request")
        );
    }

    #[test]
    fn a_bot_never_triggers_triage_on_its_own_labelling() {
        let mut payload = issue_payload("opened");
        payload.sender.as_mut().expect("a sender").kind = "Bot".into();
        assert_eq!(route("issues", &payload), Action::Ignore("sender is a bot"));
    }

    #[test]
    fn a_push_to_a_pull_request_is_reviewed() {
        assert_eq!(
            route("pull_request", &pr_payload("synchronize")),
            Action::Review {
                repo: "tinyhumansai/tinysweeper".into(),
                number: 7,
                author: "someone".into(),
                installation: 152184043,
            }
        );
    }

    #[test]
    fn closing_a_pull_request_does_nothing() {
        assert!(matches!(
            route("pull_request", &pr_payload("closed")),
            Action::Ignore(_)
        ));
    }

    #[test]
    fn the_bots_own_activity_never_wakes_it_up() {
        // Without this the loop is bounded only by the rate limiter: posting a
        // comment delivers an event that triggers a review that posts a comment.
        let mut payload = pr_payload("synchronize");
        payload.sender = Some(UserRef {
            login: "tinysweeper[bot]".into(),
            kind: "Bot".into(),
        });
        assert_eq!(
            route("pull_request", &payload),
            Action::Ignore("sender is a bot")
        );
    }

    #[test]
    fn a_comment_on_an_issue_is_not_a_pull_request() {
        let p = payload(serde_json::json!({
            "action": "created",
            "repository": {"full_name": "tinyhumansai/tinysweeper"},
            "installation": {"id": 1},
            "sender": {"login": "someone", "type": "User"},
            "issue": {"number": 3, "user": {"login": "someone", "type": "User"}},
            "comment": {"body": "@tinysweeper review", "user": {"login": "someone", "type": "User"}}
        }));
        assert!(matches!(route("issue_comment", &p), Action::Ignore(_)));
    }

    #[test]
    fn only_comments_addressed_to_tinysweeper_trigger_a_review() {
        let base = serde_json::json!({
            "action": "created",
            "repository": {"full_name": "tinyhumansai/tinysweeper"},
            "installation": {"id": 1},
            "sender": {"login": "someone", "type": "User"},
            "issue": {"number": 7, "pull_request": {}, "user": {"login": "author", "type": "User"}},
            "comment": {"body": "looks good to me", "user": {"login": "someone", "type": "User"}}
        });
        assert!(matches!(
            route("issue_comment", &payload(base.clone())),
            Action::Ignore(_)
        ));

        let mut addressed = base;
        addressed["comment"]["body"] = serde_json::json!("@tinysweeper review");
        assert!(matches!(
            route("issue_comment", &payload(addressed)),
            Action::Review { number: 7, .. }
        ));
    }

    #[test]
    fn a_commanded_review_is_attributed_to_the_commenter() {
        // Billing a maintainer's `@tinysweeper review` to the pull request
        // author would distort every trust signal built on the contributor
        // record.
        let p = payload(serde_json::json!({
            "action": "created",
            "repository": {"full_name": "tinyhumansai/tinysweeper"},
            "installation": {"id": 1},
            "sender": {"login": "maintainer", "type": "User"},
            "issue": {"number": 7, "pull_request": {}, "user": {"login": "author", "type": "User"}},
            "comment": {"body": "@tinysweeper review", "user": {"login": "maintainer", "type": "User"}}
        }));
        match route("issue_comment", &p) {
            Action::Review { author, .. } => assert_eq!(author, "maintainer"),
            other => panic!("expected a review, got {other:?}"),
        }
    }

    #[test]
    fn an_edited_or_deleted_comment_does_not_trigger_another_review() {
        // Only `created` should queue a review. Without filtering on the
        // action, editing a comment to add `@tinysweeper` — or any edit to a
        // comment that already mentioned it — would queue a fresh paid
        // review on every save, and a `deleted` delivery may not even carry
        // a comment body to check.
        let base = serde_json::json!({
            "repository": {"full_name": "tinyhumansai/tinysweeper"},
            "installation": {"id": 1},
            "sender": {"login": "someone", "type": "User"},
            "issue": {"number": 7, "pull_request": {}, "user": {"login": "author", "type": "User"}},
            "comment": {"body": "@tinysweeper review", "user": {"login": "someone", "type": "User"}}
        });

        for action in ["edited", "deleted"] {
            let mut delivery = base.clone();
            delivery["action"] = serde_json::json!(action);
            assert!(
                matches!(
                    route("issue_comment", &payload(delivery)),
                    Action::Ignore(_)
                ),
                "action `{action}` must not queue a review"
            );
        }
    }

    fn review_comment_payload(action: &str, reply: bool) -> serde_json::Value {
        let mut delivery = serde_json::json!({
            "action": action,
            "repository": {"full_name": "tinyhumansai/tinysweeper"},
            "installation": {"id": 1},
            "sender": {"login": "author", "type": "User"},
            "pull_request": {
                "number": 7,
                "draft": false,
                "head": {"sha": "abc123"},
                "user": {"login": "author", "type": "User"}
            },
            "comment": {"body": "fixed in the last push", "user": {"login": "author", "type": "User"}}
        });
        if reply {
            delivery["comment"]["in_reply_to_id"] = serde_json::json!(4242);
        }
        delivery
    }

    #[test]
    fn a_human_reply_on_a_review_thread_queues_a_run() {
        // The trigger for thread resolution: somebody answered one of our
        // review comments, so the threads on this pull request are worth
        // re-evaluating.
        assert_eq!(
            route(
                "pull_request_review_comment",
                &payload(review_comment_payload("created", true))
            ),
            Action::Review {
                repo: "tinyhumansai/tinysweeper".into(),
                number: 7,
                author: "author".into(),
                installation: 1,
            }
        );
    }

    #[test]
    fn a_review_comment_that_is_not_a_reply_is_ignored() {
        // A brand-new inline comment starts a thread of somebody else's, which
        // this path never touches — and reacting to it would queue a paid run
        // for every line a reviewer comments on.
        assert!(matches!(
            route(
                "pull_request_review_comment",
                &payload(review_comment_payload("created", false))
            ),
            Action::Ignore(_)
        ));
    }

    #[test]
    fn an_edited_or_deleted_review_comment_does_not_queue_a_run() {
        // Same reasoning as `issue_comment`: reacting to the wrong action
        // queues a paid run on every save.
        for action in ["edited", "deleted"] {
            assert!(
                matches!(
                    route(
                        "pull_request_review_comment",
                        &payload(review_comment_payload(action, true))
                    ),
                    Action::Ignore(_)
                ),
                "action `{action}` must not queue a run"
            );
        }
    }

    #[test]
    fn a_bots_reply_on_a_review_thread_never_queues_a_run() {
        // Two bots replying to each other is a loop bounded only by the rate
        // limiter, and it would resolve threads on each other's say-so.
        let mut delivery = review_comment_payload("created", true);
        delivery["sender"] = serde_json::json!({"login": "dependabot[bot]", "type": "Bot"});
        delivery["comment"]["user"] =
            serde_json::json!({"login": "dependabot[bot]", "type": "Bot"});
        assert!(matches!(
            route("pull_request_review_comment", &payload(delivery)),
            Action::Ignore(_)
        ));
    }

    #[test]
    fn an_event_without_an_installation_is_ignored() {
        let mut p = pr_payload("opened");
        p.installation = None;
        assert!(matches!(route("pull_request", &p), Action::Ignore(_)));
    }

    #[test]
    fn an_unknown_event_is_ignored_rather_than_guessed_at() {
        assert!(matches!(
            route("deployment_status", &pr_payload("created")),
            Action::Ignore(_)
        ));
    }
}
