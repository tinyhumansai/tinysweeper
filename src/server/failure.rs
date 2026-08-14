//! What a pull request is told when its review could not run.
//!
//! Until this module existed, a failed review was a log line and nothing else:
//! `routes::handle_review` caught the error, wrote it to the pod's stdout, and
//! left the pull request untouched. To everything downstream — a human
//! scrolling the checks list, and the auto-merge gate reading `check_runs` —
//! that is indistinguishable from a pull request tinysweeper had no opinion on.
//!
//! That is not a hypothetical. Over 2026-08-12/13 the review gateway spent
//! roughly fourteen hours returning HTTP 403 and 402 for every model in the
//! fallback chain; 120 reviews failed, and not one of them left a mark on the
//! pull request it was reviewing. The outage was found by reading pod logs a
//! day later, because from GitHub's side a total model outage and a clean bill
//! of health render identically.
//!
//! So this module exists to make silence impossible. A review that could not
//! run publishes a blocking check saying so, and the failure becomes a fact
//! about the pull request rather than a fact about the pod.

use crate::error::Error;
use crate::forge::types::{CheckConclusion, CheckRun};
use crate::server::status::CHECK_NAME;

/// How many times a transient failure is retried before it is reported.
///
/// Three attempts, not more. Retrying is only ever the right answer for a
/// failure that clears in seconds — a secondary rate limit, a 5xx, a dropped
/// connection. The failure that motivated this module, a gateway daily cap,
/// takes *hours* to clear, and no retry budget worth spending survives it.
/// That is precisely why the check run below is the load-bearing half of this
/// module and the retry is the cheap half.
pub const MAX_ATTEMPTS: u32 = 3;

/// How long to wait before attempt `attempt` (1-based), in milliseconds.
///
/// Exponential from two seconds. The ceiling matters more than the curve: a
/// review holds a concurrency permit while it waits, so a long backoff here
/// converts one sick pull request into a stalled queue.
pub fn backoff_ms(attempt: u32) -> u64 {
    2_000_u64 << attempt.min(3).saturating_sub(1)
}

/// Whether `err` is worth another attempt.
pub fn is_transient(err: &Error) -> bool {
    match err {
        // A gateway that is rate limited, briefly unreachable, or returning
        // 5xx fails every model in the chain at once and recovers on its own.
        // A gateway that is out of credit lands here too and will *not* clear
        // inside the retry budget — it is reported rather than recovered, and
        // that is the intended outcome, not a gap.
        Error::Model(_) => true,
        // GitHub secondary rate limits and 5xx arrive here. This variant also
        // covers a genuinely malformed request, which no retry fixes; three
        // wasted calls is the price of not dropping the transient majority.
        Error::Forge(_) => true,
        // Everything else is deterministic — a bug, a bad config, or a budget
        // ceiling — so a retry reproduces it exactly and costs real money
        // doing so. `Budget` in particular must never be retried: the run
        // already spent its limit.
        _ => false,
    }
}

