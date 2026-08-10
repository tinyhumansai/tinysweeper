//! Step 1: which unresolved issues clear the bar, and what the cap truncated.
//!
//! Pure — no forge, no Sentry, no clock — so every gate is testable on its own,
//! the same shape as [`crate::issues::close::decide`].
//!
//! ## The cap is applied last, and separately
//!
//! [`filter`] and [`apply_cap`] are two functions rather than one because
//! deduplication runs *between* them. Capping before the GitHub search would
//! let ten already-tracked issues consume the whole `max_per_run` budget and
//! promote nothing, which reads as "nothing qualified" and is indistinguishable
//! from a healthy quiet sweep. Capping after it means the cap counts issues
//! that would actually be created, which is what the setting is for.
//!
//! That ordering is load-bearing. `sweep` documents it and
//! `the_cap_counts_promotable_issues_not_tracked_ones` pins it.

use globset::{Glob, GlobMatcher};

use crate::config::types::Sentry;
use crate::sentry::types::{RawIssue, Skipped};

/// One issue and why it was dropped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rejected {
    /// The Sentry short id, for the log line.
    pub short_id: String,
    /// Why it did not qualify.
    pub reason: Skipped,
}

/// What [`filter`] decided.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Selection {
    /// Issues that cleared every gate, in the order Sentry returned them.
    pub selected: Vec<RawIssue>,
    /// Issues that did not, each with its reason.
    pub rejected: Vec<Rejected>,
}

/// Apply `min_events`, `min_users` and `ignore_culprits`.
///
/// An issue is tested against the gates in that order and reported with the
/// *first* reason it failed, not every reason: an operator raising
/// `min_events` wants to know the threshold bit, not that the issue also has
/// too few users.
///
/// A malformed glob in `ignore_culprits` is skipped rather than failing the
/// sweep — `config::validate` already reports it as a problem, and a
/// second-guessing hard failure here would take a whole deployment down for a
/// typo validation has already surfaced.
pub fn filter(issues: Vec<RawIssue>, config: &Sentry) -> Selection {
    let matchers = culprit_matchers(config);
    let mut out = Selection::default();

    for issue in issues {
        if issue.count < config.min_events {
            out.rejected.push(Rejected {
                short_id: issue.short_id.clone(),
                reason: Skipped::TooFewEvents {
                    events: issue.count,
                    required: config.min_events,
                },
            });
            continue;
        }

        if issue.user_count < config.min_users {
            out.rejected.push(Rejected {
                short_id: issue.short_id.clone(),
                reason: Skipped::TooFewUsers {
                    users: issue.user_count,
                    required: config.min_users,
                },
            });
            continue;
        }

        let culprit = issue.culprit.as_deref().unwrap_or_default();
        if let Some((pattern, _)) = matchers
            .iter()
            .find(|(_, matcher)| matcher.is_match(culprit))
        {
            out.rejected.push(Rejected {
                short_id: issue.short_id.clone(),
                reason: Skipped::IgnoredCulprit {
                    pattern: pattern.clone(),
                },
            });
            continue;
        }

        out.selected.push(issue);
    }

    out
}

/// Truncate to `max_per_run`, reporting what was cut.
///
/// The returned rejections are what makes the cap *loud*: a truncated sweep
/// that reports nothing reads exactly like a complete one, and the operator
/// finds out only when the backlog stops shrinking.
pub fn apply_cap(issues: Vec<RawIssue>, config: &Sentry) -> Selection {
    if issues.len() <= config.max_per_run {
        return Selection {
            selected: issues,
            rejected: Vec::new(),
        };
    }

    let cap = config.max_per_run;
    let mut issues = issues;
    let overflow = issues.split_off(cap);

    Selection {
        selected: issues,
        rejected: overflow
            .into_iter()
            .map(|issue| Rejected {
                short_id: issue.short_id,
                reason: Skipped::OverCap { cap },
            })
            .collect(),
    }
}

