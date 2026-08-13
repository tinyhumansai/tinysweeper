//! End-to-end sweeps against the offline forge and Sentry.
//!
//! These are the tests that map onto #91's acceptance criteria, one for one.

use super::*;
use crate::config::types::{Config, Sentry, SentryRoute};
use crate::forge::mock::MockForge;
use crate::sentry::mock::{MockSentry, SentryWrite};
use crate::sentry::types::{RawIssue, RawMetadata};

fn issue(short_id: &str, id: &str, count: u64, users: u64) -> RawIssue {
    RawIssue {
        id: id.into(),
        short_id: short_id.into(),
        culprit: Some("app/handler".into()),
        level: Some("error".into()),
        permalink: Some(format!("https://sentry.io/issues/{id}/")),
        count,
        user_count: users,
        metadata: RawMetadata {
            kind: Some("TypeError".into()),
            value: Some("boom".into()),
        },
    }
}

fn config(max_per_run: usize) -> Config {
    Config {
        sentry: Sentry {
            enabled: true,
            org: Some("acme".into()),
            projects: vec!["api".into()],
            min_events: 10,
            min_users: 1,
            max_per_run,
            labels: vec!["sentry".into()],
            annotate_sentry: true,
            route: vec![SentryRoute {
                project: "api".into(),
                repo: "acme/api".into(),
                labels: vec![],
            }],
            ..Sentry::default()
        },
        ..Config::default()
    }
}

fn ran(outcome: SweepOutcome) -> SweepReport {
    match outcome {
        SweepOutcome::Ran(report) => *report,
        SweepOutcome::Disabled => panic!("expected the sweep to run"),
    }
}

#[tokio::test]
async fn a_disabled_sweep_reads_and_writes_nothing() {
    let forge = MockForge::new();
    let sentry = MockSentry::new().with_issues("api", vec![issue("A-1", "1", 500, 50)]);
    let mut config = config(10);
    config.sentry.enabled = false;

    let outcome = sweep(&forge, &forge, &sentry, &config, false)
        .await
        .expect("ok");

    assert_eq!(outcome, SweepOutcome::Disabled);
    assert!(forge.wrote_nothing());
    assert!(sentry.wrote_nothing());
}

/// Acceptance: a sweep promotes qualifying issues as GitHub issues.
#[tokio::test]
async fn a_qualifying_issue_is_promoted_with_a_marker_and_a_link() {
    let forge = MockForge::new();
    let sentry = MockSentry::new().with_issues("api", vec![issue("A-1", "1", 500, 50)]);

    let report = ran(sweep(&forge, &forge, &sentry, &config(10), false)
        .await
        .expect("ok"));

    assert_eq!(report.promoted.len(), 1);
    assert_eq!(report.promoted[0].short_id, "A-1");
    assert_eq!(report.promoted[0].repo, "acme/api");

    let created = forge
        .issue(&repo(), report.promoted[0].number)
        .await
        .expect("read");
    assert!(
        created
            .body
            .contains("<!-- tinysweeper:sentry=acme/api/A-1 -->"),
        "{}",
        created.body
    );
    assert!(
        created.body.contains("https://sentry.io/issues/1/"),
        "{}",
        created.body
    );
    assert!(created.labels.contains(&"sentry".to_string()));
}

/// Acceptance: no duplicates across multiple runs. This is the one that
/// matters most — a duplicate is the failure that scales.
#[tokio::test]
async fn a_second_sweep_promotes_nothing_new() {
    let forge = MockForge::new();
    let sentry = MockSentry::new().with_issues("api", vec![issue("A-1", "1", 500, 50)]);
    let config = config(10);

    let first = ran(sweep(&forge, &forge, &sentry, &config, false)
        .await
        .expect("ok"));
    assert_eq!(first.promoted.len(), 1);

    let second = ran(sweep(&forge, &forge, &sentry, &config, false)
        .await
        .expect("ok"));
    assert!(second.promoted.is_empty(), "{:?}", second.promoted);
    assert_eq!(
        second.skipped[0].reason,
        Skipped::AlreadyTracked {
            number: first.promoted[0].number
        }
    );

    // And a third, because "idempotent twice" and "idempotent" differ.
    let third = ran(sweep(&forge, &forge, &sentry, &config, false)
        .await
        .expect("ok"));
    assert!(third.promoted.is_empty());
}

