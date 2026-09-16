//! Masking secrets out of a parsed diff before it ever reaches a model.
//!
//! `scan::secrets::scrub` used to be the only line of defence, and it ran on
//! a model's *output* — a comment, a check-run summary. The raw diff itself
//! was rendered straight into the prompt: a committed `.env` file, or a line
//! [`crate::scan::secrets`] flagged, went to the provider unredacted, and
//! scrubbing what came back was scrubbing after the fact. This module runs
//! *before* the request is built, over the same [`FileDiff`] every lane
//! renders, so there is nothing left for a model to quote.
//!
//! Two independent triggers, because neither alone covers the case the other
//! catches:
//!
//! - A [`crate::scan::ScanKind::Secret`] finding names a `(path, line)`. That
//!   line's matched span is masked with [`crate::scan::redact_line`] — the
//!   same matcher and the same token as scrubbing a model's output, so a
//!   human sees one redaction vocabulary everywhere it appears.
//! - [`crate::scan::is_sensitive_path`] names a whole file — `.env`, a
//!   private key — whose *shape*, not its content, is the giveaway. The
//!   scanner's rulepack and entropy heuristic can both miss a value that
//!   does not look like the credentials they know; a path on this list is
//!   masked line by line regardless of what either one flagged.

use std::collections::BTreeSet;

use crate::evidence::diff::{FileDiff, LineKind};
use crate::forge::types::ChangedFile;
use crate::scan::{self, Finding, ScanKind};

/// What one call to [`mask`] actually redacted.
///
/// Returned rather than logged so the caller can tell a reviewer about it: a
/// model handed a diff with a chunk quietly missing from it has no way to
/// distinguish "nothing was here" from "something was removed", and the two
/// call for different behaviour — the first is silence, the second is "do not
/// ask for or guess the value".
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Redactions {
    /// How many locations were masked, across every file.
    pub spans: usize,
    /// Which paths had at least one masked location, in the order first seen.
    pub files: Vec<String>,
}

impl Redactions {
    /// Whether anything was masked at all.
    pub fn is_empty(&self) -> bool {
        self.spans == 0
    }

    /// One sentence for the volatile suffix, telling a reviewer what the
    /// `<redacted, N chars>` markers it is about to read mean.
    ///
    /// Empty when nothing was masked, so a caller can push it into the prompt
    /// unconditionally without an `if` of its own — an empty string renders
    /// as nothing.
    pub fn note(&self) -> String {
        if self.is_empty() {
            return String::new();
        }
        let value = if self.spans == 1 { "value" } else { "values" };
        format!(
            "{} credential {value} were removed from this diff before you saw it and appear \
             as `<redacted, N chars>`; the lines are real, only the values are gone — never \
             ask for or guess them.",
            self.spans
        )
    }
}

