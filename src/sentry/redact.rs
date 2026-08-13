//! The PII boundary: the only way to turn a Sentry payload into something
//! writable.
//!
//! This module lands before any promotion code and everything downstream is
//! written against it, because the alternative ordering leaves a window in
//! which the sweep works and the redaction does not — and that window is
//! measured in "how fast can we delete a GitHub issue *and its edit history*".
//! GitHub keeps the original text of an edited issue, so a leak here cannot be
//! fixed afterwards.
//!
//! ## Three layers, in this order
//!
//! 1. **The parse is the allow-list.** [`crate::sentry::types`] declares only
//!    promotable fields, so the dangerous ones are never deserialized. That is
//!    the layer that fails *closed* when Sentry adds a field.
//! 2. **Scrubbing.** Every surviving string goes through
//!    [`crate::scan::secrets::scrub`] — the same rulepack the diff scanners use
//!    — before it is used for *anything*, including the dedupe marker and any
//!    log line. Not at the point of posting: by then it has already been
//!    hashed, traced and buffered.
//! 3. **A hard size cap.** [`MAX_EXCERPT_BYTES`] bounds how much can leak when
//!    layers one and two are both wrong.
//!
//! ## `scrub_patterns` layers on top, and cannot subtract
//!
//! `sentry.scrub_patterns` is applied *after* the built-in scrubbing and can
//! only remove more text. There is no configuration path that reaches
//! [`scrub_text`] with the built-in pass disabled, because the call is
//! unconditional and takes no flag — the failure mode being a config that
//! looks like hardening and is a hole.
//!
//! **Patterns are matched as case-insensitive literal substrings, not
//! regexes.** The field is documented only as "anything matching these
//! patterns", and the two candidate readings fail in opposite directions: an
//! unanchored regex someone gets wrong redacts nothing and looks like it
//! worked, while a literal substring can only ever redact more than intended.
//! Literal matching also keeps the crate's dependency floor where it is —
//! there is no regex engine in the default build, and the offline-by-default
//! invariant is worth more than pattern expressiveness here. If regexes are
//! wanted later this is the one function to change.

use crate::config::types::Sentry;
use crate::scan::secrets;
use crate::sentry::pii;
use crate::sentry::types::{RawEvent, RawFrame, RawIssue, SafeFrame, SafeIssue, Scrubbed};

/// The exact set of fields a promoted issue may carry.
///
/// This is the allow-list from the specification, written once, in the order
/// [`SafeIssue`] declares them. `promoted_field_set_equals_the_allow_list`
/// asserts the serialized key set of a real projection against it, so adding a
/// field to `SafeIssue` without adding it here fails the test run — which is
/// the point. A test that only asserts "no email appears" passes forever while
/// a new field quietly starts carrying one.
pub const ALLOWED_FIELDS: &[&str] = &[
    "short_id",
    "project",
    "kind",
    "value",
    "culprit",
    "transaction",
    "level",
    "count",
    "user_count",
    "release",
    "frames",
    "permalink",
];

/// The hard ceiling on promoted free text, in bytes.
///
/// Counted across every text field and every frame together, not per field: a
/// per-field limit multiplied by an unbounded frame count is not a limit. This
/// is a blast radius, not a formatting preference — it bounds what a failure of
/// the two layers above can cost.
pub const MAX_EXCERPT_BYTES: usize = 4096;

/// How many stack frames may be promoted.
///
/// Frames are the one unbounded list in the payload, and a deep recursion can
/// carry thousands. Capping the count as well as the bytes keeps a truncated
/// promotion readable rather than one frame repeated to the byte limit.
pub const MAX_FRAMES: usize = 30;

/// The replacement written where text was removed.
const ELIDED: &str = "…";

/// Scrub `text` for anything that must not reach GitHub.
///
/// Three passes, in this order, each of which can only ever *remove*:
///
/// 1. [`crate::scan::secrets::scrub`] — the shared credential rulepack.
/// 2. [`crate::sentry::pii::redact`] — the personal-data shapes the credential
///    rulepack does not know about. See that module for why it is local.
/// 3. `extra_patterns` — `sentry.scrub_patterns`, matched as case-insensitive
///    literal substrings.
///
/// The ordering is load-bearing: the built-in passes run first so a deployment
/// pattern can only ever tighten the result, and neither built-in pass takes a
/// flag, so there is no configuration that reaches this function with them
/// off.
pub fn scrub_text(text: &str, extra_patterns: &[String]) -> String {
    // Unconditional, and takes no flag. This is what makes "scrub_patterns
    // cannot disable the built-in scrubbing" a property of the code rather
    // than of the documentation.
    let mut scrubbed = pii::redact(&secrets::scrub(text));

    for pattern in extra_patterns {
        if pattern.trim().is_empty() {
            continue;
        }
        scrubbed = remove_literal_ci(&scrubbed, pattern);
    }

    scrubbed
}

