//! The canonical finding type, and its fingerprint.
//!
//! One type for everything a lane can report, whether a deterministic scanner
//! found it or a model did. Two parallel types would drift, and the filtering,
//! dedupe and capping rules have to apply identically to both — a scanner
//! finding is not exempt from the comment cap.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::config::types::{LaneId, Severity};
use crate::scan::types::Finding as ScanFinding;

/// Something worth telling the author about.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Finding {
    /// Which lane produced it.
    pub lane: LaneId,
    /// How much it matters.
    pub severity: Severity,
    /// How sure the producer is, 0.0 to 1.0.
    ///
    /// Deterministic scanners report 1.0 for rulepack matches and less for
    /// heuristics. A model reports its own estimate, and the config's
    /// `confidence_min` drops anything below the bar before it is posted.
    pub confidence: f64,
    /// The file it is about.
    pub path: String,
    /// The head-revision line it anchors to.
    pub line: Option<u64>,
    /// The last line, when it spans a range.
    pub end_line: Option<u64>,
    /// A stable rule or category id, e.g. `aws-access-key-id` or
    /// `unchecked-index`. Suppression is keyed partly on this, so it must not
    /// change between runs for the same class of problem.
    pub rule: String,
    /// One imperative sentence, at most 80 characters.
    pub title: String,
    /// The explanation. Markdown.
    pub body: String,
    /// A concrete replacement for the anchored lines, if there is one.
    pub suggestion: Option<String>,
    /// Whether this was raised against code the pull request did not change.
    ///
    /// Only ever set after actually diffing against an earlier reviewed SHA.
    /// Drip-feeding one new concern per cycle is a review defect, so a late
    /// finding has to announce itself as one.
    pub late: bool,
    /// The fingerprint that identifies this finding across pushes.
    ///
    /// Computed once, during review, over the code the finding anchors to —
    /// see [`Finding::fingerprint`] and
    /// [`anchor_context`](crate::findings::anchor::anchor_context). It travels
    /// in the proposal because `apply` has no diff to recompute it from, and it
    /// is what the `tinysweeper:fp=` marker carries onto GitHub.
    ///
    /// `None` on a finding that never went through review — the apply path
    /// falls back to a title-derived fingerprint so an old proposal still
    /// publishes.
    #[serde(default)]
    pub identity: Option<String>,
}

impl Finding {
    /// The identity used for dedupe and suppression.
    ///
    /// Deliberately excludes the line number and the body. A finding that moves
    /// down three lines because someone added an import is the *same* finding,
    /// and re-posting it would be exactly the noise this exists to prevent.
    /// Excluding the body means a reworded explanation does not resurrect a
    /// suppressed finding either.
    ///
    /// `context` is a short snippet of the code the finding is about, so the
    /// same rule firing on two different call sites stays two findings.
    pub fn fingerprint(&self, context: &str) -> String {
        let mut hasher = Sha256::new();
        hasher.update(self.lane.as_str().as_bytes());
        hasher.update(b"\0");
        hasher.update(self.path.as_bytes());
        hasher.update(b"\0");
        hasher.update(self.rule.as_bytes());
        hasher.update(b"\0");
        hasher.update(normalize(context).as_bytes());

        // Hand-rolled hex: sha2 0.11 returns a `Array` that does not implement
        // `LowerHex`, and pulling in a hex crate for sixteen characters is not
        // worth a dependency in the offline default build.
        hasher
            .finalize()
            .iter()
            .take(8)
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }

    /// Whether this finding is at or above `gate`.
    pub fn meets(&self, gate: Severity, confidence_min: f64) -> bool {
        self.severity >= gate && self.confidence >= confidence_min
    }

    /// The line range this anchors to, if any.
    pub fn range(&self) -> Option<(u64, u64)> {
        let start = self.line?;
        Some((start, self.end_line.unwrap_or(start)))
    }
}