/// Mask secrets in `diffs` in place, using `findings` from the scanners that
/// already ran over them.
///
/// `files` is the same [`ChangedFile`] list the diffs were parsed from — it is
/// consulted for `previous_path`, which [`FileDiff`] does not carry: a rename
/// out of a sensitive path (`.env` to `config.txt`) is still a sensitive edit
/// on the base-revision side of the diff even though the head path alone would
/// say otherwise.
///
/// Call this immediately after the scanners and before anything that renders
/// or caches the diff — retrieval, a `LaneInput`, `evidence::replay::split`
/// — so every downstream consumer, including the cached prefix, only ever
/// sees the masked text. The [`Redactions`] it returns is what tells that
/// prompt a value is missing on purpose.
pub fn mask(diffs: &mut [FileDiff], findings: &[Finding], files: &[ChangedFile]) -> Redactions {
    let mut spans = 0usize;
    let mut files_masked = Vec::new();

    for diff in diffs.iter_mut() {
        let flagged: BTreeSet<u64> = findings
            .iter()
            .filter(|finding| finding.kind == ScanKind::Secret && finding.path == diff.path)
            .filter_map(|finding| finding.line)
            .collect();
        let previous_sensitive = files
            .iter()
            .find(|file| file.path == diff.path)
            .and_then(|file| file.previous_path.as_deref())
            .is_some_and(scan::is_sensitive_path);
        let sensitive = scan::is_sensitive_path(&diff.path) || previous_sensitive;

        let mut masked_here = false;
        // A private key's armour line names the key type, not the key: the
        // base64 body between it and the matching end marker is the actual
        // secret, and it carries no vendor prefix or assignment shape for
        // either scanner to anchor a per-line finding on. This runs over
        // every line of every diff — not just a flagged or sensitive-path
        // file — because the marker text is specific enough to carry no
        // false-positive risk, and a key pasted into an ordinary source file
        // is exactly the case a per-file allowlist cannot cover.
        let mut in_key_block = false;
        for hunk in &mut diff.hunks {
            in_key_block = false;
            for line in &mut hunk.lines {
                if scan::is_private_key_begin(&line.text) {
                    in_key_block = true;
                    continue;
                }
                if in_key_block {
                    if scan::is_private_key_end(&line.text) {
                        in_key_block = false;
                        continue;
                    }
                    // Blank lines inside the armour are formatting, not key
                    // material; masking them would just be noise.
                    if !line.text.trim().is_empty() {
                        spans += 1;
                        masked_here = true;
                        line.text = scan::redact(line.text.trim());
                    }
                    continue;
                }

                // The deterministic rulepack runs over every line kind: a
                // credential shaped like a known vendor's is just as live in
                // a removed or context line — code the base revision already
                // carried — as in an added one. It is applied unconditionally
                // because it only ever matches a known shape; there is no
                // heuristic here to false-positive on ordinary code.
                let rulepack_masked = scan::redact_line(&line.text);
                if rulepack_masked != line.text {
                    spans += 1;
                    masked_here = true;
                    line.text = rulepack_masked;
                    continue;
                }

                if sensitive && matches!(line.kind, LineKind::Added | LineKind::Removed) {
                    // A sensitive file is masked wholesale: the scanner's
                    // rulepack and heuristic look at shape, and a value with
                    // neither — a plain internal hostname, a numeric flag —
                    // is still a secret by convention of living in this file.
                    let masked = mask_assignment_or_whole_line(&line.text);
                    if masked != line.text {
                        spans += 1;
                        masked_here = true;
                        line.text = masked;
                    }
                    continue;
                }

                // A finding the scanner anchored to this exact head line but
                // whose value the rulepack itself cannot see — an
                // entropy-flagged assignment, or a private-key marker with no
                // `=`/`:` to split on — still names a value that must not
                // reach a model. Fall back to the same assignment-or-whole-line
                // masking a sensitive path gets, but only for a line the
                // scanner specifically flagged, so an ordinary assignment
                // elsewhere in the same file is left readable.
                if let Some(head_line) = line.new_line
                    && flagged.contains(&head_line)
                {
                    let masked = mask_assignment_or_whole_line(&line.text);
                    if masked != line.text {
                        spans += 1;
                        masked_here = true;
                        line.text = masked;
                    }
                }
            }
        }
        if masked_here {
            files_masked.push(diff.path.clone());
        }
    }

    Redactions {
        spans,
        files: files_masked,
    }
}

