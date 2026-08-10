//! Step 3: open the GitHub issue.
//!
//! Everything written here comes off a [`SafeIssue`], which is the only type
//! this module accepts — there is no overload taking a [`RawIssue`], so the
//! redaction boundary cannot be routed around by a caller in a hurry.
//!
//! ## No model call, and none needed
//!
//! The spec forbids a model call on raw event text, and there is no useful one
//! to make on scrubbed text either: the priority follows Sentry's own `level`
//! and the classification is always `bug`. That mirrors
//! [`crate::issues::pull_request`], which likewise derives a label from
//! evidence already computed rather than paying for a second opinion that
//! could contradict what is published beside it.
//!
//! ## Untrusted text is fenced
//!
//! An exception message is captured from a running process and can contain
//! anything, including backticks and Markdown. Every free-text field goes
//! inside a code fence sized to be longer than the longest backtick run in the
//! content, so a crafted message renders as data rather than restructuring the
//! issue body. The same rule `AGENTS.md` states for diffs in prompts.

use crate::config::types::Config;
use crate::error::Result;
use crate::forge::types::RepoId;
use crate::issues::kind;
use crate::issues::types::{IssueKind, Priority};
use crate::ports::forge::{ForgeRead, ForgeWrite};
use crate::sentry::dedupe;
use crate::sentry::types::SafeIssue;

/// How many characters of the exception message reach the issue title.
///
/// A title is a list-view affordance, not the report. GitHub truncates in the
/// UI anyway; truncating here means the cut is ours and lands on a word.
const TITLE_VALUE_CHARS: usize = 80;

/// Map Sentry's level onto tinysweeper's priority vocabulary.
///
/// **Level only.** Sentry's level is the one severity assertion in the
/// payload, made by the application that raised the error; deriving priority
/// from event or user counts instead would invent a threshold nobody chose and
/// would make a widespread warning outrank a rare crash. An unrecognised or
/// absent level is P3 rather than a guess.
pub fn priority(level: &str) -> Priority {
    match level.trim().to_ascii_lowercase().as_str() {
        "fatal" => Priority::P0,
        "error" => Priority::P1,
        "warning" => Priority::P2,
        _ => Priority::P3,
    }
}

/// The issue title.
///
/// Shaped so a list view answers "what broke, and where do I look it up"
/// without opening anything. Newlines are collapsed: a title is one line, and
/// an exception message with a newline in it would otherwise truncate the
/// title at the break with no indication anything followed.
pub fn title(safe: &SafeIssue) -> String {
    let kind = if safe.kind.is_empty() {
        "Sentry issue"
    } else {
        safe.kind.as_str()
    };

    let message = collapse_whitespace(&safe.value);
    let message = truncate_on_char(&message, TITLE_VALUE_CHARS);

    if message.is_empty() {
        format!("[sentry] {kind} ({})", safe.short_id)
    } else {
        format!("[sentry] {kind}: {message} ({})", safe.short_id)
    }
}

/// The issue body, ending in the durable dedupe marker.
///
/// The marker goes last so a human editing the report cannot easily lose it,
/// and so `body.contains(marker)` stays true through ordinary edits — that
/// substring check is what [`crate::sentry::dedupe`] actually decides on.
pub fn body(safe: &SafeIssue, org: &str) -> String {
    let mut out = String::new();

    out.push_str("Promoted from Sentry by tinysweeper.\n\n");

    if !safe.value.is_empty() {
        out.push_str(&fenced(&safe.value, "text"));
        out.push('\n');
    }

    out.push_str("| | |\n| --- | --- |\n");
    row(&mut out, "Sentry", &sentry_link(safe));
    row(&mut out, "Project", &code_span(&safe.project));
    if !safe.level.is_empty() {
        row(&mut out, "Level", &code_span(&safe.level));
    }
    row(&mut out, "Events", &safe.count.to_string());
    row(&mut out, "Users affected", &safe.user_count.to_string());
    if !safe.release.is_empty() {
        row(&mut out, "Release", &code_span(&safe.release));
    }
    if !safe.transaction.is_empty() {
        row(&mut out, "Transaction", &code_span(&safe.transaction));
    }
    if !safe.culprit.is_empty() {
        row(&mut out, "Culprit", &code_span(&safe.culprit));
    }
    out.push('\n');

    if !safe.frames.is_empty() {
        out.push_str("### Stack\n\n");
        let rendered = safe
            .frames
            .iter()
            .rev()
            .map(|frame| match frame.lineno {
                Some(line) => format!("{}:{} in {}", frame.filename, line, frame.function),
                None => format!("{} in {}", frame.filename, frame.function),
            })
            .collect::<Vec<_>>()
            .join("\n");
        out.push_str(&fenced(&rendered, "text"));
        out.push('\n');
    }

    out.push_str(
        "_File, function and line only: Sentry event payloads are not promoted, \
         because they carry request bodies, headers, cookies and frame locals._\n\n",
    );

    out.push_str(&dedupe::marker(org, &safe.project, &safe.short_id));
    out.push('\n');

    out
}

