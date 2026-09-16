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
//! Several independent triggers, because none alone covers the case another
//! catches:
//!
//! - A [`crate::scan::ScanKind::Secret`] finding names a `(path, line)`. When
//!   the matched span has a recognisable rulepack shape it is masked with
//!   [`crate::scan::redact_line`] — the same matcher and the same token as
//!   scrubbing a model's output, so a human sees one redaction vocabulary
//!   everywhere it appears. A finding whose value the rulepack cannot see —
//!   an entropy-flagged assignment, or a private-key marker with nothing to
//!   split on — still gets its assigned value, or failing that the whole
//!   line, masked: the scanner already decided this line names a secret, so
//!   the fallback trusts that decision instead of requiring a second, this
//!   time content-based, confirmation.
//! - [`crate::scan::is_sensitive_path`] names a whole file — `.env`, a
//!   private key — whose *shape*, not its content, is the giveaway. The
//!   scanner's rulepack and entropy heuristic can both miss a value that
//!   does not look like the credentials they know; a path on this list is
//!   masked line by line regardless of what either one flagged. The head
//!   path is not the only path that counts: a rename out of a sensitive path
//!   still exposed that content on the base-revision side, so the diff's
//!   previous path is checked too.
//! - A private-key PEM marker is looked for on *every* line of *every* diff,
//!   flagged or not, sensitive path or not: the armour line names a key type,
//!   not the key, so [`crate::scan::secrets`] only ever anchors a finding to
//!   it — but the base64 body between it and the matching end marker carries
//!   no vendor prefix or assignment shape for either scanner to anchor a
//!   per-line finding on, and is masked wholesale regardless.
//! - The rulepack matcher itself is run over every line kind of every diff,
//!   including removed and context lines: a credential shaped like a known
//!   vendor's is exactly as live sitting in the base revision as it is newly
//!   added, and the diff renders that line either way. This is safe to do
//!   unconditionally because the rulepack only ever matches a known shape —
//!   there is no heuristic here to false-positive on ordinary code. The
//!   entropy heuristic itself still only ever runs on added lines
//!   ([`crate::scan::secrets`]'s documented trade-off against noise), so a
//!   high-entropy value with no recognisable prefix sitting only in a removed
//!   or context line of an otherwise ordinary file is not caught by this
//!   pass — narrowing that gap would mean scanning the base revision on every
//!   push, which is the noise the scanner deliberately declines to make.

use std::collections::BTreeSet;

