//! Bounded request bridge from static Embed tools into a borrowed tree reader.

use crate::config::types::LookupPolicy;
use crate::flows::review_tree::{render, truncate_chars};
use crate::ports::tree::{Found, Lookup, MAX_READ_LINES, MAX_SEARCH_HITS, TreeQuery, TreeReader};
use async_trait::async_trait;
use openhuman_embed::repository::{RepositoryHost, RepositoryQuery};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use tokio::sync::{mpsc, oneshot};

type Reply = std::result::Result<String, String>;
pub(super) struct Query {
    request: RepositoryQuery,
    reply: oneshot::Sender<Reply>,
}
struct Bridge(mpsc::Sender<Query>);
#[async_trait]
impl RepositoryHost for Bridge {
    async fn query(&self, request: RepositoryQuery) -> anyhow::Result<String> {
        let (reply, received) = oneshot::channel();
        self.0
            .send(Query { request, reply })
            .await
            .map_err(|_| anyhow::anyhow!("review ended"))?;
        received
            .await
            .map_err(|_| anyhow::anyhow!("review ended"))?
            .map_err(anyhow::Error::msg)
    }
    async fn redact(&self, text: String) -> anyhow::Result<String> {
        // Dispatch has already scrubbed numbered source and sensitive paths.
        // Keep the mandatory host redaction seam active for any future result.
        Ok(crate::evidence::redact::scrub_rendered(&text))
    }
}
pub(super) fn channel() -> (
    Arc<dyn RepositoryHost>,
    mpsc::Receiver<Query>,
    Arc<AtomicUsize>,
) {
    let (sent, received) = mpsc::channel(4);
    (
        Arc::new(Bridge(sent)),
        received,
        Arc::new(AtomicUsize::new(0)),
    )
}

