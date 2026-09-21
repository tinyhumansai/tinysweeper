//! The write boundary for MCP issue creation.
//!
//! This module makes no policy decisions. It receives an immutable plan after
//! organisation scoping, duplicate detection, template selection and code
//! enrichment have completed, mints the short-lived installation credential,
//! and performs exactly one issue creation.

use crate::error::Result;
use crate::forge::RepoId;
use crate::ports::forge::ForgeWrite;
use crate::server::auth::AppAuth;

/// A fully decided issue creation.
pub struct IssuePlan {
    /// Repository already canonicalised and authorised by the planner.
    pub repo: RepoId,
    /// GitHub App installation covering the repository.
    pub installation: u64,
    /// Final issue title.
    pub title: String,
    /// Final template-aware, enriched body.
    pub body: String,
    /// Labels requested by the authenticated caller.
    pub labels: Vec<String>,
}

/// Mint the write credential and execute one previously decided plan.
pub async fn apply(auth: &AppAuth, plan: &IssuePlan) -> Result<u64> {
    let token = auth.installation_token(plan.installation).await?;
    let write = crate::forge::github::GitHubWrite::new(&token)?;
    execute(&write, plan).await
}

async fn execute(write: &dyn ForgeWrite, plan: &IssuePlan) -> Result<u64> {
    write
        .create_issue(&plan.repo, &plan.title, &plan.body, &plan.labels)
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::forge::mock::{MockForge, Write};

    #[tokio::test]
    async fn applying_a_plan_performs_exactly_its_issue_write() {
        let forge = MockForge::new();
        let plan = IssuePlan {
            repo: RepoId::parse("acme/widget").unwrap(),
            installation: 7,
            title: "Parser can loop".into(),
            body: "Reproduction and code context.".into(),
            labels: vec!["bug".into()],
        };

        let number = execute(&forge, &plan).await.expect("applies");

        assert_eq!(number, 1);
        assert_eq!(
            forge.writes(),
            vec![Write::IssueCreated {
                title: plan.title,
                body: plan.body,
                labels: plan.labels,
            }]
        );
    }
}