use crate::evidence::diff::FileDiff;
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
        for hunk in &mut diff.hunks {
            // Diff hunks omit arbitrary stretches of the file, so armour
            // state cannot safely cross their boundary. A closing marker may
            // be in omitted context; carrying this state would hide an
            // unrelated later hunk from every reviewing lane.
            // ...unless the hunk itself proves it opened mid-key: a closing
            // marker with no opening one before it.
            let mut in_key_block =
                scan::opens_inside_private_key(hunk.lines.iter().map(|line| line.text.as_str()));
            for line in &mut hunk.lines {
                if scan::is_private_key_begin(&line.text) {
                    let masked = scan::redact_stream_line(&line.text, &mut in_key_block);
                    if masked != line.text {
                        spans += 1;
                        masked_here = true;
                        line.text = masked;
                    }
                    continue;
                }
                if in_key_block {
                    if scan::is_private_key_end(&line.text) {
                        let masked = scan::redact_stream_line(&line.text, &mut in_key_block);
                        if masked != line.text {
                            spans += 1;
                            masked_here = true;
                            line.text = masked;
                        }
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
                let mut line_masked = false;
                if rulepack_masked != line.text {
                    spans += 1;
                    masked_here = true;
                    line.text = rulepack_masked;
                    line_masked = true;
                }

                if sensitive {
                    // A sensitive file is masked wholesale, on every line
                    // kind including context: the scanner's rulepack and
                    // heuristic look at shape, and a value with neither — a
                    // plain internal hostname, a numeric flag — is still a
                    // secret by convention of living in this file. Context is
                    // unchanged code, but it is rendered into the model
                    // request exactly like an added line, so leaving it out
                    // would mean a `.env` diff that touches one line still
                    // hands over every other line in the hunk around it.
                    let masked = mask_assignment_or_whole_line(&line.text);
                    if masked != line.text {
                        if !line_masked {
                            spans += 1;
                        }
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
                if !line_masked
                    && let Some(head_line) = line.new_line
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

/// Re-apply the path-independent halves of [`mask`] to text [`render`] already
/// produced, for evidence a *previous* review cycle persisted and this cycle
/// replays byte for byte.
///
/// That text predates whichever push first ran this module — a `.env` diff or
/// a scanner-flagged secret sent before this landed was recorded unmasked,
/// and `crate::state`'s cache replays it verbatim on the first re-review after
/// deploy. [`scan::is_sensitive_path`] cannot be recovered from rendered text
/// alone — the path is one line of the render (`--- {path}`), not a property
/// attached to each line below it — so only the rulepack match and
/// private-key-body masking [`mask`] also applies unconditionally run here.
/// That is exactly the risk worth closing: a value a scanner itself would
/// have flagged, not a value only a sensitive path's shape would have caught.
///
/// [`render`]: crate::evidence::diff::render
pub fn scrub_rendered(text: &str) -> String {
    let mut in_key_block = false;
    let mut sensitive = false;
    let mut out = String::with_capacity(text.len());

    for (index, line) in text.split('\n').enumerate() {
        if index > 0 {
            out.push('\n');
        }
        if let Some(path) = line.strip_prefix("--- ") {
            sensitive = scan::is_sensitive_path(path);
            in_key_block = false;
            if sensitive {
                out.push_str("--- <redacted sensitive path>");
                continue;
            }
        }
        if line.starts_with("@@ ") {
            in_key_block = false;
        }
        let (prefix, body) = split_render_prefix(line);
        out.push_str(prefix);
        let masked = scan::redact_stream_line(body, &mut in_key_block);
        let masked = if sensitive && !prefix.is_empty() {
            mask_assignment_or_whole_line(&masked)
        } else {
            masked
        };
        out.push_str(&masked);
    }

    out
}

/// Split a line [`render`](crate::evidence::diff::render) produced into its
/// fixed-width `{line-no} {marker}` prefix and the source text after it, so
/// [`scrub_rendered`] only ever masks text a scanner could have flagged and
/// never the line-number anchor a reviewer's comment depends on.
///
/// Both of `render`'s line shapes — `{n:>5} {marker}{text}` and
/// `      {marker}{text}` — are exactly 7 bytes of prefix before the source
/// text starts. A header line (`--- path`, `@@ ... @@`) is shorter than that
/// shape implies or does not carry one of `+`, `-`, ` ` at that offset, and is
/// returned whole as its own body: the rulepack still runs on it, harmlessly,
/// because neither header shape matches a credential.
fn split_render_prefix(line: &str) -> (&str, &str) {
    // The marker offset alone is not an anchor: ordinary prose such as
    // `token= opaque` also has a space at byte six. Rendered anchors have a
    // five-column line number (or five spaces for an unnumbered line).
    if line.len() >= 7
        && line.as_bytes()[..5]
            .iter()
            .all(|byte| byte.is_ascii_digit() || *byte == b' ')
        && line.as_bytes()[5] == b' '
        && matches!(line.as_bytes()[6], b'+' | b'-' | b' ')
    {
        line.split_at(7)
    } else {
        ("", line)
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

    /// Regression for a Codex/tinysweeper finding on #166: a sensitive path is
    /// masked "line by line" per this module's own doc, but the sensitive-path
    /// branch used to skip `LineKind::Context` — an unrelated edit to `.env`
    /// still renders the untouched lines around it into the model request.
    #[test]
    fn context_lines_in_a_sensitive_file_are_masked_too() {
        let raw = "@@ -1,3 +1,3 @@\n INTERNAL_TOKEN=opaque-value\n-OLD_FLAG=off\n+OLD_FLAG=on\n";
        let mut diffs = vec![parse_file_patch(".env", raw)];
        let findings = scan::secrets::scan_added_lines(".env", diffs[0].added_lines());
        // Not shaped like anything the rulepack or the entropy heuristic
        // knows: the sensitive-path fallback, not a scanner finding, is what
        // has to catch this line.
        assert!(findings.is_empty(), "{findings:#?}");

        mask(&mut diffs, &findings, &[]);
        let rendered = replay::render(&diffs);

        assert!(
            !rendered.contains("opaque-value"),
            "a context line's value is still a secret in a sensitive file: {rendered}"
        );
        assert!(rendered.contains("INTERNAL_TOKEN="), "{rendered}");
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

    /// Regression for a Codex finding on #166: an entropy-flagged assignment
    /// has no rulepack prefix for `scan::redact_line` to match, so it used to
    /// come through `mask` untouched even though the scanner had already
    /// flagged the exact line.
    #[test]
    fn a_high_entropy_assignment_with_no_rulepack_prefix_is_still_masked() {
        let value = token("f3Kq9zR2", "mW7pL4xN8vB1cY6tH0jD5sG");
        let mut diffs = vec![parse_file_patch(
            "src/config.rs",
            &patch(&[&format!("+let secret_token = \"{value}\";")]),
        )];
        let findings = scan::secrets::scan_added_lines("src/config.rs", diffs[0].added_lines());
        assert_eq!(findings.len(), 1, "{findings:#?}");
        assert_eq!(findings[0].rule, "high-entropy-assignment");

        mask(&mut diffs, &findings, &[]);
        let rendered = replay::render(&diffs);

        assert!(!rendered.contains(&value), "{rendered}");
        assert!(
            rendered.contains("let secret_token ="),
            "surrounding code stays readable: {rendered}"
        );
    }

    /// Regression for a Codex finding on #166: the marker line of a private
    /// key names the key type, not the key — the base64 body after it has no
    /// prefix or assignment shape for either scanner to anchor a per-line
    /// finding on, so it used to reach a model unmasked even though the file
    /// was flagged as carrying a private key.
    #[test]
    fn a_private_key_body_is_masked_even_without_a_per_line_finding() {
        // Split so this file's own diff does not carry a literal, contiguous
        // armour marker — see `token`'s note above and
        // `scan::secrets::tests::an_encrypted_private_key_armour_is_recognised_too`
        // for the identical concern applied to the marker text itself.
        let begin = format!("-----BEGIN {}-----", "RSA PRIVATE KEY");
        let end = format!("-----END {}-----", "RSA PRIVATE KEY");
        let body = "MIIEowIBAAKCAQEAthisisadeadbeefexamplebodyforatestcase1234567890";
        let raw = format!("@@ -0,0 +1,3 @@\n+{begin}\n+{body}\n+{end}\n");
        let mut diffs = vec![parse_file_patch("src/config.rs", &raw)];
        let findings = scan::secrets::scan_added_lines("src/config.rs", diffs[0].added_lines());
        // The scanner anchors one finding to the marker line; the body line
        // gets none, which is exactly the gap this pass has to close.
        assert_eq!(findings.len(), 1, "{findings:#?}");

        mask(&mut diffs, &findings, &[]);
        let rendered = replay::render(&diffs);

        assert!(!rendered.contains(body), "{rendered}");
        assert!(
            rendered.contains(&begin),
            "the armour line itself names no secret: {rendered}"
        );
    }

    /// A PEM can be serialized into an assignment with literal `\\n` escape
    /// sequences, leaving key material on the same physical line as its
    /// opening marker. The marker branch must mask that value before it
    /// begins tracking subsequent armour lines.
    #[test]
    fn a_private_key_serialized_on_its_marker_line_is_masked() {
        let begin = format!("-----BEGIN {}-----", "RSA PRIVATE KEY");
        let body = "MIIEowIBAAKCAQEAthisisadeadbeefexamplebodyforatestcase1234567890";
        let value = format!("{begin}\\n{body}\\n-----END RSA PRIVATE KEY-----");
        let raw = format!("@@ -0,0 +1 @@\n+KEY=\"{value}\"\n");
        let mut diffs = vec![parse_file_patch("src/config.rs", &raw)];

        mask(&mut diffs, &[], &[]);
        let rendered = replay::render(&diffs);

        assert!(!rendered.contains(&value), "{rendered}");
        assert!(!rendered.contains(body), "{rendered}");
        assert!(rendered.contains("KEY="), "{rendered}");
    }

    /// A hunk can omit the closing marker of an earlier PEM block. State must
    /// restart at the later hunk, otherwise ordinary changed code there would
    /// be hidden as if it were key material.
    #[test]
    fn private_key_state_does_not_cross_hunk_boundaries() {
        let begin = format!("-----BEGIN {}-----", "RSA PRIVATE KEY");
        let body = "MIIEowIBAAKCAQEAthisisadeadbeefexamplebodyforatestcase1234567890";
        let raw =
            format!("@@ -0,0 +1,2 @@\n+{begin}\n+{body}\n@@ -10,0 +12 @@\n+let ordinary = true;\n");
        let mut diffs = vec![parse_file_patch("src/config.rs", &raw)];

        mask(&mut diffs, &[], &[]);
        let rendered = replay::render(&diffs);

        assert!(!rendered.contains(body), "{rendered}");
        assert!(rendered.contains("let ordinary = true;"), "{rendered}");
    }

    /// A hunk that opens inside a private key — `BEGIN` in omitted context,
    /// `END` still in view — is masked up to the closing marker.
    #[test]
    fn a_hunk_that_opens_mid_key_is_masked_up_to_the_end_marker() {
        let body = "MIIEowIBAAKCAQEAthisisadeadbeefexamplebodyforatestcase1234567890";
        let end = format!("-----END {} KEY-----", "RSA PRIVATE");
        let raw = format!("@@ -2,3 +2,3 @@\n+{body}\n+{end}\n+let after = 1;\n");
        let mut diffs = vec![parse_file_patch("src/config.rs", &raw)];

        mask(&mut diffs, &[], &[]);
        let rendered = replay::render(&diffs);

        assert!(!rendered.contains(body), "{rendered}");
        assert!(rendered.contains("let after = 1;"), "{rendered}");
    }

    /// Regression for the corpus replay breaking on opencompany#2313: a bare
    /// base64-alphabet line outside any armour — a commit hash, a path, a
    /// long identifier — is ordinary text and must render untouched. The
    /// body heuristic only ever applies inside a key.
    #[test]
    fn a_bare_hash_or_path_line_outside_armour_is_not_masked() {
        let raw = "@@ -1,3 +1,3 @@\n+e75d31e10e6af6ebe699c54c79eafad1aade20e5\n+vendor/tinyhivemind\n+SomeVeryLongIdentifierNameOnItsOwn\n";
        let mut diffs = vec![parse_file_patch("notes.txt", raw)];

        let redactions = mask(&mut diffs, &[], &[]);
        let rendered = replay::render(&diffs);

        assert!(redactions.is_empty(), "{redactions:?}");
        assert!(
            rendered.contains("e75d31e10e6af6ebe699c54c79eafad1aade20e5"),
            "{rendered}"
        );
        assert!(rendered.contains("vendor/tinyhivemind"), "{rendered}");
    }

    /// Regression for a Codex finding on #166: `scan::redact_line`'s rulepack
    /// match now runs on every line kind, not only lines a finding named, so
    /// a live credential sitting in the base revision (a removed or context
    /// line) is masked even though the scanner never looks at those lines.
    #[test]
    fn a_recognisable_credential_in_a_removed_line_is_masked() {
        let key = token("AKIA", "IOSFODNN7EXAMPLE");
        let raw = format!(
            "@@ -1,2 +1,2 @@\n-const OLD_KEY: &str = \"{key}\";\n+const OLD_KEY: &str = \"rotated\";\n let b = 3;\n"
        );
        let mut diffs = vec![parse_file_patch("src/config.rs", &raw)];
        // Deliberately empty: the scanner only ever scans added lines, so no
        // finding names the removed line — the rulepack pass has to catch it
        // on shape alone.
        let findings: Vec<Finding> = Vec::new();

        mask(&mut diffs, &findings, &[]);
        let rendered = replay::render(&diffs);

        assert!(!rendered.contains("IOSFODNN7EXAMPLE"), "{rendered}");
        assert!(rendered.contains("rotated"), "{rendered}");
    }

    /// Regression for a Codex finding on #166: `FileDiff::path` is only the
    /// head-revision path, so a rename out of a sensitive path — `.env` to
    /// `config.txt` — used to disable whole-file masking even though the
    /// base-revision content is exactly as sensitive.
    #[test]
    fn a_rename_out_of_a_sensitive_path_is_still_masked_wholesale() {
        let mut diffs = vec![parse_file_patch(
            "config.txt",
            &patch(&["+FEATURE_FLAG=on"]),
        )];
        let files = vec![ChangedFile {
            path: "config.txt".to_string(),
            previous_path: Some(".env".to_string()),
            ..ChangedFile::default()
        }];
        let findings = scan::secrets::scan_added_lines("config.txt", diffs[0].added_lines());
        assert!(findings.is_empty(), "{findings:#?}");

        mask(&mut diffs, &findings, &files);
        let rendered = replay::render(&diffs);

        assert!(
            !rendered.contains("FEATURE_FLAG=on"),
            "a rename out of .env stays masked on the base revision's shape: {rendered}"
        );
        assert!(rendered.contains("FEATURE_FLAG="), "{rendered}");
    }

    /// Regression for a Codex finding on #166: evidence a previous review
    /// cycle persisted can predate this module entirely and carry a
    /// scanner-shaped credential unmasked. `scrub_rendered` is what
    /// `crate::app::review` now runs over `reviewed_evidence` before it is
    /// replayed into a prompt.
    #[test]
    fn scrub_rendered_masks_a_recognisable_credential_and_keeps_the_anchor() {
        let key = token("AKIA", "IOSFODNN7EXAMPLE");
        let diffs = vec![parse_file_patch(
            "src/config.rs",
            &patch(&[&format!("+const KEY: &str = \"{key}\";")]),
        )];
        let legacy = replay::render(&diffs);
        assert!(legacy.contains("IOSFODNN7EXAMPLE"), "{legacy}");

        let scrubbed = scrub_rendered(&legacy);

        assert!(!scrubbed.contains("IOSFODNN7EXAMPLE"), "{scrubbed}");
        assert!(
            scrubbed.contains("--- src/config.rs"),
            "the file header survives untouched: {scrubbed}"
        );
        assert!(
            scrubbed.contains("1 +const KEY"),
            "the line-number anchor is not touched by masking: {scrubbed}"
        );
    }

    /// Regression for the same finding: a private key's body carries no
    /// rulepack-recognisable shape on its own lines, so `scrub_rendered` has
    /// to track the armour block the same way `mask` does.
    #[test]
    fn scrub_rendered_masks_a_private_key_body_between_its_markers() {
        let begin = format!("-----BEGIN {}-----", "RSA PRIVATE KEY");
        let end = format!("-----END {}-----", "RSA PRIVATE KEY");
        let body = "MIIEowIBAAKCAQEAthisisadeadbeefexamplebodyforatestcase1234567890";
        let diffs = vec![parse_file_patch(
            "src/config.rs",
            &format!("@@ -0,0 +1,3 @@\n+{begin}\n+{body}\n+{end}\n"),
        )];
        let legacy = replay::render(&diffs);

        let scrubbed = scrub_rendered(&legacy);

        assert!(!scrubbed.contains(body), "{scrubbed}");
        assert!(scrubbed.contains(&begin), "{scrubbed}");
    }

    /// Header lines (`--- path`, `@@ ... @@`) are shorter than the fixed
    /// 7-byte line-content prefix, or do not carry a marker at that offset;
    /// `scrub_rendered` must not corrupt them while still running the
    /// rulepack over their text.
    #[test]
    fn scrub_rendered_leaves_header_lines_intact() {
        let text = "--- src/config.rs\n@@ -1,1 +1,1 @@\n";
        assert_eq!(scrub_rendered(text), text);
    }
}