/// The cap applied to `short_id` and `project` by [`enforce_budget`].
///
/// Named because [`marker_component`] must use the same number: those two
/// fields are what the dedupe marker is built from, and a lookup that
/// normalises them differently from the promotion cannot match its own marker.
pub const MARKER_COMPONENT_BYTES: usize = 128;

/// Normalise a value the dedupe marker is built from, exactly as promotion does.
///
/// `promote::body` renders the marker from `SafeIssue.short_id` and
/// `SafeIssue.project`, which have been **scrubbed and then truncated** by
/// [`project`] + [`enforce_budget`]. A lookup that only scrubs searches for a
/// marker that was never written, so any value over the cap is re-promoted on
/// every sweep — the failure mode that scales.
///
/// Both paths call this so the two cannot drift apart again.
pub fn marker_component(text: &str, patterns: &[String]) -> String {
    let mut out = scrub_text(text, patterns);
    truncate_to(&mut out, MARKER_COMPONENT_BYTES);
    out
}

/// Remove every case-insensitive occurrence of `needle` from `haystack`.
fn remove_literal_ci(haystack: &str, needle: &str) -> String {
    let mut out = String::with_capacity(haystack.len());
    let mut cursor = 0usize;

    // Searches the original bytes rather than a lowered copy. Offsets taken
    // from a lowered string can address a different position in the original
    // even when the two have the same total length — see the note on
    // [`pii::find_ci`]. Here that did not panic (the boundary check below
    // caught it) but it could elide the *wrong span*: the pattern the
    // operator asked to remove would survive and unrelated text would go
    // instead, which is the failing-open direction for a scrubber.
    //
    // The old length guard also degraded silently to an exact-case
    // `replace` for any needle or haystack whose lowercase differs in
    // length, so a `scrub_patterns` entry quietly stopped being
    // case-insensitive exactly when it was most likely to matter.
    //
    // `find_ci` returns the end offset rather than `start + needle.len()`,
    // because a case-insensitive match can span a different number of bytes
    // than the needle — which is the whole reason the old arithmetic was
    // wrong.
    while let Some((found, found_end)) = pii::find_ci(&haystack[cursor..], needle) {
        let start = cursor + found;
        let end = cursor + found_end;
        // Defensive only: `find_ci` walks real character boundaries.
        if !haystack.is_char_boundary(start) || !haystack.is_char_boundary(end) {
            break;
        }
        out.push_str(&haystack[cursor..start]);
        out.push_str(ELIDED);
        cursor = end;
    }
    out.push_str(&haystack[cursor..]);
    out
}

/// Project a Sentry issue and its latest event into the promotable subset.
///
/// The only constructor of [`SafeIssue`]. `event` is optional because the
/// latest-event fetch is a second API call that may legitimately fail or be
/// skipped; a promotion without it carries no transaction, release or frames
/// and is still worth opening.
pub fn project(
    issue: &RawIssue,
    event: Option<&RawEvent>,
    project_slug: &str,
    config: &Sentry,
) -> SafeIssue {
    let patterns = &config.scrub_patterns;
    let scrub = |text: &str| scrub_text(text, patterns);

    let frames = event
        .map(|event| safe_frames(event, patterns))
        .unwrap_or_default();

    let mut safe = SafeIssue {
        // Identifiers, not captured data: Sentry generates the short id and
        // the permalink. They are scrubbed anyway — the cost is nil and the
        // alternative is a field nobody re-checks when its provenance changes.
        short_id: scrub(&issue.short_id),
        project: scrub(project_slug),
        kind: scrub(issue.metadata.kind.as_deref().unwrap_or_default()),
        value: scrub(issue.metadata.value.as_deref().unwrap_or_default()),
        culprit: scrub(issue.culprit.as_deref().unwrap_or_default()),
        transaction: scrub(
            event
                .and_then(|event| event.transaction.as_deref())
                .unwrap_or_default(),
        ),
        level: scrub(issue.level.as_deref().unwrap_or_default()),
        count: issue.count,
        user_count: issue.user_count,
        release: scrub(
            event
                .and_then(|event| event.release.as_ref())
                .and_then(|release| release.version())
                .unwrap_or_default(),
        ),
        frames,
        permalink: scrub(issue.permalink.as_deref().unwrap_or_default()),
        scrubbed: Scrubbed(()),
    };

    enforce_budget(&mut safe);
    safe
}