/// Drive only host repository requests; Embed owns the model/tool turn loop.
pub(super) async fn dispatch(
    tree: &dyn TreeReader,
    policy: &LookupPolicy,
    mut received: mpsc::Receiver<Query>,
    successful: Arc<AtomicUsize>,
) {
    let redacted = crate::ports::tree::RedactingTree::new(tree);
    let tree: &dyn TreeReader = &redacted;
    let max_queries = usize::from(policy.rounds) * usize::from(policy.per_round);
    let mut queries = 0;
    let mut chars = 0;
    enum HostLookup {
        Source(Lookup),
        Explore(TreeQuery),
    }
    while let Some(Query { request, reply }) = received.recv().await {
        let hit_limit = match &request {
            RepositoryQuery::Search { limit, .. } | RepositoryQuery::Lookup { limit, .. } => {
                (*limit as usize).min(MAX_SEARCH_HITS)
            }
            RepositoryQuery::List { limit, .. } => *limit as usize,
            _ => MAX_SEARCH_HITS,
        };
        let lookup = match request {
            RepositoryQuery::Read {
                path,
                start_line,
                end_line,
            } => Some(HostLookup::Source(Lookup::Read {
                path,
                start: Some(start_line),
                end: Some(end_line.min(start_line.saturating_add(MAX_READ_LINES - 1))),
            })),
            RepositoryQuery::Search { path, query, .. } => {
                Some(HostLookup::Source(Lookup::Search {
                    pattern: query,
                    glob: (path != ".").then(|| format!("{path}/**")),
                }))
            }
            RepositoryQuery::List { path, limit } => {
                Some(HostLookup::Explore(TreeQuery::List { path, limit }))
            }
            RepositoryQuery::Lookup { symbol, limit } => {
                Some(HostLookup::Explore(TreeQuery::Symbol { symbol, limit }))
            }
            RepositoryQuery::GitShow {
                commit,
                path,
                start_line,
                end_line,
            } => Some(HostLookup::Explore(TreeQuery::History {
                commit,
                path,
                start: start_line,
                end: end_line.min(start_line.saturating_add(MAX_READ_LINES - 1)),
            })),
        };
        let result = if let Some(lookup) = lookup {
            if !policy.enabled || queries >= max_queries || chars >= policy.max_chars {
                Err("repository lookup budget exhausted".into())
            } else {
                queries += 1;
                let found = match &lookup {
                    HostLookup::Source(lookup) => tree.lookup(lookup).await,
                    HostLookup::Explore(query) => tree.explore(query).await,
                };
                match found {
                    Ok(mut found @ (Found::Text { .. } | Found::Hits { .. } | Found::NotFound)) => {
                        if let Found::Hits {
                            hits, truncated, ..
                        } = &mut found
                            && hits.len() > hit_limit
                        {
                            *truncated = true;
                            hits.truncate(hit_limit);
                        }
                        let display = match &lookup {
                            HostLookup::Source(lookup) => lookup.clone(),
                            HostLookup::Explore(TreeQuery::History {
                                path, start, end, ..
                            }) => Lookup::Read {
                                path: path.clone(),
                                start: Some(*start),
                                end: Some(*end),
                            },
                            _ => Lookup::Search {
                                pattern: "repository exploration".into(),
                                glob: None,
                            },
                        };
                        let mut text = render(&display, &found);
                        if let HostLookup::Explore(TreeQuery::History { commit, .. }) = &lookup {
                            text.insert_str(0, &format!("Historical snapshot {commit}:\n"));
                        }
                        truncate_chars(&mut text, policy.max_chars.saturating_sub(chars));
                        chars += text.chars().count();
                        Ok(text)
                    }
                    _ => Err("repository lookup unavailable".into()),
                }
            }
        } else {
            Err("repository operation unavailable on this snapshot".into())
        };
        if result.is_ok() {
            successful.fetch_add(1, Ordering::Relaxed);
        }
        let _ = reply.send(result);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ports::tree::MockTree;

    #[tokio::test]
    async fn ranges_starting_inside_private_keys_use_the_existing_prefix_redactor() {
        let tree = MockTree::from_files([(
            "src/config.rs",
            "-----BEGIN RSA PRIVATE KEY-----\nopaqueprivatebody\n-----END RSA PRIVATE KEY-----",
        )]);
        let policy = LookupPolicy::default();
        let (host, received, successful) = channel();
        let dispatched = dispatch(&tree, &policy, received, successful);
        let queried = async {
            host.query(RepositoryQuery::Read {
                path: "src/config.rs".into(),
                start_line: 2,
                end_line: 2,
            })
            .await
            .unwrap()
        };
        let text = tokio::select! { text = queried => text, () = dispatched => panic!("bridge closed unexpectedly") };
        assert!(!text.contains("opaqueprivatebody"));
    }

    #[tokio::test]
    async fn queries_share_host_limits_and_scrub_before_character_truncation() {
        let key = format!("AKIA{}", "IOSFODNN7EXAMPLE");
        let tree = MockTree::from_files([("config.rs", format!("KEY=\"{key}\";\nordinary"))]);
        let policy = LookupPolicy {
            rounds: 1,
            per_round: 2,
            max_chars: 30,
            ..LookupPolicy::default()
        };
        let (host, received, successful) = channel();
        let dispatched = dispatch(&tree, &policy, received, successful.clone());
        let queried = async {
            let unsupported = host
                .query(RepositoryQuery::GitShow {
                    commit: "a".repeat(40),
                    path: "config.rs".into(),
                    start_line: 1,
                    end_line: 1,
                })
                .await;
            assert!(unsupported.is_err());
            let first = host
                .query(RepositoryQuery::Read {
                    path: "config.rs".into(),
                    start_line: 1,
                    end_line: 2,
                })
                .await
                .unwrap();
            assert!(!first.contains(&key));
            assert!(first.chars().count() <= 30);
            assert!(
                !first.contains("AKIA"),
                "redact before truncating a credential"
            );
            assert!(
                host.query(RepositoryQuery::Read {
                    path: "config.rs".into(),
                    start_line: 1,
                    end_line: 2
                })
                .await
                .is_err()
            );
        };
        tokio::select! { () = queried => {}, () = dispatched => panic!("bridge closed unexpectedly") }
        assert_eq!(successful.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn sensitive_path_values_never_enter_tool_results() {
        // Use an explicit host to avoid relying on a mock's path refusal.
        struct Sensitive;
        #[async_trait]
        impl TreeReader for Sensitive {
            async fn lookup(&self, _: &Lookup) -> crate::error::Result<Found> {
                Ok(Found::Text {
                    text: "PASSWORD=opaque-password".into(),
                    start: 1,
                    end: 1,
                    total: 1,
                })
            }
            fn describe(&self) -> String {
                "test".into()
            }
        }
        let (host, received, successful) = channel();
        let policy = LookupPolicy::default();
        let dispatched = dispatch(&Sensitive, &policy, received, successful);
        let queried = async {
            host.query(RepositoryQuery::Read {
                path: ".env".into(),
                start_line: 1,
                end_line: 1,
            })
            .await
        };
        let text = tokio::select! { text = queried => text, () = dispatched => panic!("bridge closed unexpectedly") };
        assert!(
            text.is_err(),
            "sensitive files are refused before host reads"
        );
    }
}
