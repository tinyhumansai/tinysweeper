//! Step 2: has GitHub already got this Sentry issue?
//!
//! A Sentry issue promoted twice is the worst outcome available here, because
//! it is the one that *scales*: every subsequent sweep adds another copy, and
//! nothing in the pipeline notices. So the link between the two systems has to
//! survive a restart, a redeploy and a database wipe, and it has to be
//! readable by a human deciding whether the bot is confused.
//!
//! ## The marker, and why it lives in the issue body
//!
//! ```text
//! <!-- tinysweeper:sentry=<org>/<project>/<short-id> -->
//! ```
//!
//! Stored in the GitHub issue body, the way review findings already carry
//! `<!-- tinysweeper:fp=… -->`. "Is this already tracked?" is then a question
//! asked of GitHub, which is the system that actually holds the answer. A
//! cache in Mongo would be an optimisation on top and must never be the source
//! of truth: if the two disagree GitHub is right, and a lost cache degrades to
//! a slower sweep rather than a duplicate one. There is deliberately no cache
//! in this module for that reason — adding one is a performance change with
//! its own argument to make.
//!
//! ## Search narrows, exact match decides
//!
//! [`find_tracked`] searches for the short id rather than for the whole marker,
//! then confirms the hit by looking for the exact marker substring in the
//! returned body. GitHub's issue search is a tokenised index over an
//! eventually-consistent store: an HTML comment is not reliably searchable as
//! one phrase, and a query that happens to work today is not a guarantee. The
//! short id (`API-1A2B`) is distinctive enough to narrow to a handful of hits,
//! and the substring check is what actually answers the question.
//!
//! The consequence worth stating: a search miss produces a **duplicate**,
//! while a search hit that fails the substring check merely produces a
//! promotion. Both failure modes are one-directional and the expensive one is
//! guarded by the cheaper check, not the other way round.

use crate::error::Result;
use crate::forge::types::{Issue, RepoId};
use crate::ports::forge::ForgeRead;

/// Build the durable marker for one Sentry issue.
///
/// `short_id` must already be scrubbed — everything reaching this function
/// comes off a [`crate::sentry::types::SafeIssue`], so it is.
pub fn marker(org: &str, project: &str, short_id: &str) -> String {
    format!(
        "<!-- {}sentry={org}/{project}/{short_id} -->",
        crate::MARKER_PREFIX
    )
}

/// What GitHub knows about one Sentry issue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Tracked {
    /// No issue carries the marker. Promote it.
    No,
    /// Already tracked by this issue, open or closed.
    Yes(Box<Issue>),
    /// The Sentry issue has no usable short id, so no durable marker can be
    /// built for it. Refuse rather than promote — see [`find_tracked`].
    Undedupable,
}

