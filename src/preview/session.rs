//! One browser session, as the server remembers it between HTTP calls.
//!
//! The hands call the brain once per turn and once at the end, and each call
//! must find the flow where the last one left it: the planned flows, every
//! flow's transcript and command script, and what has been spent so far.
//! That is this struct. It is written to the store after every call and read
//! back before the next, so a redeploy between two turns loses nothing but
//! the turn in flight.
//!
//! Always compiled, and plain data: the store persists it as a document and
//! the CLI can print one, but nothing here knows what a database is.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::preview::step::FlowState;
use crate::preview::types::Flow;

/// A session in progress.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Session {
    /// The session id the hands present on every call. Unguessable.
    ///
    /// Serialised as `_id` so the document the store writes is keyed by it
    /// without a second copy of the same string under another name.
    #[serde(rename = "_id")]
    pub id: String,
    /// `owner/name`.
    pub repo: String,
    /// The pull request number.
    pub number: u64,
    /// The head commit the hands built.
    pub head_sha: String,
    /// The merge-base the hands built.
    pub base_sha: String,
    /// The GitHub App installation that can read and write this repository.
    pub installation: u64,
    /// The flows planned for this commit, in order.
    pub flows: Vec<Flow>,
    /// The driving state of every flow that has started, by flow id.
    #[serde(default)]
    pub states: BTreeMap<String, FlowState>,
    /// The UI diff shown to the driver and the captioner, already truncated.
    pub diff_excerpt: String,
    /// What the session has spent so far, in USD.
    #[serde(default)]
    pub spent_usd: f64,
    /// The largest number of steps any flow may take.
    pub max_steps: usize,
}

impl Session {
    /// The flow with this id, if it was planned.
    pub fn flow(&self, id: &str) -> Option<&Flow> {
        self.flows.iter().find(|flow| flow.id == id)
    }

    /// Whether the budget is spent.
    pub fn exhausted(&self, budget_usd: f64) -> bool {
        self.spent_usd >= budget_usd
    }
}

/// The most UI diff the driver and the captioner are shown, in characters.
///
/// Smaller than the planner's ceiling: this text rides in every turn of every
/// flow, so its size is multiplied by the step count.
pub const MAX_EXCERPT_CHARS: usize = 20_000;

/// The diff excerpt every later prompt carries.
pub fn excerpt(diffs: &[crate::evidence::diff::FileDiff]) -> String {
    let ui: Vec<crate::evidence::diff::FileDiff> = diffs
        .iter()
        .filter(|diff| crate::preview::plan::is_ui_path(&diff.path))
        .cloned()
        .collect();
    let mut rendered = crate::evidence::diff::render(&ui);
    if rendered.chars().count() > MAX_EXCERPT_CHARS {
        rendered = rendered.chars().take(MAX_EXCERPT_CHARS).collect();
        rendered.push_str("\n… (diff truncated)\n");
    }
    rendered
}

/// A fresh, unguessable session id.
///
/// Thirty-two hex characters of SHA-256 over the session's identity and a
/// timestamp; the hash is what makes it unguessable, and the identity is what
/// makes two sessions for two commits never collide.
pub fn new_id(repo: &str, number: u64, head_sha: &str) -> String {
    use sha2::{Digest, Sha256};
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let digest = Sha256::digest(format!("{repo}#{number}@{head_sha}:{nanos}:{}", std::process::id()));
    digest
        .iter()
        .take(16)
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_hex_and_do_not_repeat() {
        let a = new_id("o/r", 1, "abc");
        let b = new_id("o/r", 1, "abc");
        assert_eq!(a.len(), 32);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }

    #[test]
    fn a_session_round_trips_through_json() {
        let session = Session {
            id: new_id("o/r", 7, "abc"),
            repo: "o/r".into(),
            number: 7,
            head_sha: "abc".into(),
            base_sha: "base".into(),
            installation: 42,
            flows: vec![],
            states: BTreeMap::new(),
            diff_excerpt: "d".into(),
            spent_usd: 0.1,
            max_steps: 25,
        };
        let json = serde_json::to_string(&session).unwrap();
        assert_eq!(serde_json::from_str::<Session>(&json).unwrap(), session);
    }
}
