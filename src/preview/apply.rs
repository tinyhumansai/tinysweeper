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

/// The files to commit to a store branch, when the store is a branch.
///
/// Absent for a bucket store: the hands uploaded there themselves and this
/// module only links.
pub struct Store<'a> {
    /// The branch to commit to.
    pub branch: &'a str,
    /// The uploaded files by their manifest name.
    pub files: &'a std::collections::BTreeMap<String, Vec<u8>>,
}

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
///
/// `existing_check_id` is the id a previous `publish` for the same session
/// already returned, if the caller has one. `publish_check` never replaces a
/// check run of the same name — GitHub keeps both — so a session retried
/// after its `finish` response was lost (the hands' own HTTP client retries a
/// dropped response, and a session is only deleted *after* a successful
/// publish) would otherwise grow a second `tinysweeper/ui-preview` row every
/// retry. Passing the id back lets the caller persist it and update the same
/// check run next time instead.
pub async fn publish(
    read: &dyn ForgeRead,
    write: &dyn ForgeWrite,
    repo: &str,
    gallery: &Gallery,
    existing_check_id: Option<u64>,
    store: Option<&Store<'_>>,
) -> Result<(Outcome, Option<u64>)> {
    let repo_id =
        RepoId::parse(repo).ok_or_else(|| Error::Forge(format!("`{repo}` is not owner/name")))?;

    let live = read.pull_request(&repo_id, gallery.number).await?;
    if live.head_sha != gallery.head_sha {
        tracing::info!(
            previewed = %gallery.head_sha,
            live = %live.head_sha,
            "head moved since the preview ran; not publishing stale pictures"
        );
        // No check to reuse or create: the caller has nothing new to
        // remember, so its existing id (if any) is handed back unchanged.
        return Ok((Outcome::HeadMoved, existing_check_id));
    }

    // The pictures first, so no comment ever links to a file that is not
    // there yet. Only the files a shown flow references are committed; the
    // hands may have uploaded more, and a store branch that keeps every
    // stray upload is a store branch that grows for nothing.
    if let Some(store) = store
        && !gallery.files.is_empty()
    {
        let mut files = Vec::with_capacity(gallery.files.len());
        for name in &gallery.files {
            let bytes = store.files.get(name).ok_or_else(|| {
                Error::Config(format!(
                    "the manifest references `{name}` but the hands never uploaded it"
                ))
            })?;
            files.push((
                crate::preview::manifest::Storage::commit_path(
                    &gallery.head_sha,
                    &gallery.run,
                    name,
                ),
                bytes.clone(),
            ));
        }
        let message = format!(
            "ui-preview: #{} at {} ({})",
            gallery.number,
            &gallery.head_sha[..gallery.head_sha.len().min(12)],
            gallery.run
        );
        let commit = write
            .publish_files(&repo_id, store.branch, &message, &files)
            .await?;
        tracing::info!(branch = store.branch, %commit, files = files.len(), "committed preview files");
    }

    let body = render::comment(gallery);
    let existing =
        crate::findings::prior::own_comment(read, &repo_id, gallery.number, render::MARKER).await?;
    let outcome = match &body {
        // A pull request that never had a preview comment stays quiet (see
        // `render::comment`'s own doc). One that did is left showing an
        // earlier head's pictures under a check that now says there is
        // nothing to see, which is actively misleading rather than quiet —
        // so an existing comment is edited down to a short stale notice.
        None => match &existing {
            Some(comment) => {
                let stale = render::stale(&gallery.head_sha);
                match comment.id {
                    Some(id) => {
                        write.update_comment(&repo_id, id, &stale).await?;
                        Outcome::Published
                    }
                    None => Outcome::NothingToShow,
                }
            }
            None => Outcome::NothingToShow,
        },
        Some(body) => {
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
    let check = CheckRun {
        name: render::CHECK_NAME.into(),
        head_sha: gallery.head_sha.clone(),
        conclusion: Some(CheckConclusion::Neutral),
        title: render::check_title(gallery),
        summary: body.unwrap_or_else(|| {
            "No user flow produced a visible change on this commit.".to_string()
        }),
        images: render::check_images(gallery),
    };
    let check_id = match existing_check_id {
        Some(id) => {
            write.update_check(&repo_id, id, check).await?;
            id
        }
        None => write.publish_check(&repo_id, check).await?,
    };

    Ok((outcome, Some(check_id)))
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
            run: "run-1".into(),
            files: vec![],
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
        let (outcome, check_id) = publish(&forge, &forge, "o/r", &gallery(), None, None)
            .await
            .unwrap();
        assert_eq!(outcome, Outcome::Published);
        assert!(check_id.is_some(), "a fresh check run's id is handed back");

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
    async fn a_branch_store_commits_the_referenced_files_before_the_comment() {
        let forge = MockForge::new().with_pull_request(pull_request("abc"), vec![], vec![]);
        let mut gallery = gallery();
        gallery.files = vec!["change-01.crop.png".into(), "change-01.png".into()];
        let mut uploaded = std::collections::BTreeMap::new();
        uploaded.insert("change-01.crop.png".to_string(), vec![1u8, 2]);
        uploaded.insert("change-01.png".to_string(), vec![3u8]);
        uploaded.insert("stray.png".to_string(), vec![9u8]);
        let store = Store {
            branch: "tinysweeper/ui-previews",
            files: &uploaded,
        };
        let (outcome, _) = publish(&forge, &forge, "o/r", &gallery, None, Some(&store))
            .await
            .unwrap();
        assert_eq!(outcome, Outcome::Published);
        let writes = forge.writes();
        let Write::Files {
            branch,
            message,
            paths,
        } = &writes[0]
        else {
            panic!("the files go first: {writes:?}");
        };
        assert_eq!(branch, "tinysweeper/ui-previews");
        assert!(message.starts_with("ui-preview: #7 at abc"));
        assert_eq!(
            paths,
            &vec![
                "abc/run-1/change-01.crop.png".to_string(),
                "abc/run-1/change-01.png".to_string()
            ],
            "only referenced files, never the stray upload"
        );
        assert!(matches!(&writes[1], Write::Comment { .. }));
    }

    #[tokio::test]
    async fn a_referenced_file_the_hands_never_uploaded_is_refused_before_anything_is_written() {
        let forge = MockForge::new().with_pull_request(pull_request("abc"), vec![], vec![]);
        let mut gallery = gallery();
        gallery.files = vec!["change-01.crop.png".into()];
        let uploaded = std::collections::BTreeMap::new();
        let store = Store {
            branch: "b",
            files: &uploaded,
        };
        assert!(
            publish(&forge, &forge, "o/r", &gallery, None, Some(&store))
                .await
                .is_err()
        );
        assert!(forge.writes().is_empty());
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
        let (outcome, _) = publish(&forge, &forge, "o/r", &gallery(), None, None)
            .await
            .unwrap();
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
        let (outcome, _) = publish(&forge, &forge, "o/r", &gallery(), None, None)
            .await
            .unwrap();
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
        publish(&forge, &forge, "o/r", &gallery(), None, None)
            .await
            .unwrap();
        assert!(matches!(&forge.writes()[0], Write::Comment { .. }));
    }

    #[tokio::test]
    async fn a_moved_head_publishes_nothing() {
        let forge = MockForge::new().with_pull_request(pull_request("def"), vec![], vec![]);
        let (outcome, check_id) = publish(&forge, &forge, "o/r", &gallery(), Some(7), None)
            .await
            .unwrap();
        assert_eq!(outcome, Outcome::HeadMoved);
        assert_eq!(
            check_id,
            Some(7),
            "an id the caller already had is handed back unchanged"
        );
        assert!(forge.writes().is_empty());
    }

    #[tokio::test]
    async fn nothing_to_show_is_only_a_check() {
        let forge = MockForge::new().with_pull_request(pull_request("abc"), vec![], vec![]);
        let empty = Gallery {
            flows: vec![],
            ..gallery()
        };
        let (outcome, _) = publish(&forge, &forge, "o/r", &empty, None, None)
            .await
            .unwrap();
        assert_eq!(outcome, Outcome::NothingToShow);
        let writes = forge.writes();
        let [Write::Check(check)] = writes.as_slice() else {
            panic!("only a check: {writes:?}");
        };
        assert_eq!(check.title, "No visible change found");
        assert!(check.images.is_empty());
    }

    #[tokio::test]
    async fn a_retry_with_a_known_check_id_updates_it_instead_of_publishing_another() {
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
        let (outcome, check_id) = publish(&forge, &forge, "o/r", &gallery(), Some(55), None)
            .await
            .unwrap();
        assert_eq!(outcome, Outcome::Published);
        assert_eq!(
            check_id,
            Some(55),
            "the caller's own id rides back unchanged"
        );
        let writes = forge.writes();
        assert!(
            matches!(&writes[1], Write::CheckUpdate { check_id: 55, .. }),
            "an existing check id updates that check rather than creating another: {writes:?}"
        );
    }

    #[tokio::test]
    async fn an_empty_gallery_replaces_an_earlier_comment_with_a_stale_notice() {
        let forge = MockForge::new()
            .with_pull_request(pull_request("def"), vec![], vec![])
            .with_comments(
                7,
                vec![IssueComment {
                    id: Some(41),
                    author: "tinysweeper".into(),
                    body: format!("{}\nold gallery from an earlier head", render::MARKER),
                }],
            );
        let empty = Gallery {
            head_sha: "def".into(),
            run: "run-1".into(),
            files: vec![],
            flows: vec![],
            ..gallery()
        };
        let (outcome, _) = publish(&forge, &forge, "o/r", &empty, None, None)
            .await
            .unwrap();
        assert_eq!(outcome, Outcome::Published);
        let writes = forge.writes();
        let Write::CommentUpdate {
            comment_id: 41,
            body,
        } = &writes[0]
        else {
            panic!("the earlier comment is edited, not left alone: {writes:?}");
        };
        assert!(body.contains("No visible change"));
        assert!(!body.contains("old gallery"));
    }

    #[tokio::test]
    async fn an_empty_gallery_with_no_earlier_comment_stays_quiet() {
        let forge = MockForge::new().with_pull_request(pull_request("def"), vec![], vec![]);
        let empty = Gallery {
            head_sha: "def".into(),
            run: "run-1".into(),
            files: vec![],
            flows: vec![],
            ..gallery()
        };
        let (outcome, _) = publish(&forge, &forge, "o/r", &empty, None, None)
            .await
            .unwrap();
        assert_eq!(outcome, Outcome::NothingToShow);
        assert!(matches!(forge.writes().as_slice(), [Write::Check(_)]));
    }
}