/// Ask GitHub whether this Sentry issue is already tracked.
///
/// Returns the *first* confirmed match. A repository with two issues carrying
/// the same marker is already in a state a human has to resolve; picking the
/// first keeps the sweep deterministic and stops it adding a third.
///
/// An empty short id yields [`Tracked::Undedupable`] rather than
/// [`Tracked::No`]. Promoting it would open an issue whose marker names
/// nothing, which can never match on a later sweep — so it would be recreated
/// every run, forever. That is the duplicate-at-scale failure this module
/// exists to prevent, arriving by a different door.
pub async fn find_tracked(
    read: &dyn ForgeRead,
    repo: &RepoId,
    org: &str,
    project: &str,
    short_id: &str,
) -> Result<Tracked> {
    if short_id.trim().is_empty() {
        tracing::warn!(
            project = %project,
            "sentry issue has no short id; skipping rather than promoting something that could never be deduplicated"
        );
        return Ok(Tracked::Undedupable);
    }

    let needle = marker(org, project, short_id);
    let hits = read.search_issues(repo, short_id).await?;

    let Some(indexed) = hits.into_iter().find(|issue| issue.body.contains(&needle)) else {
        return Ok(Tracked::No);
    };

    // Re-read the issue before anyone trusts its state.
    //
    // Search is an eventually-consistent index, so `indexed.open` is whatever
    // the index last recorded — and the caller uses it to decide whether to
    // resolve the Sentry issue. A tracking issue that was closed and then
    // reopened still reads as closed here for as long as the index lags, and
    // resolving on that stale answer marks a live error fixed. The extra read
    // is one API call on the already-tracked path, which is the cheap path.
    //
    // A failed re-read falls back to the indexed copy rather than failing the
    // sweep: dedupe itself only needs the number, which does not go stale, and
    // refusing to dedupe would promote a duplicate — the worse outcome.
    match read.issue(repo, indexed.number).await {
        Ok(fresh) => Ok(Tracked::Yes(Box::new(fresh))),
        Err(err) => {
            tracing::warn!(
                number = indexed.number,
                error = %err,
                "could not re-read the tracking issue; using the indexed copy, whose open/closed \
                 state may be stale"
            );
            Ok(Tracked::Yes(Box::new(indexed)))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::forge::mock::MockForge;

    fn repo() -> RepoId {
        RepoId {
            owner: "acme".into(),
            name: "api".into(),
        }
    }

    fn tracking_issue(number: u64, body: &str, open: bool) -> Issue {
        Issue {
            number,
            title: "PaymentError".into(),
            body: body.into(),
            author: "tinysweeper".into(),
            open,
            ..Issue::default()
        }
    }

    #[test]
    fn the_marker_is_stable_and_carries_the_crate_prefix() {
        assert_eq!(
            marker("acme", "api", "API-1A2B"),
            "<!-- tinysweeper:sentry=acme/api/API-1A2B -->"
        );
    }

    #[tokio::test]
    async fn an_untracked_issue_is_not_found() {
        let forge = MockForge::new();
        let found = find_tracked(&forge, &repo(), "acme", "api", "API-1A2B")
            .await
            .expect("ok");
        assert_eq!(found, Tracked::No);
    }

    #[tokio::test]
    async fn a_tracked_issue_is_found_by_its_marker() {
        let body = format!("Seen 500 times.\n\n{}", marker("acme", "api", "API-1A2B"));
        let forge = MockForge::new().with_issue(tracking_issue(12, &body, true));

        let Tracked::Yes(found) = find_tracked(&forge, &repo(), "acme", "api", "API-1A2B")
            .await
            .expect("ok")
        else {
            panic!("expected a tracked issue");
        };
        assert_eq!(found.number, 12);
    }

    /// The acceptance criterion that stops a fixed issue being reopened by the
    /// next sweep.
    #[tokio::test]
    async fn a_closed_tracked_issue_still_counts_as_tracked() {
        let body = format!("Fixed.\n\n{}", marker("acme", "api", "API-1A2B"));
        let forge = MockForge::new().with_issue(tracking_issue(12, &body, false));

        let Tracked::Yes(found) = find_tracked(&forge, &repo(), "acme", "api", "API-1A2B")
            .await
            .expect("ok")
        else {
            panic!("a closed tracked issue is still tracked");
        };
        assert_eq!(found.number, 12);
        assert!(!found.open);
    }

    /// A search hit whose body does not actually carry the marker is not a
    /// match — the substring check is what decides, not the index.
    #[tokio::test]
    async fn a_search_hit_without_the_marker_is_rejected() {
        let forge = MockForge::new().with_issue(tracking_issue(
            12,
            "Someone mentioned API-1A2B in prose, with no marker.",
            true,
        ));

        let found = find_tracked(&forge, &repo(), "acme", "api", "API-1A2B")
            .await
            .expect("ok");
        assert_eq!(found, Tracked::No, "prose must not read as a marker");
    }

    /// Markers are project-scoped: the same short id under a different project
    /// is a different Sentry issue.
    #[tokio::test]
    async fn a_marker_for_another_project_does_not_match() {
        let body = marker("acme", "web", "API-1A2B");
        let forge = MockForge::new().with_issue(tracking_issue(12, &body, true));

        let found = find_tracked(&forge, &repo(), "acme", "api", "API-1A2B")
            .await
            .expect("ok");
        assert_eq!(found, Tracked::No);
    }

    #[tokio::test]
    async fn an_issue_with_no_short_id_is_refused_rather_than_promoted() {
        let forge = MockForge::new();
        let found = find_tracked(&forge, &repo(), "acme", "api", "  ")
            .await
            .expect("ok");
        assert_eq!(found, Tracked::Undedupable);
    }
}
