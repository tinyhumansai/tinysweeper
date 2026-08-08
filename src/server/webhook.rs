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
    /// Nothing to do, with a reason for the log.
    Ignore(&'static str),
}

/// Decide what a delivery means.
///
/// Deliberately conservative: anything not explicitly understood is ignored,
/// because the failure mode of a wrong guess is spending money and posting
/// comments on something nobody asked about.
pub fn route(event: &str, payload: &Payload) -> Action {
    // A bot's own activity must never wake it up. Without this, posting a
    // review comment triggers a delivery that triggers a review that posts a
    // comment, and the loop is only bounded by the rate limiter.
    if let Some(sender) = &payload.sender
        && sender.kind == "Bot"
    {
        return Action::Ignore("sender is a bot");
    }

    let Some(repository) = &payload.repository else {
        return Action::Ignore("no repository");
    };
    let Some(installation) = &payload.installation else {
        return Action::Ignore("no installation");
    };

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
        _ => Action::Ignore("uninteresting event"),
    }
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
