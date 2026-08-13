//! Where a model-produced finding is allowed to point.
//!
//! Two rules, and the difference between them is the whole module:
//!
//! - A **file-scoped** lane (`critique`, `security`, `tests`) reports about
//!   code. Its findings must sit on a line this pull request changed, and one
//!   that does not is dropped. Posting a comment on unrelated code is the
//!   fastest way for a review bot to lose a team's trust.
//! - A **pull-request-scoped** lane (`commits`, `description`) reports about
//!   things that have no line at all: a commit message, an empty body. Dropping
//!   those would silence the lane entirely, so instead the anchor is *removed*
//!   and the finding becomes summary-only — `apply` posts an inline comment
//!   only for a finding that has a line, and renders the rest in the summary
//!   table.

use crate::evidence::diff::FileDiff;
use crate::findings::types::Finding;

/// Whether a finding points at a line this pull request actually changed.
///
/// A finding marked `late` is exempt — it is explicitly about untouched code,
/// and the model had to justify that separately.
pub fn anchored_in_diff(finding: &Finding, diffs: &[FileDiff]) -> bool {
    if finding.late {
        return diffs.iter().any(|d| d.path == finding.path);
    }

    let Some((start, end)) = finding.range() else {
        return false;
    };
    diffs
        .iter()
        .find(|d| d.path == finding.path)
        .is_some_and(|d| d.touches_range(start, end))
}

/// Demote a finding that does not sit on a changed line to summary-only.
///
/// Used by the lanes whose subject matter has no line number. Keeping the bad
/// anchor would put a comment on unrelated code; dropping the finding would
/// throw away the lane's actual job.
pub fn demote_unanchored(mut finding: Finding, diffs: &[FileDiff]) -> Finding {
    if !anchored_in_diff(&finding, diffs) {
        finding.line = None;
        finding.end_line = None;
    }
    finding
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::types::{LaneId, Severity};
    use crate::evidence::diff::parse_file_patch;

    fn diffs() -> Vec<FileDiff> {
        vec![parse_file_patch(
            "src/main.rs",
            "@@ -1,2 +1,3 @@\n fn main() {\n+    let x = 1;\n }\n",
        )]
    }

    fn finding(path: &str, line: Option<u64>) -> Finding {
        Finding {
            lane: LaneId::Commits,
            severity: Severity::Medium,
            confidence: 0.9,
            path: path.into(),
            line,
            end_line: None,
            rule: "r".into(),
            title: "t".into(),
            body: "b".into(),
            suggestion: None,
            applicable: None,
            late: false,
            identity: None,
            corroboration: 1,
        }
    }

    #[test]
    fn a_changed_line_anchors() {
        assert!(anchored_in_diff(&finding("src/main.rs", Some(2)), &diffs()));
    }

    #[test]
    fn a_context_line_does_not() {
        assert!(!anchored_in_diff(
            &finding("src/main.rs", Some(1)),
            &diffs()
        ));
    }

    #[test]
    fn a_late_finding_only_needs_the_file_to_be_touched() {
        let mut late = finding("src/main.rs", Some(1));
        late.late = true;
        assert!(anchored_in_diff(&late, &diffs()));
    }

    #[test]
    fn demotion_keeps_the_finding_and_drops_only_the_anchor() {
        let demoted = demote_unanchored(finding("docs/README.md", Some(9)), &diffs());
        assert_eq!(demoted.line, None, "a bad anchor becomes no anchor");
        assert_eq!(demoted.title, "t", "the finding itself survives");
    }

    #[test]
    fn a_good_anchor_survives_demotion_untouched() {
        let kept = demote_unanchored(finding("src/main.rs", Some(2)), &diffs());
        assert_eq!(kept.line, Some(2));
    }
}
