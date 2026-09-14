//! Publishing a preview — the write half.
//!
//! Holds a [`ForgeWrite`] and executes a decision some other module already
//! made: the gallery was validated by `manifest`, captioned by `caption`, and
//! rendered by `render` before this is called, and nothing here reads a model
//! or changes what it was handed. One of the enumerated write modules in
//! `AGENTS.md`, and held to the same bar as the others.
//!
//! Two writes, both idempotent per head commit:
//!
//! - **One comment per pull request**, found by [`render::MARKER`] and edited
//!   in place. A body identical to what is already posted is not re-sent —
//!   an edit that changes nothing still bumps `updated_at` and pings anyone
//!   subscribed.
//! - **One check run per head commit**, `tinysweeper/ui-preview`, always
//!   `Neutral`: a preview is a picture, not a verdict, and a required check
//!   that goes red because a screenshot failed to upload is a merge blocked
//!   for nothing.
//!
//! Before either, the live head is compared with the one the gallery is for.
//! The CI job that produced it ran minutes ago; a push since means the
//! pictures are of a commit nobody is looking at.

use crate::error::{Error, Result};
use crate::forge::types::{CheckConclusion, CheckRun, RepoId};
use crate::ports::forge::{ForgeRead, ForgeWrite};
use crate::preview::render;
use crate::preview::types::Gallery;

/// What `publish` did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The comment was created or edited, and the check published.
    Published,
    /// The head moved since the pictures were taken; nothing was written.
    HeadMoved,
    /// Nothing to show; only the check was published, saying so.
    NothingToShow,
    /// The comment already says exactly this; only the check was published.
    Unchanged,
}