/// Mask one line whose value must be hidden regardless of its shape: either a
/// sensitive-path line, or a line the scanner specifically flagged whose
/// value the rulepack matcher itself cannot see (an entropy-flagged
/// assignment, or a private-key marker with nothing to split on).
///
/// Callers try [`scan::redact_line`] first, so this only ever runs once that
/// has already failed to find a recognisable shape. Masks the assigned value
/// when the line looks like an assignment, or the whole line when it does
/// not — a sensitive file's or a flagged line's whole point is that its value
/// is secret whether or not it is shaped like one the rulepack knows.
fn mask_assignment_or_whole_line(text: &str) -> String {
    if text.trim().is_empty() {
        return text.to_string();
    }

    match text.find(['=', ':']) {
        Some(index) => {
            let (name, value) = text.split_at(index + 1);
            if value.trim().is_empty() {
                text.to_string()
            } else {
                format!("{name}{}", scan::redact(value.trim()))
            }
        }
        None => scan::redact(text),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::evidence::diff::parse_file_patch;
    use crate::evidence::replay;

    /// Split so this file does not itself contain a credential-shaped
    /// literal — see the identical note in `scan::secrets::tests::token`.
    fn token(prefix: &str, body: &str) -> String {
        format!("{prefix}{body}")
    }

    fn patch(lines: &[&str]) -> String {
        let body = lines.join("\n");
        format!("@@ -0,0 +1,{} @@\n{body}\n", lines.len())
    }

    #[test]
    fn a_secret_flagged_line_is_masked_in_rendered_output() {
        let key = token("AKIA", "IOSFODNN7EXAMPLE");
        let mut diffs = vec![parse_file_patch(
            "src/config.rs",
            &patch(&[&format!("+const KEY: &str = \"{key}\";")]),
        )];
        let findings = scan::secrets::scan_added_lines("src/config.rs", diffs[0].added_lines());

        mask(&mut diffs, &findings, &[]);
        let rendered = replay::render(&diffs);

        assert!(!rendered.contains("IOSFODNN7EXAMPLE"), "{rendered}");
        assert!(
            rendered.contains("const KEY: &str ="),
            "surrounding code stays readable: {rendered}"
        );
    }

    #[test]
    fn an_env_file_is_masked_on_every_line_not_just_the_matched_one() {
        let key = token("AKIA", "IOSFODNN7EXAMPLE");
        let mut diffs = vec![parse_file_patch(
            ".env",
            &patch(&[&format!("+AWS_KEY={key}"), "+FEATURE_FLAG=on"]),
        )];
        let findings = scan::secrets::scan_added_lines(".env", diffs[0].added_lines());
        // Only the AWS line is scanner-flagged; the plain flag is not
        // credential-shaped and would pass the rulepack and the heuristic.
        assert_eq!(findings.len(), 1, "{findings:#?}");

        mask(&mut diffs, &findings, &[]);
        let rendered = replay::render(&diffs);

        assert!(!rendered.contains("IOSFODNN7EXAMPLE"), "{rendered}");
        assert!(
            !rendered.contains("FEATURE_FLAG=on"),
            "an .env value is masked on shape of the file, not just a scanner hit: {rendered}"
        );
        assert!(rendered.contains("FEATURE_FLAG="), "{rendered}");
    }

    #[test]
    fn context_and_removed_lines_around_a_secret_are_untouched() {
        let key = token("AKIA", "IOSFODNN7EXAMPLE");
        let raw = format!(
            "@@ -1,3 +1,3 @@\n let a = 1;\n-let old = 2;\n+const KEY: &str = \"{key}\";\n let b = 3;\n"
        );
        let mut diffs = vec![parse_file_patch("src/config.rs", &raw)];
        let findings = scan::secrets::scan_added_lines("src/config.rs", diffs[0].added_lines());

        mask(&mut diffs, &findings, &[]);
        let rendered = replay::render(&diffs);

        assert!(rendered.contains("let a = 1;"), "{rendered}");
        assert!(rendered.contains("let old = 2;"), "{rendered}");
        assert!(rendered.contains("let b = 3;"), "{rendered}");
        assert!(!rendered.contains("IOSFODNN7EXAMPLE"), "{rendered}");
    }

    #[test]
    fn mask_reports_how_many_spans_it_redacted_and_which_files() {
        let key = token("AKIA", "IOSFODNN7EXAMPLE");
        let mut diffs = vec![
            parse_file_patch(
                "src/config.rs",
                &patch(&[&format!("+const KEY: &str = \"{key}\";")]),
            ),
            parse_file_patch(".env", &patch(&["+FEATURE_FLAG=on"])),
        ];
        let findings = scan::secrets::scan_added_lines("src/config.rs", diffs[0].added_lines());

        let redactions = mask(&mut diffs, &findings, &[]);

        // One span in `src/config.rs` (the scanner-flagged key) and one in
        // `.env` (masked wholesale, on shape of the path alone).
        assert_eq!(redactions.spans, 2, "{redactions:?}");
        assert_eq!(redactions.files, vec!["src/config.rs", ".env"]);
        assert!(!redactions.is_empty());
        assert!(redactions.note().contains('2'), "{}", redactions.note());
        assert!(
            redactions.note().contains("never ask for or guess"),
            "{}",
            redactions.note()
        );
    }

    #[test]
    fn nothing_redacted_reports_an_empty_summary_and_an_empty_note() {
        let mut diffs = vec![parse_file_patch(
            "src/config.rs",
            &patch(&["+let ordinary = 1;"]),
        )];
        let findings = scan::secrets::scan_added_lines("src/config.rs", diffs[0].added_lines());

        let redactions = mask(&mut diffs, &findings, &[]);

        assert!(redactions.is_empty(), "{redactions:?}");
        assert_eq!(redactions.note(), "");
    }

    #[test]
    fn line_numbers_survive_masking() {
        let key = token("AKIA", "IOSFODNN7EXAMPLE");
        let mut diffs = vec![parse_file_patch(
            ".env",
            &patch(&[&format!("+AWS_KEY={key}"), "+FEATURE_FLAG=on"]),
        )];
        let before: Vec<(Option<u64>, Option<u64>)> = diffs[0].hunks[0]
            .lines
            .iter()
            .map(|line| (line.new_line, line.old_line))
            .collect();
        let changed_before = diffs[0].changed_lines.clone();
        let findings = scan::secrets::scan_added_lines(".env", diffs[0].added_lines());

        mask(&mut diffs, &findings, &[]);

        let after: Vec<(Option<u64>, Option<u64>)> = diffs[0].hunks[0]
            .lines
            .iter()
            .map(|line| (line.new_line, line.old_line))
            .collect();
        assert_eq!(before, after, "masking must never move an anchor");
        assert_eq!(diffs[0].changed_lines, changed_before);
    }
}
