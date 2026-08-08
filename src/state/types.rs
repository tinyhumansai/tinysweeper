//! What one pull request's last review left behind.

use serde::{Deserialize, Serialize};

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
        };
        let encoded = serde_json::to_string(&state).expect("serialises");
        let decoded: ReviewedState = serde_json::from_str(&encoded).expect("deserialises");
        assert_eq!(decoded, state);
    }
}