/// The dedupe key must survive both normalisations promotion applies.
///
/// `promote::body` writes the marker from `SafeIssue.short_id`, which has been
/// scrubbed **and truncated to `MARKER_COMPONENT_BYTES`**. A lookup that
/// applied only one of those searched for a marker that was never written, so
/// an over-long short id was re-promoted on every sweep — a duplicate per run,
/// forever. This covers the sweep's own call site, not just the helper.
#[tokio::test]
async fn an_over_long_short_id_is_not_re_promoted_on_the_next_sweep() {
    // Short tokens on purpose: a single long alphanumeric run is itself
    // opaque-token-shaped, so scrubbing would replace the whole id and
    // truncation would never engage — the test would pass without testing.
    let long = "API 1A2B ".repeat(30);
    assert!(
        long.len() > crate::sentry::redact::MARKER_COMPONENT_BYTES,
        "the fixture must exceed the cap or this proves nothing"
    );
    let forge = MockForge::new();
    let sentry = MockSentry::new().with_issues("api", vec![issue(&long, "1", 500, 50)]);
    let config = config(10);

    let first = ran(sweep(&forge, &forge, &sentry, &config, false)
        .await
        .expect("ok"));
    assert_eq!(first.promoted.len(), 1, "the first sweep promotes it");

    let second = ran(sweep(&forge, &forge, &sentry, &config, false)
        .await
        .expect("ok"));
    assert!(
        second.promoted.is_empty(),
        "the second sweep must find its own marker: {:?}",
        second.promoted
    );
}

/// Acceptance: a project with no route is skipped with a warning, not
/// silently — the report carries it, so a caller can print it.
#[tokio::test]
async fn an_unrouted_project_is_skipped_loudly_and_recorded() {
    let forge = MockForge::new();
    let sentry = MockSentry::new().with_issues("web", vec![issue("W-1", "9", 500, 50)]);

    let mut config = config(10);
    config.sentry.projects = vec!["api".into(), "web".into()];

    let report = ran(sweep(&forge, &forge, &sentry, &config, false)
        .await
        .expect("ok"));

    assert_eq!(report.unrouted, vec!["web".to_string()]);
    assert!(
        report.promoted.is_empty(),
        "an unrouted project must not be guessed into a repository"
    );
}

/// Acceptance: hitting `max_per_run` is reported, not silent.
#[tokio::test]
async fn the_cap_is_reported_rather_than_silently_truncating() {
    let forge = MockForge::new();
    let issues = (0..5)
        .map(|n| issue(&format!("A-{n}"), &n.to_string(), 500, 50))
        .collect();
    let sentry = MockSentry::new().with_issues("api", issues);

    let report = ran(sweep(&forge, &forge, &sentry, &config(2), false)
        .await
        .expect("ok"));

    assert_eq!(report.promoted.len(), 2);
    assert_eq!(report.truncated, vec![("api".to_string(), 2)]);
    assert_eq!(
        report
            .skipped
            .iter()
            .filter(|s| s.reason == Skipped::OverCap { cap: 2 })
            .count(),
        3
    );
}

