//! Promotion: what the issue looks like, and what cannot get into it.

use super::*;
use crate::config::types::{Config, Sentry, SentryRoute};
use crate::forge::mock::{MockForge, Write as ForgeWriteRecord};
use crate::sentry::redact;
use crate::sentry::types::{RawEvent, RawIssue};

fn repo() -> RepoId {
    RepoId {
        owner: "acme".into(),
        name: "api".into(),
    }
}

fn config() -> Config {
    Config {
        sentry: Sentry {
            enabled: true,
            org: Some("acme".into()),
            projects: vec!["api".into()],
            labels: vec!["sentry".into(), "bug".into()],
            route: vec![SentryRoute {
                project: "api".into(),
                repo: "acme/api".into(),
                labels: vec!["area: sentry".into()],
            }],
            ..Sentry::default()
        },
        ..Config::default()
    }
}

fn raw(level: &str, kind: &str, value: &str) -> RawIssue {
    RawIssue {
        id: "4711".into(),
        short_id: "API-1A2B".into(),
        culprit: Some("app/handlers/checkout in charge".into()),
        level: Some(level.into()),
        permalink: Some("https://sentry.io/organizations/acme/issues/4711/".into()),
        count: 1500,
        user_count: 42,
        metadata: crate::sentry::types::RawMetadata {
            kind: Some(kind.into()),
            value: Some(value.into()),
        },
    }
}

fn event() -> RawEvent {
    serde_json::from_str(
        r#"{
          "transaction": "POST /checkout",
          "release": {"version": "2026.8.1"},
          "entries": [{"type":"exception","data":{"values":[
            {"stacktrace":{"frames":[
              {"filename":"src/outer.rs","function":"main","lineno":1},
              {"filename":"src/gateway.rs","function":"authorize","lineno":88}
            ]}}
          ]}}]
        }"#,
    )
    .expect("fixture parses")
}

fn safe(level: &str, kind: &str, value: &str) -> SafeIssue {
    redact::project(
        &raw(level, kind, value),
        Some(&event()),
        "api",
        &config().sentry,
    )
}

#[test]
fn priority_follows_the_sentry_level_only() {
    assert_eq!(priority("fatal"), Priority::P0);
    assert_eq!(priority("error"), Priority::P1);
    assert_eq!(priority("warning"), Priority::P2);
    assert_eq!(priority("info"), Priority::P3);
    assert_eq!(priority("debug"), Priority::P3);
}

#[test]
fn an_unknown_or_missing_level_is_the_least_urgent_rather_than_a_guess() {
    assert_eq!(priority(""), Priority::P3);
    assert_eq!(priority("catastrophe"), Priority::P3);
    assert_eq!(
        priority("  ERROR  "),
        Priority::P1,
        "case and space tolerant"
    );
}

#[test]
fn the_title_names_the_exception_and_the_short_id() {
    let title = title(&safe("error", "PaymentError", "card declined"));
    assert_eq!(title, "[sentry] PaymentError: card declined (API-1A2B)");
}

/// A title is one line. An exception message with a newline would otherwise
/// truncate it at the break with nothing to show anything followed.
#[test]
fn a_multiline_message_is_collapsed_into_one_line() {
    let title = title(&safe(
        "error",
        "PaymentError",
        "line one\nline two\n\tindented",
    ));
    assert!(!title.contains('\n'), "{title}");
    assert!(title.contains("line one line two indented"), "{title}");
}

#[test]
fn a_very_long_message_is_truncated_in_the_title() {
    let title = title(&safe("error", "E", &"x".repeat(500)));
    assert!(title.contains('…'), "{title}");
    assert!(title.len() < 200, "{title}");
    assert!(title.ends_with("(API-1A2B)"), "{title}");
}

#[test]
fn a_missing_exception_type_still_produces_a_usable_title() {
    let title = title(&safe("error", "", ""));
    assert_eq!(title, "[sentry] Sentry issue (API-1A2B)");
}

#[test]
fn the_body_ends_with_the_dedupe_marker() {
    let body = body(&safe("error", "PaymentError", "card declined"), "acme");
    assert!(
        body.trim_end()
            .ends_with("<!-- tinysweeper:sentry=acme/api/API-1A2B -->"),
        "{body}"
    );
}

#[test]
fn the_body_carries_the_counts_release_and_a_link_back() {
    let body = body(&safe("error", "PaymentError", "card declined"), "acme");
    assert!(body.contains("| Events | 1500 |"), "{body}");
    assert!(body.contains("| Users affected | 42 |"), "{body}");
    assert!(body.contains("`2026.8.1`"), "{body}");
    assert!(body.contains("`POST /checkout`"), "{body}");
    assert!(
        body.contains("https://sentry.io/organizations/acme/issues/4711/"),
        "{body}"
    );
}

/// Frames render innermost-first, because that is where the fault is.
#[test]
fn the_stack_renders_innermost_first() {
    let body = body(&safe("error", "PaymentError", "boom"), "acme");
    let gateway = body.find("src/gateway.rs:88").expect("inner frame");
    let outer = body.find("src/outer.rs:1").expect("outer frame");
    assert!(
        gateway < outer,
        "innermost frame should come first:\n{body}"
    );
}