/// Strip credential-shaped material out of text bound for a pull request.
///
/// Model and forge errors are quoted verbatim from an upstream response, and
/// upstream puts identifiers in them. The gateway's own quota error embeds a
/// key-management URL ending in the key's id — harmless to a reader, but it is
/// account-identifying material on what may be a public pull request, and the
/// security boundary in `AGENTS.md` says location and type, never the value.
///
/// Word-level rather than pattern-level on purpose: anything that looks like a
/// URL or carries enough entropy to be a token is dropped whole, so a novel
/// secret shape is scrubbed by default instead of surviving until someone
/// writes a rule for it.
pub fn scrub(text: &str) -> String {
    text.split_whitespace()
        .map(|word| {
            let bare = word.trim_matches(|c: char| !c.is_alphanumeric());
            if word.contains("://") || bare.len() >= 32 && is_opaque(bare) {
                "<redacted>"
            } else {
                word
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Whether `word` reads as an opaque token rather than prose.
///
/// A long run of hex or base62 with no separators is a key, a hash or an id;
/// English words that long do not exist without a hyphen or a space.
fn is_opaque(word: &str) -> bool {
    word.chars().all(|c| c.is_ascii_alphanumeric())
        && word.chars().any(|c| c.is_ascii_digit())
        && word.chars().any(|c| c.is_ascii_alphabetic())
}

/// The check run that reports a review which could not be produced.
///
/// The conclusion is [`CheckConclusion::ActionRequired`] rather than `Failure`
/// or `Skipped`, and the choice is the whole point of this module.
/// `CheckConclusion::blocks` is true for `ActionRequired`, so the auto-merge
/// gate stops treating the absence of a review as consent — which is the bug.
/// `Skipped` reads better in English and would have kept exactly that bug,
/// since it does not block. `Failure` is wrong in the other direction: it
/// claims tinysweeper found something disqualifying in the code, when what
/// actually happened is that it never got to look.
pub fn check_run(head_sha: &str, err: &Error) -> CheckRun {
    CheckRun {
        name: CHECK_NAME.to_string(),
        head_sha: head_sha.to_string(),
        conclusion: Some(CheckConclusion::ActionRequired),
        title: title_for(err).to_string(),
        summary: summary_for(err),
    }
}

/// The one-line title shown in the checks list.
fn title_for(err: &Error) -> &'static str {
    match err {
        Error::Model(_) => "The review could not reach a model",
        Error::Forge(_) => "The review could not read the pull request",
        Error::Budget { .. } => "The review ran out of budget",
        Error::Config(_) | Error::ConfigNotFound(_) => "The review is misconfigured",
        _ => "The review could not run",
    }
}

/// The markdown body, written for whoever opens the check on the pull request.
///
/// It says three things and no more: that nothing was reviewed, what went
/// wrong, and that this check is not a verdict on the code. The last one earns
/// its space — a red check on a pull request reads as an accusation unless it
/// explicitly says otherwise.
fn summary_for(err: &Error) -> String {
    let remedy = match err {
        Error::Model(_) => {
            "The model gateway rejected every model in the fallback chain. This is usually \
             credit, a per-key daily cap, or a provider outage — check the gateway's key \
             status before re-running."
        }
        Error::Forge(_) => {
            "GitHub rejected a read the review depends on. If this persists, check the app \
             installation's permissions on this repository."
        }
        Error::Budget { .. } => {
            "The per-pull-request spend ceiling was reached before the lanes finished. Raise \
             `models.budget_usd_per_pr`, or narrow what this pull request changes."
        }
        _ => "Re-run the review once the underlying problem is fixed.",
    };

    format!(
        "**No code was reviewed.** This check reports that tinysweeper failed to run — it is \
         not a finding about this pull request, and it says nothing about whether the change \
         is correct.\n\n\
         ```\n{}\n```\n\n\
         {remedy}\n\n\
         It blocks auto-merge on purpose: an unreviewed pull request must not be \
         indistinguishable from a reviewed one. Re-run the review from the repository's \
         **Manual review** workflow, or merge by hand if you have reviewed it yourself.",
        scrub(&err.to_string())
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failed_review_blocks_rather_than_passing_quietly() {
        // The whole reason this module exists. If this assertion ever flips,
        // a model outage silently reads as consent to auto-merge again.
        let check = check_run("abc123", &Error::Model("gateway returned 403".into()));
        assert_eq!(check.conclusion, Some(CheckConclusion::ActionRequired));
        assert!(
            check.conclusion.is_some_and(CheckConclusion::blocks),
            "a review that never ran must not be mistaken for a passing one"
        );
        assert!(
            !check.is_in_progress(),
            "a reported failure is terminal; leaving it pending would stall the merge gate"
        );
    }

    #[test]
    fn the_auto_merge_gate_reads_this_check_as_failing() {
        // The assertion above is about the conclusion; this one is about the
        // gate. `automerge::policy::check_refusal` scans *every* check on the
        // head SHA and refuses on the first `is_failing()`, so this check
        // blocks a merge without having to be named in `require_checks` —
        // which matters, because no reviewed repository lists it today.
        let check = check_run("abc123", &Error::Model("gateway returned 403".into()));
        let observed = crate::forge::types::CheckStatus {
            name: check.name.clone(),
            conclusion: check.conclusion,
        };
        assert!(observed.is_failing(), "the gate must refuse on this check");
        assert!(
            !observed.is_inapplicable(),
            "a failed review is not a lane reporting it had nothing to say"
        );
    }

    #[test]
    fn the_summary_says_it_is_not_a_verdict_on_the_code() {
        // A red check with no disclaimer reads as an accusation against the
        // contributor for something tinysweeper never even looked at.
        let check = check_run("abc123", &Error::Model("gateway returned 403".into()));
        assert!(check.summary.contains("No code was reviewed"));
        assert!(
            check
                .summary
                .contains("not a finding about this pull request")
        );
    }

    #[test]
    fn the_gateways_key_url_never_reaches_the_pull_request() {
        // The exact string the outage produced, 14,675 times.
        let err = Error::Model(
            "openai returned HTTP 403: Key limit exceeded (daily limit). Manage it using \
             https://openrouter.ai/workspaces/default/keys/dbc1273b6d25c6b5155c2cc14a33a7a22"
                .into(),
        );
        let summary = summary_for(&err);
        assert!(!summary.contains("openrouter.ai"));
        assert!(!summary.contains("dbc1273b6d25c6b5155c2cc14a33a7a22"));
        // The half a human needs is still there.
        assert!(summary.contains("Key limit exceeded"));
    }

    #[test]
    fn scrub_drops_opaque_tokens_but_keeps_prose() {
        let scrubbed = scrub("failed with token a1b2c3d4e5f60718293a4b5c6d7e8f90 while reading");
        assert!(!scrubbed.contains("a1b2c3d4e5f60718293a4b5c6d7e8f90"));
        assert_eq!(scrubbed, "failed with token <redacted> while reading");
    }

    #[test]
    fn scrub_leaves_ordinary_long_words_alone() {
        // No digits, so it is prose however long it is. Without this the
        // summary would redact its own explanation.
        let text = "the indistinguishable configuration was uninterpretable";
        assert_eq!(scrub(text), text);
    }

    #[test]
    fn only_the_recoverable_failures_are_retried() {
        assert!(is_transient(&Error::Model("502".into())));
        assert!(is_transient(&Error::Forge("secondary rate limit".into())));
        // Spending more money to reproduce a spend ceiling is the one retry
        // that is actively harmful.
        assert!(!is_transient(&Error::Budget {
            spent: 4.25,
            limit: 4.0
        }));
        assert!(!is_transient(&Error::Config("bad preset".into())));
        assert!(!is_transient(&Error::lane("critique", "no verdict")));
    }

    #[test]
    fn backoff_grows_and_then_stops_growing() {
        assert_eq!(backoff_ms(1), 2_000);
        assert_eq!(backoff_ms(2), 4_000);
        assert_eq!(backoff_ms(3), 8_000);
        // A review holds a concurrency permit while it waits, so the wait is
        // capped rather than doubling forever.
        assert_eq!(backoff_ms(9), 8_000);
    }

    #[test]
    fn each_failure_class_gets_its_own_remedy() {
        // A summary that says "something went wrong" for every cause wastes
        // the one place an operator actually looks.
        let model = summary_for(&Error::Model("403".into()));
        let forge = summary_for(&Error::Forge("404".into()));
        assert!(model.contains("per-key daily cap"));
        assert!(forge.contains("installation's permissions"));
        assert_ne!(
            title_for(&Error::Model("x".into())),
            title_for(&Error::Forge("y".into()))
        );
    }
}