/// Collapse whitespace so reformatting does not change a fingerprint.
///
/// A `cargo fmt` run that rewraps a line must not resurrect every suppressed
/// finding in the file.
fn normalize(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

impl From<ScanFinding> for Finding {
    fn from(scan: ScanFinding) -> Self {
        // Scanners are certain about rulepack matches and say so through
        // severity; the heuristic ones already report at a lower severity, and
        // this maps that to a confidence the shared gate understands.
        let confidence = match scan.severity {
            Severity::Critical => 1.0,
            Severity::High => 0.8,
            _ => 0.6,
        };

        let body = match &scan.redacted_hint {
            Some(hint) => format!("{}\n\nMatched: `{hint}`", scan.detail),
            None => scan.detail.clone(),
        };

        Self {
            // Scanner output is evidence for the lane that requested it; the
            // caller re-labels it. `commits` is the common case and the safe
            // default, because that lane fails on any secret.
            lane: LaneId::Commits,
            severity: scan.severity,
            confidence,
            path: scan.path,
            line: scan.line,
            end_line: None,
            rule: scan.rule,
            title: scan.title,
            body,
            suggestion: None,
            late: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::types::ScanKind;

    fn finding() -> Finding {
        Finding {
            lane: LaneId::Critique,
            severity: Severity::Medium,
            confidence: 0.9,
            path: "src/lib.rs".into(),
            line: Some(42),
            end_line: None,
            rule: "unchecked-index".into(),
            title: "Guard the index before dereferencing".into(),
            body: "…".into(),
            suggestion: None,
            late: false,
        }
    }

    #[test]
    fn a_finding_that_moved_lines_keeps_its_fingerprint() {
        // Someone adds an import at the top of the file. Nothing about the
        // finding changed, so re-posting it would be pure noise.
        let before = finding().fingerprint("let x = items[i];");
        let mut after = finding();
        after.line = Some(45);
        assert_eq!(before, after.fingerprint("let x = items[i];"));
    }

    #[test]
    fn rewording_the_explanation_does_not_resurrect_a_suppressed_finding() {
        let before = finding().fingerprint("let x = items[i];");
        let mut reworded = finding();
        reworded.body = "a completely different explanation".into();
        reworded.title = "Check the bound first".into();
        assert_eq!(before, reworded.fingerprint("let x = items[i];"));
    }

    #[test]
    fn reformatting_the_code_does_not_change_the_fingerprint() {
        let a = finding().fingerprint("let x = items[i];");
        let b = finding().fingerprint("let x =\n    items[i];");
        assert_eq!(a, b, "whitespace must not matter");
    }

    #[test]
    fn the_same_rule_at_two_call_sites_stays_two_findings() {
        let a = finding().fingerprint("let x = items[i];");
        let b = finding().fingerprint("let y = others[j];");
        assert_ne!(a, b);
    }

    #[test]
    fn the_same_code_in_two_files_stays_two_findings() {
        let a = finding().fingerprint("let x = items[i];");
        let mut other = finding();
        other.path = "src/other.rs".into();
        assert_ne!(a, other.fingerprint("let x = items[i];"));
    }

    #[test]
    fn different_lanes_do_not_collide() {
        let a = finding().fingerprint("code");
        let mut security = finding();
        security.lane = LaneId::Security;
        assert_ne!(a, security.fingerprint("code"));
    }

    #[test]
    fn the_gate_applies_to_severity_and_confidence_together() {
        let f = finding();
        assert!(f.meets(Severity::Medium, 0.6));
        assert!(!f.meets(Severity::High, 0.6), "below the severity gate");
        assert!(
            !f.meets(Severity::Medium, 0.95),
            "below the confidence gate"
        );
    }

    #[test]
    fn a_scanner_finding_becomes_a_finding_without_leaking_its_value() {
        let scan = ScanFinding::new(
            ScanKind::Secret,
            Severity::Critical,
            "src/main.rs",
            "aws-access-key-id",
            "Remove the committed credential",
            "Rotate it first.",
        )
        .at_line(7)
        .with_hint("AKIA… <redacted, 20 chars>");

        let finding: Finding = scan.into();
        assert_eq!(finding.confidence, 1.0);
        assert_eq!(finding.line, Some(7));
        assert!(finding.body.contains("Rotate it first."));
        assert!(finding.body.contains("AKIA…"));
        assert!(!finding.body.contains("IOSFODNN"));
    }

    #[test]
    fn a_range_defaults_to_a_single_line() {
        assert_eq!(finding().range(), Some((42, 42)));

        let mut spanning = finding();
        spanning.end_line = Some(50);
        assert_eq!(spanning.range(), Some((42, 50)));
    }
}
