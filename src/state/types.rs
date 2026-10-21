//! What one pull request's last review left behind.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::config::types::Severity;

/// The state of the most recent review of a pull request.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewedState {
    /// The head SHA that review ran against.
    ///
    /// Kept so a re-delivery for the same commit is recognisable, and so the
    /// replay below can be trusted to describe that commit and no other.
    pub head_sha: String,
    /// The rendered evidence sent to the lanes, verbatim.
    ///
    /// Replayed byte-for-byte as prompt layer 3. Anything that reformats this
    /// between runs — a changed renderer, a normalisation pass — costs the
    /// cache hit without changing any output, which is the kind of regression
    /// no test notices. See `crate::evidence::replay`.
    pub evidence: String,
    /// Fingerprints of every finding posted so far, across all pushes.
    ///
    /// Accumulated rather than replaced: a finding posted three pushes ago is
    /// still on the pull request, and re-posting it is exactly the noise this
    /// exists to prevent.
    pub fingerprints: Vec<String>,
    /// Titles of the findings still standing, for prompt layer 4.
    pub titles: Vec<String>,
    /// The severity each of those titles was reported at.
    ///
    /// Carried so a re-review reports an unchanged finding at the level it
    /// already has. Without it severity is re-decided from nothing on every
    /// push, and one concern drifts through medium, high and critical while the
    /// code it is about sits still.
    ///
    /// `default` because it was added after the first states were written, and
    /// a state that predates it must still deserialize — an empty map means
    /// "nothing to pin", which is exactly the old behaviour.
    #[serde(default)]
    pub severities: BTreeMap<String, Severity>,
    /// What the `e2e` lane left pending, so a later check completion can
    /// settle its check run without re-running the review.
    ///
    /// `None` when that lane is off, skipped, or already concluded. Keyed to
    /// its own head SHA inside, so a completion for an older commit is
    /// ignored rather than applied to the wrong review.
    #[serde(default)]
    pub e2e: Option<crate::lanes::e2e::runs::Watch>,
    /// The durable review comment, avoiding a discovery read on later passes.
    #[serde(default)]
    pub hub_comment_id: Option<u64>,
    /// Last trustworthy structured summary, used for continuity and history.
    #[serde(default)]
    pub summary: Option<crate::summary::ReviewSummary>,
    /// Exact serialized assistant turns retained as a byte-stable cache prefix.
    #[serde(default)]
    pub summary_transcript: Vec<crate::summary::SummaryTranscriptTurn>,
    /// The last verdict and blocking-finding set successfully published by apply.
    ///
    /// Missing on state written before review-level deduplication. An absent
    /// record is unknown, so it never suppresses a review.
    #[serde(default)]
    pub published_review: Option<PublishedReview>,
    /// Exact usage from the most recent apply pass, including a deduped one.
    #[serde(default)]
    pub latest_usage: Option<ReviewUsage>,
}

/// A successfully published review decision, used to suppress identical repeats.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublishedReview {
    /// Verdict submitted to the forge.
    pub event: crate::forge::types::ReviewEvent,
    /// Reviewed head when the decision was last evaluated.
    pub head_sha: String,
    /// Stable identities of the findings that contribute to a blocking verdict.
    pub blocking_findings: Vec<String>,
    /// Standing forge review expected after this publication. A COMMENT may
    /// leave an older approval or changes request in force, so its publication
    /// head alone cannot authenticate the next dedupe decision.
    #[serde(default)]
    pub standing_review: Option<crate::forge::types::OwnReview>,
    /// Whether the successfully published proposal had complete coverage.
    /// Missing metadata is unknown and cannot certify a current coverage warning.
    #[serde(default)]
    pub coverage_complete: Option<bool>,
}

/// Provider accounting retained without putting the breakdown in review prose.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewUsage {
    /// Lossless decimal form of the reported USD total.
    pub cost_usd: String,
    /// Prompt tokens, including cache-served tokens.
    pub input_tokens: u64,
    /// Generated tokens.
    pub output_tokens: u64,
    /// Prompt tokens served from provider cache.
    pub cached_tokens: u64,
    /// Embedding tokens.
    pub embed_tokens: u64,
    /// Model identifiers as reported, including blank values.
    pub models: Vec<String>,
}

/// The key a pull request's state is stored under.
pub fn key(repo: &str, number: u64) -> String {
    format!("{repo}#{number}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_key_names_the_repository_and_the_pull_request() {
        assert_eq!(
            key("tinyhumansai/tinysweeper", 7),
            "tinyhumansai/tinysweeper#7"
        );
    }

    #[test]
    fn state_round_trips_through_json() {
        let state = ReviewedState {
            head_sha: "abc123".into(),
            evidence: "--- a.rs\n@@ -1 +1 @@\n    1 +x\n".into(),
            fingerprints: vec!["0123456789abcdef".into()],
            titles: vec!["Guard the index".into()],
            severities: BTreeMap::from([("Guard the index".to_string(), Severity::High)]),
            e2e: None,
            hub_comment_id: Some(42),
            summary: None,
            summary_transcript: vec![],
            published_review: None,
            latest_usage: None,
        };
        let encoded = serde_json::to_string(&state).expect("serialises");
        let decoded: ReviewedState = serde_json::from_str(&encoded).expect("deserialises");
        assert_eq!(decoded, state);
    }

    #[test]
    fn a_state_written_before_severities_existed_still_loads() {
        // The field arrived after states were already in the store, and a TTL
        // measured in days means old records outlive the deploy that adds it. A
        // record that failed to deserialize would be discarded silently and the
        // pull request re-reviewed from scratch — more expensive, and it would
        // lose exactly the continuity the field was added to keep.
        let old =
            r#"{"head_sha":"abc123","evidence":"","fingerprints":[],"titles":["Guard the index"]}"#;
        let decoded: ReviewedState = serde_json::from_str(old).expect("deserialises");
        assert!(decoded.severities.is_empty());
        assert!(decoded.published_review.is_none());
        assert!(decoded.latest_usage.is_none());
        assert_eq!(decoded.titles, vec!["Guard the index".to_string()]);
    }

    #[test]
    fn a_publication_receipt_without_a_standing_review_remains_unknown() {
        let receipt = PublishedReview {
            event: crate::forge::types::ReviewEvent::Comment,
            head_sha: "comment-head".into(),
            blocking_findings: vec![],
            standing_review: Some(crate::forge::types::OwnReview {
                event: crate::forge::types::ReviewEvent::RequestChanges,
                head_sha: Some("blocked-head".into()),
            }),
            coverage_complete: Some(true),
        };
        let mut encoded = serde_json::to_value(&receipt).expect("serializes");
        encoded
            .as_object_mut()
            .expect("receipt object")
            .remove("standing_review");
        encoded
            .as_object_mut()
            .expect("receipt object")
            .remove("coverage_complete");
        let decoded: PublishedReview = serde_json::from_value(encoded).expect("legacy receipt");
        assert!(decoded.standing_review.is_none());
        assert!(decoded.coverage_complete.is_none());
    }
}
