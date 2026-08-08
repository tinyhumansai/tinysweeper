//! Types shared by the deterministic scanners.
//!
//! Scanners run before any model call. They are cheap, they are certain, and
//! their findings are the only ones that can fail a check without a model
//! having looked at anything — so their output shape is deliberately narrow.

use serde::{Deserialize, Serialize};

use crate::config::types::Severity;

/// What kind of problem a scanner found.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScanKind {
    /// Something shaped like a credential was committed.
    Secret,
    /// A large or binary blob entered the history.
    Blob,
    /// Build output, dependencies or other junk was committed.
    Junk,
    /// A GitHub Actions workflow was made more permissive or less pinned.
    Workflow,
    /// A dependency or lockfile changed.
    Dependency,
}

impl ScanKind {
    /// A short label for check-run summaries.
    pub fn as_str(self) -> &'static str {
        match self {
            ScanKind::Secret => "secret",
            ScanKind::Blob => "blob",
            ScanKind::Junk => "junk",
            ScanKind::Workflow => "workflow",
            ScanKind::Dependency => "dependency",
        }
    }
}

/// One thing a scanner found.
///
/// Note what is *not* here: the matched text. A secret's value must never reach
/// a comment, a check-run summary, or a log, so the type simply has nowhere to
/// put it. See [`Finding::redacted_hint`] for what is safe to show instead.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Finding {
    /// What kind of problem this is.
    pub kind: ScanKind,
    /// How much it matters.
    pub severity: Severity,
    /// The file it was found in.
    pub path: String,
    /// The head-revision line, when the finding has one.
    pub line: Option<u64>,
    /// A specific rule id, e.g. `aws-access-key-id`. Stable enough to suppress
    /// by.
    pub rule: String,
    /// One imperative sentence naming the problem.
    pub title: String,
    /// What to do about it.
    pub detail: String,
    /// A safe fragment of the match — never the value itself.
    pub redacted_hint: Option<String>,
}

impl Finding {
    /// Build a finding with no location beyond the file.
    pub fn new(
        kind: ScanKind,
        severity: Severity,
        path: impl Into<String>,
        rule: impl Into<String>,
        title: impl Into<String>,
        detail: impl Into<String>,
    ) -> Self {
        Self {
            kind,
            severity,
            path: path.into(),
            line: None,
            rule: rule.into(),
            title: title.into(),
            detail: detail.into(),
            redacted_hint: None,
        }
    }

    /// Anchor this finding to a line.
    pub fn at_line(mut self, line: u64) -> Self {
        self.line = Some(line);
        self
    }

    /// Attach a redacted hint.
    pub fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.redacted_hint = Some(hint.into());
        self
    }
}

/// Show enough of a matched credential to be recognisable, and no more.
///
/// Keeps the first four characters — usually the vendor prefix, which is the
/// only genuinely useful part — and reports the length. Anything shorter than
/// eight characters is elided entirely, because at that size a prefix plus a
/// length is close to the whole value.
pub fn redact(value: &str) -> String {
    let chars: Vec<char> = value.chars().collect();
    if chars.len() < 8 {
        return format!("<redacted, {} chars>", chars.len());
    }
    let prefix: String = chars.iter().take(4).collect();
    format!("{prefix}… <redacted, {} chars>", chars.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redaction_keeps_the_vendor_prefix_and_nothing_else() {
        // Split so this file does not itself contain a credential-shaped
        // literal; see the note on `token` in `scan::secrets`.
        let hint = redact(&format!(
            "{}{}",
            "sk-or-", "v1-0f1e2d3c4b5a69788796a5b4c3d2e1f0"
        ));

        assert!(hint.starts_with("sk-o"));
        assert!(hint.contains("redacted"));
        assert!(
            !hint.contains("0f1e2d3c"),
            "the body of the secret must never appear: {hint}"
        );
    }

    #[test]
    fn short_values_are_elided_entirely() {
        let hint = redact("abc123");
        assert_eq!(hint, "<redacted, 6 chars>");
        assert!(!hint.contains("abc"));
    }

    #[test]
    fn redaction_reports_the_length_so_a_human_can_recognise_the_shape() {
        assert!(redact(&"x".repeat(40)).contains("40 chars"));
    }

    #[test]
    fn a_finding_has_nowhere_to_put_a_raw_value() {
        // Compile-time property, asserted here so a future field addition has
        // to consciously break this test.
        let finding = Finding::new(
            ScanKind::Secret,
            Severity::Critical,
            "src/lib.rs",
            "aws-access-key-id",
            "Remove the committed AWS access key",
            "Rotate it, then purge it from history.",
        )
        .at_line(12)
        .with_hint(redact(&format!("{}{}", "AKIA", "IOSFODNN7EXAMPLE")));

        let json = serde_json::to_string(&finding).expect("serialises");
        assert!(!json.contains("IOSFODNN7EXAMPLE"), "{json}");
        assert!(json.contains("AKIA"), "the recognisable prefix survives");
    }
}