/// Compile the `ignore_culprits` globs, dropping any that will not compile.
fn culprit_matchers(config: &Sentry) -> Vec<(String, GlobMatcher)> {
    config
        .ignore_culprits
        .iter()
        .filter_map(|pattern| match Glob::new(pattern) {
            Ok(glob) => Some((pattern.clone(), glob.compile_matcher())),
            Err(err) => {
                tracing::warn!(
                    pattern = %pattern,
                    error = %err,
                    "ignoring invalid sentry.ignore_culprits glob"
                );
                None
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> Sentry {
        Sentry {
            min_events: 10,
            min_users: 1,
            max_per_run: 3,
            ..Sentry::default()
        }
    }

    fn issue(short_id: &str, count: u64, users: u64, culprit: &str) -> RawIssue {
        RawIssue {
            short_id: short_id.to_string(),
            count,
            user_count: users,
            culprit: Some(culprit.to_string()),
            ..RawIssue::default()
        }
    }

    #[test]
    fn an_issue_clearing_every_gate_is_selected() {
        let out = filter(vec![issue("A-1", 50, 5, "app/handler")], &config());
        assert_eq!(out.selected.len(), 1);
        assert!(out.rejected.is_empty());
    }

    #[test]
    fn too_few_events_is_reported_with_the_threshold() {
        let out = filter(vec![issue("A-1", 3, 5, "app/handler")], &config());
        assert!(out.selected.is_empty());
        assert_eq!(
            out.rejected[0].reason,
            Skipped::TooFewEvents {
                events: 3,
                required: 10
            }
        );
    }

    #[test]
    fn too_few_users_is_reported_with_the_threshold() {
        let out = filter(vec![issue("A-1", 50, 0, "app/handler")], &config());
        assert_eq!(
            out.rejected[0].reason,
            Skipped::TooFewUsers {
                users: 0,
                required: 1
            }
        );
    }

    /// The first failing gate is the reported one — an issue failing two gates
    /// should not produce two log lines about the same issue.
    #[test]
    fn only_the_first_failed_gate_is_reported() {
        let out = filter(vec![issue("A-1", 1, 0, "app/handler")], &config());
        assert_eq!(out.rejected.len(), 1);
        assert!(matches!(
            out.rejected[0].reason,
            Skipped::TooFewEvents { .. }
        ));
    }

    #[test]
    fn an_ignored_culprit_names_the_glob_that_matched() {
        let config = Sentry {
            ignore_culprits: vec!["vendor/**".into(), "**/node_modules/**".into()],
            ..config()
        };
        let out = filter(vec![issue("A-1", 50, 5, "vendor/lib/thing.rs")], &config);
        assert_eq!(
            out.rejected[0].reason,
            Skipped::IgnoredCulprit {
                pattern: "vendor/**".into()
            }
        );
    }

    #[test]
    fn an_invalid_glob_is_skipped_rather_than_failing_the_sweep() {
        let config = Sentry {
            ignore_culprits: vec!["[".into(), "vendor/**".into()],
            ..config()
        };
        let out = filter(vec![issue("A-1", 50, 5, "app/handler")], &config);
        assert_eq!(out.selected.len(), 1, "the sweep still ran");
    }

    #[test]
    fn an_issue_with_no_culprit_is_not_matched_by_a_glob() {
        let config = Sentry {
            ignore_culprits: vec!["vendor/**".into()],
            ..config()
        };
        let raw = RawIssue {
            short_id: "A-1".into(),
            count: 50,
            user_count: 5,
            culprit: None,
            ..RawIssue::default()
        };
        assert_eq!(filter(vec![raw], &config).selected.len(), 1);
    }

    #[test]
    fn the_cap_truncates_and_reports_every_dropped_issue() {
        let issues = (0..5)
            .map(|n| issue(&format!("A-{n}"), 50, 5, "app"))
            .collect();
        let out = apply_cap(issues, &config());

        assert_eq!(out.selected.len(), 3);
        assert_eq!(out.rejected.len(), 2);
        assert_eq!(out.rejected[0].short_id, "A-3");
        assert!(
            out.rejected
                .iter()
                .all(|r| r.reason == Skipped::OverCap { cap: 3 })
        );
    }

    #[test]
    fn a_sweep_under_the_cap_reports_no_truncation() {
        let issues = vec![issue("A-1", 50, 5, "app")];
        let out = apply_cap(issues, &config());
        assert_eq!(out.selected.len(), 1);
        assert!(
            out.rejected.is_empty(),
            "an untruncated sweep must not claim a cap"
        );
    }

    #[test]
    fn selection_order_follows_sentry() {
        let issues = vec![
            issue("A-1", 50, 5, "app"),
            issue("A-2", 90, 9, "app"),
            issue("A-3", 70, 7, "app"),
        ];
        let out = filter(issues, &config());
        let ids: Vec<&str> = out.selected.iter().map(|i| i.short_id.as_str()).collect();
        assert_eq!(ids, vec!["A-1", "A-2", "A-3"]);
    }
}