/// The ordering property from the module docs: dedupe runs before the cap, so
/// already-tracked issues do not eat the promotion budget.
#[tokio::test]
async fn the_cap_counts_promotable_issues_not_tracked_ones() {
    let forge = MockForge::new();
    let issues: Vec<RawIssue> = (0..4)
        .map(|n| issue(&format!("A-{n}"), &n.to_string(), 500, 50))
        .collect();
    let sentry = MockSentry::new().with_issues("api", issues);
    let config = config(2);

    // Pre-track the first two, exactly as a previous sweep would have.
    for n in 0..2 {
        forge
            .create_issue(
                &repo(),
                &format!("existing {n}"),
                &format!("<!-- tinysweeper:sentry=acme/api/A-{n} -->"),
                &[],
            )
            .await
            .expect("seeded");
    }

    let report = ran(sweep(&forge, &forge, &sentry, &config, false)
        .await
        .expect("ok"));

    // If the cap ran first it would have consumed both budget slots on the
    // already-tracked A-0 and A-1 and promoted nothing.
    assert_eq!(report.promoted.len(), 2, "{:?}", report.promoted);
    let promoted: Vec<&str> = report
        .promoted
        .iter()
        .map(|p| p.short_id.as_str())
        .collect();
    assert_eq!(promoted, vec!["A-2", "A-3"]);
}

/// Acceptance: closing the GitHub issue resolves the Sentry issue.
#[tokio::test]
async fn closing_the_github_issue_resolves_sentry_on_the_next_sweep() {
    let forge = MockForge::new();
    let sentry = MockSentry::new().with_issues("api", vec![issue("A-1", "1", 500, 50)]);
    let mut config = config(10);
    config.sentry.resolve_when_tracked = true;

    let first = ran(sweep(&forge, &forge, &sentry, &config, false)
        .await
        .expect("ok"));
    let number = first.promoted[0].number;

    // Still open: nothing resolves.
    let second = ran(sweep(&forge, &forge, &sentry, &config, false)
        .await
        .expect("ok"));
    assert!(second.resolved.is_empty());

    forge.close_issue(&repo(), number).await.expect("closed");

    let third = ran(sweep(&forge, &forge, &sentry, &config, false)
        .await
        .expect("ok"));
    assert_eq!(third.resolved, vec!["1".to_string()]);
    assert!(sentry.writes().contains(&SentryWrite::Resolved {
        issue_id: "1".into()
    }));
}

#[tokio::test]
async fn nothing_resolves_when_the_flag_is_off() {
    let forge = MockForge::new();
    let sentry = MockSentry::new().with_issues("api", vec![issue("A-1", "1", 500, 50)]);
    let config = config(10);

    let first = ran(sweep(&forge, &forge, &sentry, &config, false)
        .await
        .expect("ok"));
    forge
        .close_issue(&repo(), first.promoted[0].number)
        .await
        .expect("closed");

    let second = ran(sweep(&forge, &forge, &sentry, &config, false)
        .await
        .expect("ok"));
    assert!(second.resolved.is_empty());
}

#[tokio::test]
async fn the_sentry_issue_is_annotated_with_the_github_url() {
    let forge = MockForge::new();
    let sentry = MockSentry::new().with_issues("api", vec![issue("A-1", "1", 500, 50)]);

    let report = ran(sweep(&forge, &forge, &sentry, &config(10), false)
        .await
        .expect("ok"));

    assert!(report.promoted[0].annotated);
    let SentryWrite::Annotation { issue_id, text } = &sentry.writes()[0] else {
        panic!("expected an annotation");
    };
    assert_eq!(issue_id, "1");
    assert!(
        text.contains("https://github.com/acme/api/issues/"),
        "{text}"
    );
}

#[tokio::test]
async fn issues_below_the_gates_are_skipped_with_reasons() {
    let forge = MockForge::new();
    let sentry = MockSentry::new().with_issues(
        "api",
        vec![issue("A-1", "1", 3, 50), issue("A-2", "2", 500, 0)],
    );

    let report = ran(sweep(&forge, &forge, &sentry, &config(10), false)
        .await
        .expect("ok"));

    assert!(report.promoted.is_empty());
    assert!(forge.wrote_nothing());
    assert!(
        report
            .skipped
            .iter()
            .any(|s| matches!(s.reason, Skipped::TooFewEvents { .. }))
    );
    assert!(
        report
            .skipped
            .iter()
            .any(|s| matches!(s.reason, Skipped::TooFewUsers { .. }))
    );
}

