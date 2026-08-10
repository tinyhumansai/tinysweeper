//! The golden fixture for the PII boundary.
//!
//! One realistic Sentry payload carrying every dangerous shape at once, in
//! every place the payload can put them, asserted against literally — not by
//! regex, because a regex assertion tests the same idea twice and passes when
//! both copies are wrong the same way.
//!
//! Two of these tests are the ones that matter as the code changes:
//!
//! - `promoted_field_set_equals_the_allow_list` fails when a field is added to
//!   `SafeIssue` without being argued onto the allow-list. A test asserting
//!   only "no email appears" passes forever while a new field quietly starts
//!   carrying one.
//! - `scrub_patterns_cannot_disable_the_built_in_scrubbing` fails if the
//!   layering is ever inverted into a replacement.

use super::*;
use crate::config::types::Sentry;
use crate::sentry::types::{RawEvent, RawIssue};

/// An email in the exception message, the user block, and a breadcrumb.
const EMAIL: &str = "alice.smith@example.com";
/// A bearer token in a request header and in the message.
const BEARER: &str = "eyJhbGciOiJIUzI1NiJ9.QUJDREVGRw.c2lnbmF0dXJlaGVyZQ";
/// A session cookie value.
const COOKIE: &str = "9f2b7c1e4a8d3f6b0c5e2a9d7f4b1e8c";
/// A card-shaped number, in the message and in a frame local.
const CARD: &str = "4111111111111111";
/// A frame-local variable value.
const FRAME_VAR: &str = "hunter2correcthorsebattery";
/// A credential the shared rulepack already knows.
const AWS_KEY: &str = "AKIAIOSFODNN7EXAMPLE";

/// Every secret the fixture contains, for the "none of these survive" sweep.
const ALL_SECRETS: &[&str] = &[EMAIL, BEARER, COOKIE, CARD, FRAME_VAR, AWS_KEY];

/// The issue payload as Sentry's list endpoint returns it.
fn issue_json() -> String {
    format!(
        r#"{{
          "id": "4711",
          "shortId": "API-1A2B",
          "culprit": "app/handlers/checkout in charge",
          "level": "error",
          "permalink": "https://sentry.io/organizations/acme/issues/4711/",
          "count": "1500",
          "userCount": 42,
          "status": "unresolved",
          "assignedTo": {{"email": "{EMAIL}"}},
          "metadata": {{
            "type": "PaymentError",
            "value": "declined card {CARD} for {EMAIL} using Bearer {BEARER}, key {AWS_KEY}",
            "filename": "checkout.rs"
          }}
        }}"#
    )
}

/// The latest-event payload, with every dangerous section populated.
fn event_json() -> String {
    format!(
        r#"{{
          "eventID": "abc123",
          "transaction": "POST /checkout",
          "release": {{"version": "2026.8.1"}},
          "user": {{"email": "{EMAIL}", "id": "u-9", "ip_address": "203.0.113.7"}},
          "contexts": {{"device": {{"name": "iPhone 15"}}, "os": {{"name": "iOS"}}}},
          "extra": {{"session_token": "{COOKIE}", "note": "{EMAIL}"}},
          "request": {{
            "url": "https://shop.example.com/checkout",
            "cookies": {{"session": "{COOKIE}"}},
            "headers": [["Authorization", "Bearer {BEARER}"], ["Cookie", "session={COOKIE}"]],
            "data": {{"card": "{CARD}", "email": "{EMAIL}"}}
          }},
          "entries": [
            {{"type": "breadcrumbs", "data": {{"values": [
              {{"message": "signed in as {EMAIL}", "data": {{"cookie": "{COOKIE}"}}}}
            ]}}}},
            {{"type": "request", "data": {{"cookies": {{"session": "{COOKIE}"}}}}}},
            {{"type": "exception", "data": {{"values": [
              {{
                "type": "PaymentError",
                "value": "declined {CARD}",
                "stacktrace": {{"frames": [
                  {{"filename": "src/gateway.rs", "function": "authorize", "lineno": 88,
                   "vars": {{"password": "{FRAME_VAR}", "card": "{CARD}", "user": "{EMAIL}"}},
                   "context_line": "let card = \"{CARD}\";"}},
                  {{"filename": "src/checkout.rs", "function": "charge", "lineno": 204,
                   "vars": {{"token": "Bearer {BEARER}"}}}}
                ]}}
              }}
            ]}}}}
          ]
        }}"#
    )
}

