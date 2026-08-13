//! Step 4: close the loop back into Sentry.
//!
//! Two writes, both gated by their own config flag, both narrow:
//!
//! - [`annotate`] comments the GitHub URL onto the Sentry issue, so somebody
//!   triaging in Sentry can see it is tracked without searching GitHub.
//! - [`resolve_if_fixed`] marks the Sentry issue resolved once the GitHub
//!   issue that tracks it is closed.
//!
//! ## Which trigger resolves a Sentry issue
//!
//! The specification carries two readings of `resolve_when_tracked`, and they
//! behave differently:
//!
//! - The config field's own doc comment says "resolve the Sentry issue in the
//!   next release **once it is tracked**", which would resolve at promotion
//!   time.
//! - #90's step 4 and #91's acceptance criterion say "**when the GitHub issue
//!   is closed**, the Sentry issue should be resolved too — that is the
//!   'close' half and is the reason to bother".
//!
//! This module implements the second: resolve only when the flag is on **and**
//! the tracking GitHub issue is closed. Resolving at promotion time would mark
//! an error fixed the moment somebody noticed it, which is precisely backwards
//! — the Sentry issue would disappear from the unresolved list while the bug
//! is still in production, and the next occurrence would have to reopen it.
//! The conservative reading is also the reversible one: a Sentry issue left
//! unresolved costs a line in a list, while one resolved early hides a live
//! error.
//!
//! ## Nothing here closes a GitHub issue
//!
//! Deliberately out of scope, per #90. Closing someone's issue is the most
//! expensive mistake this bot can make; a Sentry event count is weaker
//! evidence than `issues.close` already demands. Traffic is one-way: GitHub's
//! state drives Sentry, never the reverse.

use crate::config::types::Sentry;
use crate::error::Result;
use crate::forge::types::{Issue, RepoId};
use crate::ports::sentry::SentryApi;

/// The canonical web URL of a GitHub issue.
pub fn issue_url(repo: &RepoId, number: u64) -> String {
    format!(
        "https://github.com/{}/{}/issues/{number}",
        repo.owner, repo.name
    )
}

/// Comment the tracking GitHub issue's URL onto the Sentry issue.
///
/// A no-op when `annotate_sentry` is off. Failure is reported to the caller
/// rather than swallowed, but the caller treats it as non-fatal: the promotion
/// has already happened and the marker in the GitHub issue is the durable half
/// of the link. Losing the annotation costs navigability, not correctness.
pub async fn annotate(
    sentry: &dyn SentryApi,
    config: &Sentry,
    sentry_issue_id: &str,
    repo: &RepoId,
    number: u64,
) -> Result<bool> {
    if !config.annotate_sentry {
        return Ok(false);
    }

    let url = issue_url(repo, number);
    sentry
        .annotate(
            sentry_issue_id,
            &format!("Tracked in {url} (opened automatically by tinysweeper)."),
        )
        .await?;

    Ok(true)
}

/// Resolve the Sentry issue when the GitHub issue tracking it is closed.
///
/// Returns whether anything was written. Both conditions are required and are
/// checked here rather than at the call site, so there is one place the rule
/// lives and one place to read it.
pub async fn resolve_if_fixed(
    sentry: &dyn SentryApi,
    config: &Sentry,
    sentry_issue_id: &str,
    tracking: &Issue,
) -> Result<bool> {
    if !config.resolve_when_tracked {
        return Ok(false);
    }
    // The load-bearing half: tracked-and-still-open is not fixed.
    if tracking.open {
        return Ok(false);
    }

    sentry.resolve(sentry_issue_id).await?;
    tracing::info!(
        sentry_issue = %sentry_issue_id,
        github_issue = tracking.number,
        "resolved a sentry issue whose tracking issue is closed"
    );

    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sentry::mock::{MockSentry, SentryWrite};

    fn repo() -> RepoId {
        RepoId {
            owner: "acme".into(),
            name: "api".into(),
        }
    }

    fn tracking(number: u64, open: bool) -> Issue {
        Issue {
            number,
            open,
            ..Issue::default()
        }
    }

    fn config(annotate: bool, resolve: bool) -> Sentry {
        Sentry {
            annotate_sentry: annotate,
            resolve_when_tracked: resolve,
            ..Sentry::default()
        }
    }

    #[test]
    fn the_issue_url_is_the_canonical_one() {
        assert_eq!(
            issue_url(&repo(), 12),
            "https://github.com/acme/api/issues/12"
        );
    }

    #[tokio::test]
    async fn annotation_writes_the_github_url() {
        let sentry = MockSentry::new();
        let wrote = annotate(&sentry, &config(true, false), "4711", &repo(), 12)
            .await
            .expect("ok");

        assert!(wrote);
        let SentryWrite::Annotation { issue_id, text } = &sentry.writes()[0] else {
            panic!("expected an annotation");
        };
        assert_eq!(issue_id, "4711");
        assert!(
            text.contains("https://github.com/acme/api/issues/12"),
            "{text}"
        );
    }

    #[tokio::test]
    async fn annotation_is_off_when_the_flag_is_off() {
        let sentry = MockSentry::new();
        let wrote = annotate(&sentry, &config(false, false), "4711", &repo(), 12)
            .await
            .expect("ok");

        assert!(!wrote);
        assert!(sentry.wrote_nothing());
    }

    /// The acceptance criterion: closing the GitHub issue resolves Sentry.
    #[tokio::test]
    async fn a_closed_tracking_issue_resolves_sentry() {
        let sentry = MockSentry::new();
        let wrote = resolve_if_fixed(&sentry, &config(false, true), "4711", &tracking(12, false))
            .await
            .expect("ok");

        assert!(wrote);
        assert_eq!(
            sentry.writes(),
            vec![SentryWrite::Resolved {
                issue_id: "4711".into()
            }]
        );
    }

    /// The half that stops a live error being hidden: tracked is not fixed.
    #[tokio::test]
    async fn an_open_tracking_issue_resolves_nothing() {
        let sentry = MockSentry::new();
        let wrote = resolve_if_fixed(&sentry, &config(false, true), "4711", &tracking(12, true))
            .await
            .expect("ok");

        assert!(!wrote);
        assert!(
            sentry.wrote_nothing(),
            "an open tracking issue must not resolve its sentry issue"
        );
    }

    #[tokio::test]
    async fn nothing_resolves_when_the_flag_is_off() {
        let sentry = MockSentry::new();
        let wrote = resolve_if_fixed(&sentry, &config(false, false), "4711", &tracking(12, false))
            .await
            .expect("ok");

        assert!(!wrote);
        assert!(sentry.wrote_nothing());
    }
}
