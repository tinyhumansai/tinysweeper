//! TinySweeper-owned read-only reviewer tools over a bounded repository host.
//!
//! Query validation, redaction and untrusted-data envelopes belong to this host,
//! while OpenHuman Embed supplies only the neutral tool and agent contracts.

mod query;
mod tool;

use std::sync::Arc;

pub use query::RepositoryQuery;
use tool::RepositoryTool;

/// Trusted repository data source and secret-redaction boundary.
///
/// Implementations must enforce repository scope (including symlinks), snapshot
/// identity, permissions and resource bounds, and never execute contributor code.
/// The tools validate model arguments but cannot sandbox the host implementation.
#[async_trait::async_trait]
pub trait RepositoryHost: Send + Sync {
    /// Read tree entries, file ranges, literal search results, symbols or git data.
    ///
    /// Queries reaching this method from [`repository_tools`] are validated.
    /// Returned repository data is treated as untrusted, never as instructions.
    /// Error diagnostics are withheld from the model, so they may contain host context.
    async fn query(&self, query: RepositoryQuery) -> anyhow::Result<String>;

    /// Remove secrets from all query output before it enters a tool result.
    ///
    /// This is mandatory even for pre-sanitized data sources; only those sources
    /// should use an identity implementation. Failure withholds the entire result.
    /// Redact before truncation so a truncated credential cannot evade detection.
    async fn redact(&self, content: String) -> anyhow::Result<String>;
}

/// Construct the five directly advertisable host-backed repository tools.
///
/// Tool names are `repo_list`, `repo_read`, `repo_search`, `repo_lookup`, and
/// `repo_git_show`. They only call [`RepositoryHost::query`] and
/// [`RepositoryHost::redact`]. Results are bounded, fenced as untrusted JSON,
/// and never include host error diagnostics. Use with `ToolScopeSpec::HostOnly`
/// and `Access::readonly()` for untrusted review turns.
pub fn repository_tools(host: Arc<dyn RepositoryHost>) -> Vec<Box<dyn openhuman_embed::Tool>> {
    ["list", "read", "search", "lookup", "git_show"]
        .into_iter()
        .map(|operation| {
            Box::new(RepositoryTool {
                operation,
                host: host.clone(),
            }) as Box<dyn openhuman_embed::Tool>
        })
        .collect()
}

#[cfg(test)]
mod runtime_test;
#[cfg(test)]
mod test;