fn fixture() -> (RawIssue, RawEvent) {
    (
        serde_json::from_str(&issue_json()).expect("issue fixture parses"),
        serde_json::from_str(&event_json()).expect("event fixture parses"),
    )
}

fn config() -> Sentry {
    Sentry {
        scrub_patterns: Vec::new(),
        ..Sentry::default()
    }
}

/// The acceptance criterion from the issue, stated directly.
#[test]
fn no_secret_in_the_fixture_survives_into_the_promotion() {
    let (issue, event) = fixture();
    let safe = project(&issue, Some(&event), "api", &config());

    // Every field, every frame, rendered as one blob — so a secret that
    // escaped into a field this test forgot to name is still caught.
    let promoted = serde_json::to_string(&safe).expect("serializes");

    for secret in ALL_SECRETS {
        assert!(
            !promoted.contains(secret),
            "`{secret}` survived into the promoted issue:\n{promoted}"
        );
    }
}

/// The structural half. A value-matching test passes forever while a *new*
/// field starts carrying personal data; this one does not.
#[test]
fn promoted_field_set_equals_the_allow_list() {
    let (issue, event) = fixture();
    let safe = project(&issue, Some(&event), "api", &config());

    let value = serde_json::to_value(&safe).expect("serializes");
    let object = value.as_object().expect("an object");

    let mut promoted: Vec<&str> = object.keys().map(String::as_str).collect();
    promoted.sort_unstable();

    let mut allowed: Vec<&str> = ALLOWED_FIELDS.to_vec();
    allowed.sort_unstable();

    assert_eq!(
        promoted, allowed,
        "the promoted field set drifted from the allow-list"
    );
}

/// A frame is file, function and line. Anything else on it is a leak.
#[test]
fn frames_carry_only_file_function_and_line() {
    let (issue, event) = fixture();
    let safe = project(&issue, Some(&event), "api", &config());

    assert_eq!(safe.frames.len(), 2, "both exception frames promoted");

    let frame = serde_json::to_value(&safe.frames[0]).expect("serializes");
    let mut keys: Vec<&str> = frame
        .as_object()
        .expect("an object")
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(keys, vec!["filename", "function", "lineno"]);

    assert_eq!(safe.frames[0].filename, "src/gateway.rs");
    assert_eq!(safe.frames[0].function, "authorize");
    assert_eq!(safe.frames[0].lineno, Some(88));
}

/// Breadcrumb and request entries are not exception entries, so they
/// contribute no frames and no text at all.
#[test]
fn breadcrumb_and_request_entries_contribute_nothing() {
    let (issue, event) = fixture();
    let safe = project(&issue, Some(&event), "api", &config());

    let promoted = serde_json::to_string(&safe).expect("serializes");
    assert!(!promoted.contains("signed in as"), "{promoted}");
    assert!(!promoted.contains("iPhone"), "{promoted}");
    assert!(!promoted.contains("203.0.113.7"), "{promoted}");
    assert!(!promoted.contains("shop.example.com"), "{promoted}");
}

/// The allow-listed fields still arrive — a boundary that promotes nothing is
/// trivially safe and useless.
#[test]
fn the_allow_listed_fields_survive() {
    let (issue, event) = fixture();
    let safe = project(&issue, Some(&event), "api", &config());

    assert_eq!(safe.short_id, "API-1A2B");
    assert_eq!(safe.project, "api");
    assert_eq!(safe.kind, "PaymentError");
    assert_eq!(safe.culprit, "app/handlers/checkout in charge");
    assert_eq!(safe.transaction, "POST /checkout");
    assert_eq!(safe.level, "error");
    assert_eq!(safe.count, 1500);
    assert_eq!(safe.user_count, 42);
    assert_eq!(safe.release, "2026.8.1");
    assert!(safe.permalink.contains("sentry.io"));

    // The message keeps its shape — the redaction is surgical, not a blanket
    // drop. Without this the test above would pass on an empty string.
    assert!(safe.value.starts_with("declined"), "{}", safe.value);
    assert!(safe.value.contains("[redacted"), "{}", safe.value);
}

/// The failure mode the spec calls out by name: a config that looks like
/// hardening and is a hole.
#[test]
fn scrub_patterns_cannot_disable_the_built_in_scrubbing() {
    let (issue, event) = fixture();

    // A pattern that redacts something harmless, proving patterns are applied
    // *and* that applying them leaves the built-in passes intact.
    let config = Sentry {
        scrub_patterns: vec!["declined".to_string()],
        ..Sentry::default()
    };

    let safe = project(&issue, Some(&event), "api", &config);
    let promoted = serde_json::to_string(&safe).expect("serializes");

    assert!(
        !promoted.contains("declined"),
        "the pattern was not applied"
    );
    for secret in ALL_SECRETS {
        assert!(
            !promoted.contains(secret),
            "`{secret}` survived once scrub_patterns was set"
        );
    }
}

