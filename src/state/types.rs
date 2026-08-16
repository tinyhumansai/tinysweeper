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
        assert_eq!(decoded.titles, vec!["Guard the index".to_string()]);
    }
}
