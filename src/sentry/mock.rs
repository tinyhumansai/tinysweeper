//! An in-memory Sentry that records every write.
//!
//! Like [`crate::forge::mock::MockForge`], this is not a stub: it is what the
//! whole Sentry test suite runs against and what `--dry-run` renders from. It
//! records rather than discards, so a test can assert on the exact annotation
//! text and the exact set of issues a sweep resolved — which is how the
//! close-the-loop rules stay honest.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use crate::error::{Error, Result};
use crate::ports::sentry::SentryApi;
use crate::sentry::types::{RawEvent, RawIssue};

/// One thing the mock was asked to write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SentryWrite {
    /// A comment was left on an issue.
    Annotation {
        /// The Sentry issue id.
        issue_id: String,
        /// The comment body.
        text: String,
    },
    /// An issue was resolved.
    Resolved {
        /// The Sentry issue id.
        issue_id: String,
    },
}

/// The seeded state a [`MockSentry`] answers from.
#[derive(Debug, Clone, Default)]
pub struct MockSentryState {
    /// Unresolved issues, keyed by project slug.
    pub issues: BTreeMap<String, Vec<RawIssue>>,
    /// Latest events, keyed by Sentry issue id.
    pub events: BTreeMap<String, RawEvent>,
}

/// An offline Sentry.
#[derive(Debug, Clone, Default)]
pub struct MockSentry {
    state: MockSentryState,
    writes: Arc<Mutex<Vec<SentryWrite>>>,
    /// When set, every method fails with this message. Exercises the paths
    /// where Sentry is down and the sweep must degrade rather than abort.
    fails_with: Option<String>,
}

impl MockSentry {
    /// An empty Sentry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Seed the unresolved issues of one project.
    pub fn with_issues(mut self, project: &str, issues: Vec<RawIssue>) -> Self {
        self.state.issues.insert(project.to_string(), issues);
        self
    }

    /// Seed the latest event for one issue id.
    pub fn with_event(mut self, issue_id: &str, event: RawEvent) -> Self {
        self.state.events.insert(issue_id.to_string(), event);
        self
    }

    /// Make every call fail, to exercise degradation.
    pub fn failing(mut self, message: &str) -> Self {
        self.fails_with = Some(message.to_string());
        self
    }

    /// Everything the mock was asked to write, in order.
    pub fn writes(&self) -> Vec<SentryWrite> {
        self.writes.lock().expect("writes lock").clone()
    }

    /// Whether nothing was written. The assertion most tests actually want.
    pub fn wrote_nothing(&self) -> bool {
        self.writes().is_empty()
    }

    fn guard(&self) -> Result<()> {
        match &self.fails_with {
            Some(message) => Err(Error::Forge(format!("sentry: {message}"))),
            None => Ok(()),
        }
    }
}

#[async_trait]
impl SentryApi for MockSentry {
    async fn unresolved_issues(&self, project: &str, limit: usize) -> Result<Vec<RawIssue>> {
        self.guard()?;
        Ok(self
            .state
            .issues
            .get(project)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .take(limit)
            .collect())
    }

    async fn latest_event(&self, issue_id: &str) -> Result<Option<RawEvent>> {
        self.guard()?;
        Ok(self.state.events.get(issue_id).cloned())
    }

    async fn annotate(&self, issue_id: &str, text: &str) -> Result<()> {
        self.guard()?;
        self.writes
            .lock()
            .expect("writes lock")
            .push(SentryWrite::Annotation {
                issue_id: issue_id.to_string(),
                text: text.to_string(),
            });
        Ok(())
    }

    async fn resolve(&self, issue_id: &str) -> Result<()> {
        self.guard()?;
        self.writes
            .lock()
            .expect("writes lock")
            .push(SentryWrite::Resolved {
                issue_id: issue_id.to_string(),
            });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn issue(short_id: &str) -> RawIssue {
        RawIssue {
            short_id: short_id.to_string(),
            ..RawIssue::default()
        }
    }

    #[tokio::test]
    async fn it_answers_seeded_issues_and_honours_the_limit() {
        let sentry = MockSentry::new().with_issues("api", vec![issue("A-1"), issue("A-2")]);

        let all = sentry.unresolved_issues("api", 10).await.expect("ok");
        assert_eq!(all.len(), 2);

        let capped = sentry.unresolved_issues("api", 1).await.expect("ok");
        assert_eq!(capped.len(), 1);
        assert_eq!(capped[0].short_id, "A-1");
    }

    #[tokio::test]
    async fn an_unknown_project_is_empty_rather_than_an_error() {
        let sentry = MockSentry::new();
        assert!(
            sentry
                .unresolved_issues("nope", 10)
                .await
                .expect("ok")
                .is_empty()
        );
    }

    #[tokio::test]
    async fn writes_are_recorded_in_order() {
        let sentry = MockSentry::new();
        sentry.annotate("4711", "tracked in #12").await.expect("ok");
        sentry.resolve("4711").await.expect("ok");

        assert_eq!(
            sentry.writes(),
            vec![
                SentryWrite::Annotation {
                    issue_id: "4711".to_string(),
                    text: "tracked in #12".to_string(),
                },
                SentryWrite::Resolved {
                    issue_id: "4711".to_string(),
                },
            ]
        );
    }

    #[tokio::test]
    async fn a_failing_sentry_writes_nothing() {
        let sentry = MockSentry::new().failing("503");
        assert!(sentry.unresolved_issues("api", 10).await.is_err());
        assert!(sentry.annotate("1", "x").await.is_err());
        assert!(sentry.wrote_nothing());
    }
}