#[test]
fn scrub_patterns_are_case_insensitive_and_additive() {
    let scrubbed = scrub_text("Host INTERNAL-BOX responded", &["internal-box".to_string()]);
    assert!(!scrubbed.contains("INTERNAL-BOX"), "{scrubbed}");
    assert!(scrubbed.contains("Host"), "{scrubbed}");
    assert!(scrubbed.contains("responded"), "{scrubbed}");
}

#[test]
fn an_empty_scrub_pattern_is_ignored_rather_than_redacting_everything() {
    let scrubbed = scrub_text("ordinary text", &["".to_string(), "   ".to_string()]);
    assert_eq!(scrubbed, "ordinary text");
}

/// Scrubbing happens at projection, so there is no unscrubbed `SafeIssue` for
/// a later stage to hash, log or post. This is the "scrub before the text is
/// used for anything at all" requirement, expressed as a property.
#[test]
fn projection_scrubs_before_anything_downstream_can_observe_it() {
    let (issue, event) = fixture();
    let safe = project(&issue, Some(&event), "api", &config());

    // The dedupe marker is built from the short id, which is already scrubbed
    // by the time anyone can read it.
    assert!(!format!("{safe:?}").contains(CARD));
    assert!(!format!("{safe:?}").contains(EMAIL));
}

#[test]
fn the_excerpt_is_capped_even_when_the_message_is_enormous() {
    let mut issue: RawIssue = serde_json::from_str(&issue_json()).expect("parses");
    issue.metadata.value = Some("A".repeat(100_000));

    let event: RawEvent = serde_json::from_str(&event_json()).expect("parses");
    let safe = project(&issue, Some(&event), "api", &config());

    assert!(
        excerpt_bytes(&safe) <= MAX_EXCERPT_BYTES,
        "excerpt was {} bytes, over the {MAX_EXCERPT_BYTES} cap",
        excerpt_bytes(&safe)
    );
}

#[test]
fn a_pathological_frame_count_is_capped_and_keeps_the_innermost_frames() {
    let frames: Vec<String> = (0..500)
        .map(|n| format!(r#"{{"filename":"f{n}.rs","function":"fn{n}","lineno":{n}}}"#))
        .collect();
    let raw = format!(
        r#"{{"entries":[{{"type":"exception","data":{{"values":[
            {{"stacktrace":{{"frames":[{}]}}}}
        ]}}}}]}}"#,
        frames.join(",")
    );

    let event: RawEvent = serde_json::from_str(&raw).expect("parses");
    let (issue, _) = fixture();
    let safe = project(&issue, Some(&event), "api", &config());

    assert!(safe.frames.len() <= MAX_FRAMES, "{}", safe.frames.len());
    // Sentry orders frames innermost-last, so the failing code is at the end.
    assert_eq!(
        safe.frames.last().expect("a frame").function,
        "fn499",
        "truncation dropped the innermost frames instead of the outermost"
    );
    assert!(excerpt_bytes(&safe) <= MAX_EXCERPT_BYTES);
}

/// A promotion without the second API call is still worth opening.
#[test]
fn an_issue_with_no_event_still_projects() {
    let (issue, _) = fixture();
    let safe = project(&issue, None, "api", &config());

    assert_eq!(safe.short_id, "API-1A2B");
    assert!(safe.frames.is_empty());
    assert!(safe.transaction.is_empty());
    assert!(safe.release.is_empty());

    let promoted = serde_json::to_string(&safe).expect("serializes");
    for secret in ALL_SECRETS {
        assert!(!promoted.contains(secret), "`{secret}` survived");
    }
}

/// Truncation must not split a multi-byte character, which would produce
/// invalid UTF-8 in a GitHub issue body.
#[test]
fn truncation_lands_on_a_character_boundary() {
    let mut issue: RawIssue = serde_json::from_str(&issue_json()).expect("parses");
    issue.metadata.value = Some("é".repeat(50_000));

    let safe = project(&issue, None, "api", &config());
    assert!(safe.value.is_char_boundary(safe.value.len()));
    assert!(excerpt_bytes(&safe) <= MAX_EXCERPT_BYTES);
}