/// Untrusted text must render as data. A message containing its own fence must
/// not be able to close ours and have the rest render as Markdown.
///
/// The property is *containment*, not absence: the heading text is still in
/// the body — it has to be, it is the error message — but it sits between an
/// opening and a closing fence that are strictly longer than any backtick run
/// inside it, so Markdown renders it verbatim.
#[test]
fn a_message_containing_a_fence_cannot_escape_the_one_we_wrap_it_in() {
    let hostile = "``` \n### Injected heading\n``` more";
    let body = body(&safe("error", "E", hostile), "acme");

    let fences: Vec<usize> = body
        .lines()
        .enumerate()
        .filter(|(_, line)| line.trim_start().starts_with("````"))
        .map(|(index, _)| index)
        .collect();
    assert!(
        fences.len() >= 2,
        "expected an opening and closing fence longer than the content's own:\n{body}"
    );

    let heading = body
        .lines()
        .position(|line| line.trim() == "### Injected heading")
        .expect("the message text is still present");
    assert!(
        heading > fences[0] && heading < fences[1],
        "the injected heading escaped the fence:\n{body}"
    );

    // The marker still terminates the body, so dedupe still works on it.
    assert!(body.trim_end().ends_with("-->"), "{body}");
}

/// A pipe in a table cell would shear the table into a different shape.
#[test]
fn a_pipe_in_a_table_cell_is_escaped() {
    let mut issue = raw("error", "E", "boom");
    issue.culprit = Some("app | evil | column".into());
    let safe = redact::project(&issue, None, "api", &config().sentry);

    let body = body(&safe, "acme");
    let culprit_row = body
        .lines()
        .find(|line| line.starts_with("| Culprit |"))
        .expect("a culprit row");
    assert!(culprit_row.contains("\\|"), "{culprit_row}");
}

#[tokio::test]
async fn promotion_creates_the_issue_with_config_route_and_priority_labels() {
    let forge = MockForge::new();
    let config = config();
    let safe = safe("error", "PaymentError", "card declined");

    let number = promote(&forge, &forge, &config, &repo(), "acme", &safe)
        .await
        .expect("promotes");
    assert_eq!(number, 1);

    let ForgeWriteRecord::IssueCreated { title, labels, .. } = forge
        .writes()
        .into_iter()
        .find(|w| matches!(w, ForgeWriteRecord::IssueCreated { .. }))
        .expect("an issue was created")
    else {
        unreachable!()
    };

    assert!(title.starts_with("[sentry] PaymentError"), "{title}");
    assert!(labels.contains(&"sentry".to_string()), "{labels:?}");
    assert!(labels.contains(&"bug".to_string()), "{labels:?}");
    assert!(labels.contains(&"area: sentry".to_string()), "{labels:?}");
    assert!(labels.contains(&"priority: p1".to_string()), "{labels:?}");
}

/// The route's labels are additive: a per-route list that forgets `sentry`
/// must not drop it.
#[tokio::test]
async fn route_labels_add_to_the_section_wide_ones() {
    let config = config();
    let labels = config.sentry.labels_for("api");
    assert_eq!(labels, vec!["sentry", "bug", "area: sentry"]);
}

#[tokio::test]
async fn a_project_with_no_route_still_gets_the_section_labels() {
    let config = config();
    assert_eq!(config.sentry.labels_for("unrouted"), vec!["sentry", "bug"]);
}

/// The end-to-end property the whole module exists for.
#[tokio::test]
async fn nothing_personal_reaches_the_created_issue() {
    let forge = MockForge::new();
    let config = config();
    let safe = safe(
        "error",
        "PaymentError",
        "declined 4111111111111111 for alice@example.com with Bearer eyJhbGciOiJIUzI1NiJ9.QUJD.c2ln",
    );

    promote(&forge, &forge, &config, &repo(), "acme", &safe)
        .await
        .expect("promotes");

    let written = format!("{:?}", forge.writes());
    for secret in [
        "4111111111111111",
        "alice@example.com",
        "eyJhbGciOiJIUzI1NiJ9",
    ] {
        assert!(!written.contains(secret), "`{secret}` reached GitHub");
    }
}

/// `kind::plan` refuses when the owner defines no types, and that refusal must
/// not fail the promotion — an untyped tracked issue beats a lost one.
#[tokio::test]
async fn an_owner_with_no_issue_types_still_gets_the_issue() {
    let forge = MockForge::new();
    let mut config = config();
    config.issues.apply_issue_type = true;

    let number = promote(
        &forge,
        &forge,
        &config,
        &repo(),
        "acme",
        &safe("error", "E", "boom"),
    )
    .await
    .expect("promotes");

    assert_eq!(number, 1);
    assert!(
        !forge
            .writes()
            .iter()
            .any(|w| matches!(w, ForgeWriteRecord::IssueType { .. })),
        "no type should be set when the owner defines none"
    );
}