/// Extract file/function/line for each frame of every exception entry.
///
/// Frames arrive innermost-last from Sentry. The **last** `MAX_FRAMES` are
/// kept rather than the first, because the innermost frames are the ones that
/// name the failing code; truncating from the end would keep the runtime's
/// entry points and drop the answer.
fn safe_frames(event: &RawEvent, patterns: &[String]) -> Vec<SafeFrame> {
    let mut frames: Vec<&RawFrame> = Vec::new();
    for entry in &event.entries {
        if entry.kind != "exception" {
            continue;
        }
        for value in &entry.data.values {
            if let Some(stacktrace) = &value.stacktrace {
                frames.extend(stacktrace.frames.iter());
            }
        }
    }

    let start = frames.len().saturating_sub(MAX_FRAMES);
    frames[start..]
        .iter()
        .map(|frame| SafeFrame {
            filename: scrub_text(frame.filename.as_deref().unwrap_or_default(), patterns),
            function: scrub_text(frame.function.as_deref().unwrap_or_default(), patterns),
            lineno: frame.lineno,
        })
        .collect()
}

/// Truncate `safe` until its promoted text fits [`MAX_EXCERPT_BYTES`].
///
/// Frames go first and the exception message last: a promotion that has lost
/// its stack trace is still actionable, while one that has lost the error text
/// is not. `count`, `user_count` and `lineno` are numbers and cost nothing to
/// keep.
fn enforce_budget(safe: &mut SafeIssue) {
    // The fixed fields are bounded FIRST, so `fixed` below is their real cost.
    //
    // These caps used to run at the end of this function, under a comment
    // claiming a pathological permalink could not blow the budget — which is
    // precisely what it did: a 5 KB permalink was charged at 5 KB, drove
    // `budget` to zero and reduced the exception message to the elision
    // marker, and was only then cut to 512 bytes. Charging a field at a length
    // it will not be promoted at is the whole defect.
    truncate_to(&mut safe.short_id, MARKER_COMPONENT_BYTES);
    truncate_to(&mut safe.project, MARKER_COMPONENT_BYTES);
    truncate_to(&mut safe.permalink, 512);
    truncate_to(&mut safe.level, 32);
    truncate_to(&mut safe.kind, 256);

    // Ordered least- to most-valuable, which is the order they are sacrificed.
    let fixed = safe.short_id.len()
        + safe.project.len()
        + safe.permalink.len()
        + safe.level.len()
        + safe.kind.len();

    let mut budget = MAX_EXCERPT_BYTES.saturating_sub(fixed);

    // The message is the point of the issue, so it is reserved first — but it
    // is still capped, because a single exception message can be megabytes.
    let value_cap = budget.min(MAX_EXCERPT_BYTES / 2);
    truncate_to(&mut safe.value, value_cap);
    budget = budget.saturating_sub(safe.value.len());

    truncate_to(&mut safe.culprit, budget.min(512));
    budget = budget.saturating_sub(safe.culprit.len());

    truncate_to(&mut safe.transaction, budget.min(512));
    budget = budget.saturating_sub(safe.transaction.len());

    truncate_to(&mut safe.release, budget.min(256));
    budget = budget.saturating_sub(safe.release.len());

    // Whatever is left buys frames, whole ones only. A half-rendered frame is
    // noise rather than evidence.
    let mut kept = Vec::with_capacity(safe.frames.len());
    for frame in safe.frames.drain(..) {
        let cost = frame.filename.len() + frame.function.len();
        if cost > budget {
            break;
        }
        budget -= cost;
        kept.push(frame);
    }
    safe.frames = kept;
}

/// Truncate `text` to at most `max` bytes, on a character boundary.
///
/// **At most `max` including the marker.** When there is not room for even the
/// marker the text is cleared rather than replaced by it: `max.saturating_sub`
/// bottoms out at zero, so the old code emitted a 3-byte `ELIDED` for a cap of
/// 0..3 and overshot. Four fields doing that put the total over
/// [`MAX_EXCERPT_BYTES`], which is the one thing this function exists to
/// guarantee.
fn truncate_to(text: &mut String, max: usize) {
    if text.len() <= max {
        return;
    }
    if max < ELIDED.len() {
        text.clear();
        return;
    }
    // Leave room for the marker so the result still fits the budget.
    let room = max.saturating_sub(ELIDED.len());
    let mut end = room;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    text.push_str(ELIDED);
}

/// The total promoted text of `safe`, in bytes. Used by the budget test and by
/// the sweep's log line, never by the promotion body.
pub fn excerpt_bytes(safe: &SafeIssue) -> usize {
    safe.short_id.len()
        + safe.project.len()
        + safe.kind.len()
        + safe.value.len()
        + safe.culprit.len()
        + safe.transaction.len()
        + safe.level.len()
        + safe.release.len()
        + safe.permalink.len()
        + safe
            .frames
            .iter()
            .map(|frame| frame.filename.len() + frame.function.len())
            .sum::<usize>()
}

#[cfg(test)]
#[path = "redact_test.rs"]
mod test;
