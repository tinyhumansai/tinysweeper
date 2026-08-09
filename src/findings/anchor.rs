//! The code a finding's identity is computed over.
//!
//! [`Finding::fingerprint`](crate::findings::Finding::fingerprint) hashes the
//! lane, the path, the rule and a *snippet of the code* — deliberately not the
//! line number, the severity or the prose. Producing that snippet needs the
//! parsed diff, which the apply path does not have, so it happens once during
//! review and the result travels in the proposal.
//!
//! This is the difference between dedupe that holds across pushes and dedupe
//! that does not: the fingerprint written into the `tinysweeper:fp=` marker
//! used to be taken over the finding's *title*, so a model rewording its own
//! sentence minted a fresh identity and the comment was posted again.

use crate::evidence::diff::FileDiff;
use crate::findings::types::Finding;

/// The changed-line text `finding` anchors to, as the fingerprint sees it.
///
/// Empty when the finding names no line, or names a file the diff no longer
/// contains. Empty is a usable identity rather than a failure: the fingerprint
/// still covers the lane, the path and the rule, so two findings only collide
/// when they are genuinely the same rule on the same file with no anchor to
/// tell them apart.
pub fn anchor_context(finding: &Finding, diffs: &[FileDiff]) -> String {
    let Some((start, end)) = finding.range() else {
        return String::new();
    };
    let Some(diff) = diffs.iter().find(|d| d.path == finding.path) else {
        return String::new();
    };

    let mut lines = Vec::new();
    for hunk in &diff.hunks {
        for line in &hunk.lines {
            if let Some(number) = line.new_line
                && (start..=end).contains(&number)
            {
                lines.push(line.text.as_str());
            }
        }
    }
    lines.join("\n")
}

/// Stamp every finding with the identity dedupe and triage key on, and with
/// the applicable suggestion if one can be built.
///
/// Done in one place so review, apply and the dedupe reader cannot disagree
/// about what a finding *is*. The suggestion joins it for the same reason it
/// exists here at all: both need the parsed diff, and the apply path has none.
///
/// Order matters. The identity is computed first and over the *original* code,
/// so stamping a suggestion cannot move a finding's fingerprint and re-post a
/// comment that was already deduped away.
pub fn stamp(findings: &mut [Finding], diffs: &[FileDiff]) {
    for finding in findings.iter_mut() {
        let context = anchor_context(finding, diffs);
        finding.identity = Some(finding.fingerprint(&context));
        finding.applicable = crate::findings::suggest::applicable(finding, diffs);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::types::{LaneId, Severity};
    use crate::evidence::diff::parse_file_patch;

    const PATCH: &str =
        "@@ -1,3 +1,5 @@\n fn main() {\n+    let x = items[i];\n+    println!(\"{x}\");\n }\n";

    fn diffs() -> Vec<FileDiff> {
        vec![parse_file_patch("src/main.rs", PATCH)]
    }

    fn finding(line: Option<u64>) -> Finding {
        Finding {
            lane: LaneId::Critique,
            severity: Severity::High,
            confidence: 0.9,
            path: "src/main.rs".into(),
            line,
            end_line: None,
            rule: "unchecked-index".into(),
            title: "Guard the index before dereferencing".into(),
            body: "…".into(),
            suggestion: None,
            applicable: None,
            late: false,
            identity: None,
            corroboration: 1,
        }
    }

    #[test]
    fn the_anchor_is_the_line_the_finding_points_at() {
        assert_eq!(
            anchor_context(&finding(Some(2)), &diffs()),
            "    let x = items[i];"
        );
    }

    #[test]
    fn a_range_gathers_every_line_in_it() {
        let mut spanning = finding(Some(2));
        spanning.end_line = Some(3);
        let context = anchor_context(&spanning, &diffs());
        assert!(context.contains("items[i]"));
        assert!(context.contains("println!"));
    }

    #[test]
    fn an_unanchored_finding_yields_an_empty_context_rather_than_a_panic() {
        assert_eq!(anchor_context(&finding(None), &diffs()), "");
        let mut elsewhere = finding(Some(2));
        elsewhere.path = "src/other.rs".into();
        assert_eq!(anchor_context(&elsewhere, &diffs()), "");
    }

    #[test]
    fn a_reworded_title_does_not_change_the_identity() {
        // The whole reason the identity is stamped here rather than in `apply`
        // from the title: a model that rephrases itself must not resurrect a
        // finding that has already been posted.
        let mut first = [finding(Some(2))];
        let mut reworded = [finding(Some(2))];
        reworded[0].title = "Check the bound first".into();
        reworded[0].body = "different words entirely".into();
        reworded[0].severity = Severity::Medium;

        stamp(&mut first, &diffs());
        stamp(&mut reworded, &diffs());
        assert_eq!(first[0].identity, reworded[0].identity);
    }

    #[test]
    fn a_finding_that_moved_down_the_file_keeps_its_identity() {
        // The same added line, three lines lower after an import landed above
        // it. Re-posting that is exactly the noise dedupe exists to stop.
        let shifted = "@@ -1,6 +1,8 @@\n use std::io;\n \n \n fn main() {\n+    let x = items[i];\n+    println!(\"{x}\");\n }\n";
        let mut before = [finding(Some(2))];
        let mut after = [finding(Some(5))];

        stamp(&mut before, &diffs());
        stamp(&mut after, &[parse_file_patch("src/main.rs", shifted)]);
        assert_eq!(before[0].identity, after[0].identity);
    }

    #[test]
    fn two_call_sites_of_the_same_rule_stay_two_findings() {
        let mut findings = [finding(Some(2)), finding(Some(3))];
        stamp(&mut findings, &diffs());
        assert_ne!(findings[0].identity, findings[1].identity);
    }
}
