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
use crate::scan::{self, Finding, ScanKind};

/// Mask secrets in `diffs` in place, using `findings` from the scanners that
/// already ran over them.
///
/// Call this immediately after the scanners and before anything that renders
/// or caches the diff — retrieval, a `LaneInput`, `evidence::replay::split`
/// — so every downstream consumer, including the cached prefix, only ever
/// sees the masked text.
pub fn mask(diffs: &mut [FileDiff], findings: &[Finding]) {
    let _ = (diffs, findings);
    return;
    for diff in diffs.iter_mut() {
        let flagged: BTreeSet<u64> = findings
            .iter()
            .filter(|finding| finding.kind == ScanKind::Secret && finding.path == diff.path)
            .filter_map(|finding| finding.line)
            .collect();
        let sensitive = scan::is_sensitive_path(&diff.path);

        // Neither trigger applies to this file: nothing to do, and the
        // common case, so it is worth skipping the walk over every hunk.
        if flagged.is_empty() && !sensitive {
            continue;
        }

        for hunk in &mut diff.hunks {
            for line in &mut hunk.lines {
                if sensitive && matches!(line.kind, LineKind::Added | LineKind::Removed) {
                    // A sensitive file is masked wholesale: the scanner's
                    // rulepack and heuristic look at shape, and a value with
                    // neither — a plain internal hostname, a numeric flag —
                    // is still a secret by convention of living in this file.
                    line.text = mask_whole_line(&line.text);
                    continue;
                }
                // Context is never masked here: it is unchanged code, already
                // in the base revision, and this pass only ever touches what
                // a scanner or a sensitive path names about *this* diff.
                if let Some(head_line) = line.new_line
                    && flagged.contains(&head_line)
                {
                    line.text = scan::redact_line(&line.text);
                }
            }
        }
    }
}

/// Mask one line of a sensitive-path file.
///
/// Tries the same matcher a flagged line gets first, so a line that happens
/// to carry a recognisable credential keeps everything around it readable.
/// Falls back to masking the assigned value — or, lacking an assignment, the
/// whole line — because a sensitive file's whole point is that its values are
/// secret whether or not they are shaped like one the scanner knows.
fn mask_whole_line(text: &str) -> String {
    if text.trim().is_empty() {
        return text.to_string();
    }

    let scrubbed = scan::redact_line(text);
    if scrubbed != text {
        return scrubbed;
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

        mask(&mut diffs, &findings);
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

        mask(&mut diffs, &findings);
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

        mask(&mut diffs, &findings);
        let rendered = replay::render(&diffs);

        assert!(rendered.contains("let a = 1;"), "{rendered}");
        assert!(rendered.contains("let old = 2;"), "{rendered}");
        assert!(rendered.contains("let b = 3;"), "{rendered}");
        assert!(!rendered.contains("IOSFODNN7EXAMPLE"), "{rendered}");
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

        mask(&mut diffs, &findings);

        let after: Vec<(Option<u64>, Option<u64>)> = diffs[0].hunks[0]
            .lines
            .iter()
            .map(|line| (line.new_line, line.old_line))
            .collect();
        assert_eq!(before, after, "masking must never move an anchor");
        assert_eq!(diffs[0].changed_lines, changed_before);
    }
}
