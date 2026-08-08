//! The structured-output schema every lane answers with.
//!
//! Lanes never parse prose. The model is constrained to this JSON schema, and
//! anything that does not fit is a hard error rather than a best-effort parse —
//! a review bot that guesses at malformed output is a review bot that posts
//! nonsense on someone's pull request.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::config::types::{LaneId, Severity};
use crate::error::{Error, Result};
use crate::findings::types::Finding;
use crate::scan::secrets::scrub;

/// What a lane's model call returns.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LaneResponse {
    /// One or two sentences for the check-run summary.
    #[serde(default)]
    pub summary: String,
    /// The findings, possibly none.
    #[serde(default)]
    pub findings: Vec<RawFinding>,
    /// Earlier findings the model verified as fixed.
    ///
    /// Part of the continuity contract: a re-review has to account for what it
    /// said last time rather than quietly moving on.
    #[serde(default)]
    pub resolved: Vec<String>,
}

/// A finding as the model reports it, before any filtering.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RawFinding {
    /// The file, exactly as it appears in the diff.
    pub path: String,
    /// The code the finding is about, quoted verbatim from the diff.
    ///
    /// This is how a finding is anchored. Models are bad at line arithmetic
    /// and good at copying, so the model quotes and `src/position` computes
    /// the range — see that module for why the reverse loses good findings.
    #[serde(default)]
    pub existing_code: Option<String>,
    /// The head-revision line it anchors to.
    ///
    /// No longer part of the schema the model answers: it is filled in by
    /// `src/position` after the fact, and stays deserializable only so
    /// proposals written by an older version still parse.
    #[serde(default)]
    pub line: Option<u64>,
    /// The last line, when it spans a range. Also computed host-side.
    #[serde(default)]
    pub end_line: Option<u64>,
    /// A stable category id.
    pub rule: String,
    /// Imperative, at most 80 characters.
    pub title: String,
    /// The explanation.
    pub body: String,
    /// How much it matters.
    pub severity: Severity,
    /// How sure the model is, 0.0 to 1.0.
    pub confidence: f64,
    /// A concrete replacement for the anchored lines.
    #[serde(default)]
    pub suggestion: Option<String>,
    /// Whether this is about code the pull request did not change.
    #[serde(default)]
    pub late: bool,
}

impl RawFinding {
    /// Promote to a canonical [`Finding`] belonging to `lane`.
    ///
    /// Every free-text field is scrubbed of credential shapes on the way
    /// through. The model reads the raw diff, so a pull request that commits a
    /// key gives the model something it can quote back — and redacting scanner
    /// findings carefully counts for nothing if the critique lane prints the
    /// value two comments later.
    pub fn into_finding(self, lane: LaneId) -> Finding {
        Finding {
            lane,
            severity: self.severity,
            // A model that reports confidence outside the range is confused
            // about something; clamping keeps the gate arithmetic honest
            // rather than letting a 5.0 sail past every threshold.
            confidence: self.confidence.clamp(0.0, 1.0),
            path: self.path,
            line: self.line,
            end_line: self.end_line,
            rule: self.rule,
            title: truncate(&scrub(&self.title), 80),
            body: scrub(&self.body),
            suggestion: self.suggestion.as_deref().map(scrub),
            late: self.late,
        }
    }
}

/// Parse a model response, failing loudly on anything unusable.
pub fn parse(lane: LaneId, value: Value) -> Result<LaneResponse> {
    serde_json::from_value(value).map_err(|err| {
        Error::lane(
            lane.as_str(),
            format!("the model's response did not match the schema: {err}"),
        )
    })
}

/// The JSON schema attached to every lane request.
pub fn json_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["summary", "findings"],
        "properties": {
            "summary": {
                "type": "string",
                "description": "Your final verdict in one or two sentences, written as if the review is finished — because it is. Say what the change does and whether it is safe to merge. Never describe what you are about to do or what you would need to check; there is no second turn. Do not repeat the findings, and do not claim a problem here that is absent from the findings list: a bug mentioned only in prose is anchored to nothing and blocks nothing."
            },
            "resolved": {
                "type": "array",
                "description": "Titles of findings from earlier cycles that this revision fixed.",
                "items": { "type": "string" }
            },
            "findings": {
                "type": "array",
                "description": "Problems introduced by this pull request. An empty array is a valid and common answer; do not pad it.",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["path", "line", "rule", "title", "body", "severity", "confidence"],
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "File path exactly as it appears in the diff."
                        },
                        "line": {
                            "type": "integer",
                            "minimum": 1,
                            "description": "A line the diff changed. A finding you cannot anchor to a changed line is not a finding."
                        },
                        "end_line": {
                            "type": ["integer", "null"],
                            "minimum": 1,
                            "description": "Last line, if the finding spans a range."
                        },
                        "rule": {
                            "type": "string",
                            "description": "Stable kebab-case category, e.g. `unchecked-index`. The same class of problem must always use the same value, because suppression is keyed on it."
                        },
                        "title": {
                            "type": "string",
                            "maxLength": 80,
                            "description": "Imperative, at most 80 characters: `Guard the index before dereferencing`."
                        },
                        "body": {
                            "type": "string",
                            "description": "What is wrong, why it matters, and what to do. Markdown."
                        },
                        "severity": {
                            "type": "string",
                            "enum": ["low", "medium", "high", "critical"]
                        },
                        "confidence": {
                            "type": "number",
                            "minimum": 0,
                            "maximum": 1,
                            "description": "How sure you are. Use a low value when the finding depends on something you could not see, not as a general hedge."
                        },
                        "suggestion": {
                            "type": ["string", "null"],
                            "description": "Replacement text for the anchored lines, if you have one."
                        },
                        "late": {
                            "type": "boolean",
                            "description": "True only if this is about code the pull request did not change, and only after checking an earlier revision."
                        }
                    }
                }
            }
        }
    })
}

fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let kept: String = text.chars().take(max.saturating_sub(1)).collect();
    format!("{}…", kept.trim_end())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_well_formed_response_parses() {
        let value = json!({
            "summary": "Adds a bounds check.",
            "findings": [{
                "path": "src/lib.rs",
                "line": 42,
                "rule": "unchecked-index",
                "title": "Guard the index before dereferencing",
                "body": "…",
                "severity": "high",
                "confidence": 0.9
            }]
        });

        let parsed = parse(LaneId::Critique, value).expect("parses");
        assert_eq!(parsed.findings.len(), 1);
        assert_eq!(parsed.findings[0].severity, Severity::High);
    }

    #[test]
    fn an_empty_finding_list_is_valid() {
        let parsed = parse(
            LaneId::Critique,
            json!({"summary": "Nothing to report.", "findings": []}),
        )
        .expect("parses");
        assert!(parsed.findings.is_empty());
    }

    #[test]
    fn malformed_output_fails_loudly_rather_than_being_guessed_at() {
        let err = parse(
            LaneId::Critique,
            json!({"summary": "…", "findings": [{"path": "src/lib.rs"}]}),
        )
        .unwrap_err();

        assert!(
            err.to_string().contains("did not match the schema"),
            "{err}"
        );
        assert!(err.to_string().contains("critique"), "{err}");
    }

    #[test]
    fn a_credential_the_model_quoted_never_reaches_the_finding() {
        let key = format!("{}{}", "AKIA", "IOSFODNN7EXAMPLE");
        let raw = RawFinding {
            path: "src/lib.rs".into(),
            line: 1,
            end_line: None,
            rule: "hardcoded-credential".into(),
            title: format!("Remove the hardcoded key {key}"),
            body: format!("`{key}` is committed on line 1."),
            severity: Severity::Critical,
            confidence: 1.0,
            suggestion: Some(format!("let key = std::env::var(\"KEY\")?; // was {key}")),
            late: false,
        };

        let finding = raw.into_finding(LaneId::Critique);
        let rendered = serde_json::to_string(&finding).expect("serialises");
        assert!(!rendered.contains("IOSFODNN7EXAMPLE"), "{rendered}");
        assert!(rendered.contains("AKIA"), "the vendor prefix survives");
    }

    #[test]
    fn confidence_outside_the_range_is_clamped() {
        let raw = RawFinding {
            path: "src/lib.rs".into(),
            line: 1,
            end_line: None,
            rule: "r".into(),
            title: "t".into(),
            body: "b".into(),
            severity: Severity::Low,
            confidence: 5.0,
            suggestion: None,
            late: false,
        };
        assert_eq!(raw.into_finding(LaneId::Critique).confidence, 1.0);
    }

    #[test]
    fn an_over_long_title_is_truncated_rather_than_rejected() {
        // The schema asks for 80; models occasionally ignore maxLength, and
        // losing an otherwise good finding over a long title would be silly.
        let long = "x".repeat(200);
        let raw = RawFinding {
            path: "src/lib.rs".into(),
            line: 1,
            end_line: None,
            rule: "r".into(),
            title: long,
            body: "b".into(),
            severity: Severity::Low,
            confidence: 0.5,
            suggestion: None,
            late: false,
        };
        let finding = raw.into_finding(LaneId::Critique);
        assert_eq!(finding.title.chars().count(), 80);
        assert!(finding.title.ends_with('…'));
    }

    #[test]
    fn the_schema_forbids_unknown_properties() {
        let schema = json_schema();
        assert_eq!(schema["additionalProperties"], json!(false));
        assert_eq!(
            schema["properties"]["findings"]["items"]["additionalProperties"],
            json!(false)
        );
    }

    #[test]
    fn the_schema_tells_the_model_an_empty_list_is_acceptable() {
        let schema = json_schema();
        let description = schema["properties"]["findings"]["description"]
            .as_str()
            .expect("described");
        assert!(
            description.contains("empty array is a valid"),
            "{description}"
        );
    }

    #[test]
    fn severity_round_trips_through_the_schema_enum() {
        let allowed =
            json_schema()["properties"]["findings"]["items"]["properties"]["severity"]["enum"]
                .clone();
        assert_eq!(allowed, json!(["low", "medium", "high", "critical"]));
        for severity in Severity::ALL {
            let value = serde_json::to_value(severity).expect("serialises");
            assert!(
                allowed.as_array().unwrap().contains(&value),
                "{severity} missing from the schema enum"
            );
        }
    }
}
