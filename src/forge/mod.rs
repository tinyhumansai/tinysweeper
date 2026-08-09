//! Forge adapters and the domain types they speak in.
//!
//! [`mock::MockForge`] is always compiled: it backs the entire test suite and
//! `--dry-run`. The real GitHub adapter arrives behind the `github` feature.

#[cfg(feature = "github")]
pub mod github;
pub mod mock;
pub mod types;

pub use crate::forge::mock::{MockForge, MockState, Write};
pub use crate::forge::types::{
    ChangedFile, CheckConclusion, CheckRun, CheckStatus, Commit, FileStatus, Issue, IssueComment,
    PullRequest, PullRequestContext, RepoId, ReviewComment, ReviewEvent, ReviewVerdict,
};