/// A dry run decides identically and writes nothing anywhere.
#[tokio::test]
async fn a_dry_run_writes_nothing_but_reports_what_it_would_do() {
    let forge = MockForge::new();
    let sentry = MockSentry::new().with_issues("api", vec![issue("A-1", "1", 500, 50)]);

    let report = ran(sweep(&forge, &forge, &sentry, &config(10), true)
        .await
        .expect("ok"));

    assert!(report.dry_run);
    assert_eq!(report.promoted.len(), 1, "it reports what it would promote");
    assert!(forge.wrote_nothing(), "a dry run must not open an issue");
    assert!(sentry.wrote_nothing(), "a dry run must not touch sentry");
}

/// One unreachable project must not take the others down with it.
#[tokio::test]
async fn a_failing_project_is_recorded_and_the_sweep_continues() {
    let forge = MockForge::new();
    let sentry = MockSentry::new().failing("503 service unavailable");

    let report = ran(sweep(&forge, &forge, &sentry, &config(10), false)
        .await
        .expect("ok"));

    assert_eq!(report.failed.len(), 1);
    assert_eq!(report.failed[0].0, "api");
    assert!(report.promoted.is_empty());
    assert!(
        !report.wrote_nothing() || report.promoted.is_empty(),
        "a failed sweep reports failure rather than success"
    );
}

/// A route naming something that is not `owner/name` is refused rather than
/// promoted somewhere unintended.
#[tokio::test]
async fn a_malformed_route_repo_is_refused() {
    let forge = MockForge::new();
    let sentry = MockSentry::new().with_issues("api", vec![issue("A-1", "1", 500, 50)]);
    let mut config = config(10);
    config.sentry.route[0].repo = "not-a-repo".into();

    let report = ran(sweep(&forge, &forge, &sentry, &config, false)
        .await
        .expect("ok"));

    assert_eq!(report.failed.len(), 1);
    assert!(forge.wrote_nothing());
}

/// An issue with no short id can never be deduplicated, so it is refused
/// rather than promoted and recreated on every subsequent sweep.
#[tokio::test]
async fn an_issue_with_no_short_id_is_never_promoted() {
    let forge = MockForge::new();
    let mut raw = issue("", "1", 500, 50);
    raw.short_id = String::new();
    let sentry = MockSentry::new().with_issues("api", vec![raw]);

    let report = ran(sweep(&forge, &forge, &sentry, &config(10), false)
        .await
        .expect("ok"));

    assert!(report.promoted.is_empty());
    assert!(forge.wrote_nothing());
    // And it is accounted for, so the report's tally adds up: "fetched 1,
    // promoted 0, skipped 0" is a report nobody can act on.
    assert_eq!(report.skipped.len(), 1);
    assert_eq!(report.skipped[0].reason, Skipped::Undedupable);
}

/// The spec's log rule, as far as a test can reach it: nothing the sweep
/// records carries text that did not go through the redaction boundary.
#[tokio::test]
async fn the_report_carries_no_unscrubbed_text() {
    let forge = MockForge::new();
    let mut raw = issue("A-1", "1", 500, 50);
    raw.metadata.value = Some("declined 4111111111111111 for alice@example.com".into());
    let sentry = MockSentry::new().with_issues("api", vec![raw]);

    let report = ran(sweep(&forge, &forge, &sentry, &config(10), false)
        .await
        .expect("ok"));

    let rendered = format!("{report:?}");
    assert!(!rendered.contains("4111111111111111"), "{rendered}");
    assert!(!rendered.contains("alice@example.com"), "{rendered}");
}

fn repo() -> RepoId {
    RepoId {
        owner: "acme".into(),
        name: "api".into(),
    }
}