/// Open the issue, apply labels, and set the native issue type.
///
/// Returns the new issue number. The type is set in a second call because
/// GitHub's issue-creation API does not carry it; a failure to set it is
/// logged and does not undo the promotion, since an untyped tracked issue is
/// far better than a lost one.
pub async fn promote(
    read: &dyn ForgeRead,
    write: &dyn ForgeWrite,
    config: &Config,
    repo: &RepoId,
    org: &str,
    safe: &SafeIssue,
) -> Result<u64> {
    let mut labels = config.sentry.labels_for(&safe.project);
    let priority_label = priority(&safe.level).label().to_string();
    if !labels.contains(&priority_label) {
        labels.push(priority_label);
    }

    // Applied directly rather than through `issues::labels::plan`. That
    // planner exists to bound *model-suggested* labels on somebody else's
    // issue — `max_labels` is a noise budget for suggestions. These labels are
    // deterministic deployment configuration on an issue tinysweeper is
    // opening itself, and running them through the budget could silently drop
    // the `sentry` label the operator configured, which is the one that makes
    // the promoted set findable.
    let number = write
        .create_issue(repo, &title(safe), &body(safe, org), &labels)
        .await?;

    set_issue_type(read, write, config, repo, number).await;

    Ok(number)
}

/// Set the native issue type, reusing the issue-triage decision rules.
///
/// A promoted Sentry issue is a bug by construction, so the classification is
/// not a judgement call. Everything else — the owner defining no types, no
/// type matching the word, the feature being off — is
/// [`crate::issues::kind::plan`]'s existing refusal set, reused rather than
/// restated so the two paths cannot diverge.
async fn set_issue_type(
    read: &dyn ForgeRead,
    write: &dyn ForgeWrite,
    config: &Config,
    repo: &RepoId,
    number: u64,
) {
    let available = match read.issue_types(repo).await {
        Ok(types) => types,
        Err(err) => {
            tracing::debug!(%err, "could not read issue types; leaving the promoted issue untyped");
            return;
        }
    };

    // `current` is None: the issue was created moments ago and cannot already
    // carry a type.
    match kind::plan(
        Some(IssueKind::Bug),
        None,
        &available,
        config.issues.apply_issue_type,
    ) {
        kind::Decision::Set(name) => {
            if let Err(err) = write.set_issue_type(repo, number, &name).await {
                tracing::warn!(number, %err, "could not set the issue type on a promoted issue");
            }
        }
        kind::Decision::Skip(reason) => {
            tracing::debug!(number, reason, "not setting an issue type");
        }
    }
}

/// A Markdown link to the Sentry issue, or the bare short id when the
/// permalink is missing.
fn sentry_link(safe: &SafeIssue) -> String {
    if safe.permalink.is_empty() {
        code_span(&safe.short_id)
    } else {
        // The short id is Sentry-generated and scrubbed, so it cannot carry
        // Markdown syntax; the permalink is angle-bracketed so a URL with
        // parentheses cannot terminate the link early.
        format!("[{}](<{}>)", safe.short_id, safe.permalink)
    }
}

fn row(out: &mut String, name: &str, value: &str) {
    out.push_str("| ");
    out.push_str(name);
    out.push_str(" | ");
    out.push_str(value);
    out.push_str(" |\n");
}

/// Render `text` as an inline code span, escaping any backtick and pipe.
///
/// The pipe matters: these land in Markdown table cells, where a raw `|`
/// starts a new column and would shear the table.
fn code_span(text: &str) -> String {
    let cleaned = collapse_whitespace(text)
        .replace('`', "'")
        .replace('|', "\\|");
    format!("`{cleaned}`")
}

/// Wrap `text` in a fence guaranteed to be longer than any backtick run
/// inside it, so untrusted content cannot escape the block.
fn fenced(text: &str, language: &str) -> String {
    let longest_run = text
        .split(|c| c != '`')
        .map(str::len)
        .max()
        .unwrap_or_default();
    let fence = "`".repeat(longest_run.max(2) + 1);
    format!("{fence}{language}\n{text}\n{fence}\n")
}

/// Collapse every whitespace run to a single space and trim.
fn collapse_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Truncate to `max` characters on a character boundary, adding an ellipsis.
fn truncate_on_char(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let truncated: String = text.chars().take(max).collect();
    format!("{}…", truncated.trim_end())
}

#[cfg(test)]
#[path = "promote_test.rs"]
mod test;