/// Publish a gallery.
pub async fn publish(
    read: &dyn ForgeRead,
    write: &dyn ForgeWrite,
    repo: &str,
    gallery: &Gallery,
) -> Result<Outcome> {
    let repo_id =
        RepoId::parse(repo).ok_or_else(|| Error::Forge(format!("`{repo}` is not owner/name")))?;

    let live = read.pull_request(&repo_id, gallery.number).await?;
    if live.head_sha != gallery.head_sha {
        tracing::info!(
            previewed = %gallery.head_sha,
            live = %live.head_sha,
            "head moved since the preview ran; not publishing stale pictures"
        );
        return Ok(Outcome::HeadMoved);
    }

    let body = render::comment(gallery);
    let outcome = match &body {
        None => Outcome::NothingToShow,
        Some(body) => {
            let existing =
                crate::findings::prior::own_comment(read, &repo_id, gallery.number, render::MARKER)
                    .await?;
            match existing {
                Some(comment) if comment.body == *body => Outcome::Unchanged,
                Some(comment) => match comment.id {
                    // A comment with no id cannot be edited; posting a new one
                    // is the harmless direction to be wrong in.
                    Some(id) => {
                        write.update_comment(&repo_id, id, body).await?;
                        Outcome::Published
                    }
                    None => {
                        write.create_comment(&repo_id, gallery.number, body).await?;
                        Outcome::Published
                    }
                },
                None => {
                    write.create_comment(&repo_id, gallery.number, body).await?;
                    Outcome::Published
                }
            }
        }
    };

    // The check is published in every case, including "nothing to show":
    // that is the one message the comment deliberately does not carry.
    write
        .publish_check(
            &repo_id,
            CheckRun {
                name: render::CHECK_NAME.into(),
                head_sha: gallery.head_sha.clone(),
                conclusion: Some(CheckConclusion::Neutral),
                title: render::check_title(gallery),
                summary: body.unwrap_or_else(|| {
                    "No user flow produced a visible change on this commit.".to_string()
                }),
                images: render::check_images(gallery),
            },
        )
        .await?;

    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::forge::mock::{MockForge, Write};
    use crate::forge::types::{IssueComment, PullRequest};
    use crate::preview::types::{GalleryChange, GalleryFlow};

    fn pull_request(head: &str) -> PullRequest {
        PullRequest {
            number: 7,
            head_sha: head.into(),
            ..PullRequest::default()
        }
    }

    fn gallery() -> Gallery {
        Gallery {
            number: 7,
            head_sha: "abc".into(),
            flows: vec![GalleryFlow {
                id: "f1".into(),
                title: "Open settings".into(),
                caption: Some("Settings gain a toggle.".into()),
                is_new: false,
                clip: None,
                changes: vec![GalleryChange {
                    full_url: "https://p.example/o/r/abc/run-1/change-01.png".into(),
                    crop_url: "https://p.example/o/r/abc/run-1/change-01.crop.png".into(),
                    before_url: None,
                    path: "/settings".into(),
                    callouts: vec![],
                }],
            }],
            empty_flows: 0,
            dropped_flows: 0,
        }
    }

    #[tokio::test]
    async fn a_first_run_creates_the_comment_and_publishes_a_neutral_check() {
        let forge = MockForge::new().with_pull_request(pull_request("abc"), vec![], vec![]);
        let outcome = publish(&forge, &forge, "o/r", &gallery()).await.unwrap();
        assert_eq!(outcome, Outcome::Published);

        let writes = forge.writes();
        assert_eq!(writes.len(), 2);
        let Write::Comment { number, body } = &writes[0] else {
            panic!("a comment first: {writes:?}");
        };
        assert_eq!(*number, 7);
        assert!(body.starts_with(render::MARKER));
        let Write::Check(check) = &writes[1] else {
            panic!("then the check: {writes:?}");
        };
        assert_eq!(check.name, render::CHECK_NAME);
        assert_eq!(check.conclusion, Some(CheckConclusion::Neutral));
        assert_eq!(check.title, "1 flow previewed");
        assert_eq!(check.images.len(), 1);
        assert_eq!(
            check.images[0].caption.as_deref(),
            Some("Settings gain a toggle.")
        );
    }

    #[tokio::test]
    async fn a_second_run_edits_the_same_comment() {
        let forge = MockForge::new()
            .with_pull_request(pull_request("abc"), vec![], vec![])
            .with_comments(
                7,
                vec![IssueComment {
                    id: Some(41),
                    author: "tinysweeper".into(),
                    body: format!("{}\nold", render::MARKER),
                }],
            );
        let outcome = publish(&forge, &forge, "o/r", &gallery()).await.unwrap();
        assert_eq!(outcome, Outcome::Published);
        assert!(matches!(
            &forge.writes()[0],
            Write::CommentUpdate { comment_id: 41, .. }
        ));
    }

    #[tokio::test]
    async fn an_identical_body_is_not_re_sent() {
        let body = render::comment(&gallery()).unwrap();
        let forge = MockForge::new()
            .with_pull_request(pull_request("abc"), vec![], vec![])
            .with_comments(
                7,
                vec![IssueComment {
                    id: Some(41),
                    author: "tinysweeper".into(),
                    body,
                }],
            );
        let outcome = publish(&forge, &forge, "o/r", &gallery()).await.unwrap();
        assert_eq!(outcome, Outcome::Unchanged);
        assert!(matches!(forge.writes().as_slice(), [Write::Check(_)]));
    }

    #[tokio::test]
    async fn a_contributor_who_copies_the_marker_does_not_get_their_comment_edited() {
        let forge = MockForge::new()
            .with_pull_request(pull_request("abc"), vec![], vec![])
            .with_comments(
                7,
                vec![IssueComment {
                    id: Some(9),
                    author: "someone-else".into(),
                    body: format!("{}\nmine", render::MARKER),
                }],
            );
        publish(&forge, &forge, "o/r", &gallery()).await.unwrap();
        assert!(matches!(&forge.writes()[0], Write::Comment { .. }));
    }

    #[tokio::test]
    async fn a_moved_head_publishes_nothing() {
        let forge = MockForge::new().with_pull_request(pull_request("def"), vec![], vec![]);
        let outcome = publish(&forge, &forge, "o/r", &gallery()).await.unwrap();
        assert_eq!(outcome, Outcome::HeadMoved);
        assert!(forge.writes().is_empty());
    }

    #[tokio::test]
    async fn nothing_to_show_is_only_a_check() {
        let forge = MockForge::new().with_pull_request(pull_request("abc"), vec![], vec![]);
        let empty = Gallery {
            flows: vec![],
            ..gallery()
        };
        let outcome = publish(&forge, &forge, "o/r", &empty).await.unwrap();
        assert_eq!(outcome, Outcome::NothingToShow);
        let writes = forge.writes();
        let [Write::Check(check)] = writes.as_slice() else {
            panic!("only a check: {writes:?}");
        };
        assert_eq!(check.title, "No visible change found");
        assert!(check.images.is_empty());
    }
}
