//! Core types for the falsification pass, and the schema it answers with.

use serde::Deserialize;
use serde_json::{Value, json};

use crate::config::types::LaneId;
use crate::findings::types::Finding;
use crate::ports::model::Usage;

/// What the filter answers with.
///
/// One field, deliberately: the pass can only reject. Anything that let it
/// return a finding would let it return a *changed* finding, and a filter that
/// can rewrite what it filters is a second reviewer nobody gated.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct FalsifyResponse {
    /// Findings the diff disproves.
    #[serde(default)]
    pub incorrect: Vec<Incorrect>,
}

/// One rejection, as the model reports it.
#[derive(Debug, Clone, Deserialize)]
pub struct Incorrect {
    /// The finding's 1-based position in the list it was shown.
    pub index: u64,
    /// What in the diff disproves it.
    #[serde(default)]
    pub reason: String,
}

/// A finding the pass removed, kept for the run log.
///
/// Recorded rather than discarded: a filter that silently deletes findings is
/// indistinguishable from a filter that is broken.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rejection {
    /// The lane whose finding this was.
    pub lane: LaneId,
    /// The finding's title.
    pub title: String,
    /// What the filter said disproves it.
    pub reason: String,
}

/// What the pass concluded.
#[derive(Debug, Clone, Default)]
pub struct FalsifyOutcome {
    /// The findings that survived.
    pub findings: Vec<Finding>,
    /// The findings the diff disproved.
    pub rejected: Vec<Rejection>,
    /// What the call cost.
    pub usage: Usage,
    /// Set when the pass could not run and let everything through.
    pub failed_open: Option<String>,
}

impl FalsifyOutcome {
    /// An outcome that rejected nothing, without having called anything.
    pub fn kept(findings: Vec<Finding>) -> Self {
        Self {
            findings,
            ..Self::default()
        }
    }

    /// An outcome where the pass broke and every finding survived.
    pub fn failed_open(findings: Vec<Finding>, reason: impl Into<String>) -> Self {
        Self {
            findings,
            failed_open: Some(reason.into()),
            ..Self::default()
        }
    }
}

/// The schema the filter answers with.
pub fn json_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["incorrect"],
        "properties": {
            "incorrect": {
                "type": "array",
                "description": "The findings the diff disproves. An empty array is the normal answer; do not pad it. Include a finding here only if the diff itself shows it to be wrong, never because you are unsure of it.",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["index", "reason"],
                    "properties": {
                        "index": {
                            "type": "integer",
                            "minimum": 1,
                            "description": "The finding's number in the list you were shown, counting from 1."
                        },
                        "reason": {
                            "type": "string",
                            "description": "The specific thing in the diff that disproves it. Quote the line. `I am not convinced` is not a reason."
                        }
                    }
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_answer_parses_as_rejecting_nothing() {
        let parsed: FalsifyResponse = serde_json::from_value(json!({"incorrect": []})).unwrap();
        assert!(parsed.incorrect.is_empty());
    }

    #[test]
    fn a_missing_reason_is_tolerated_rather_than_losing_the_rejection() {
        let parsed: FalsifyResponse =
            serde_json::from_value(json!({"incorrect": [{"index": 2}]})).unwrap();
        assert_eq!(parsed.incorrect[0].index, 2);
        assert!(parsed.incorrect[0].reason.is_empty());
    }

    #[test]
    fn the_schema_forbids_the_filter_returning_findings_of_its_own() {
        let schema = json_schema();
        assert_eq!(schema["additionalProperties"], json!(false));
        let properties = schema["properties"].as_object().expect("object");
        assert_eq!(
            properties.keys().collect::<Vec<_>>(),
            vec!["incorrect"],
            "the pass may only reject"
        );
    }

    #[test]
    fn a_failed_open_outcome_keeps_every_finding() {
        let outcome = FalsifyOutcome::failed_open(vec![], "boom");
        assert_eq!(outcome.failed_open.as_deref(), Some("boom"));
    }
}
