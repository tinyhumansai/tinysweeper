//! When two reviewers are talking about the same thing.

use super::*;
use crate::config::types::{LaneId, Severity};

fn finding(path: &str, line: Option<u64>, rule: &str) -> Finding {
    Finding {
        lane: LaneId::Critique,
        severity: Severity::Medium,
        confidence: 0.8,
        path: path.into(),
        line,
        end_line: None,
        rule: rule.into(),
        title: "t".into(),
        body: "b".into(),
        suggestion: None,
        late: false,
        identity: None,
        applicable: None,
        corroboration: 1,
    }
}

#[test]
fn the_same_line_with_different_rule_ids_corroborates() {
    // The case the fingerprint gets wrong. `rule` is model-authored free text,
    // so two reviewers on one missing bounds check will name it differently —
    // and grouping on the fingerprint would post both.
    let a = finding("src/a.rs", Some(10), "unchecked-index");
    let b = finding("src/a.rs", Some(10), "missing-bounds-check");
    assert!(corroborates(&a, &b));
}

#[test]
fn wording_is_never_evidence_either_way() {
    // Two agents describing one defect word it differently by construction —
    // that is the entire reason for running more than one.
    let mut a = finding("src/a.rs", Some(10), "x");
    a.title = "Guard the index".into();
    a.body = "panics".into();
    let mut b = finding("src/a.rs", Some(10), "x");
    b.title = "Bounds check missing before dereference".into();
    b.body = "this will abort the process".into();

    assert!(corroborates(&a, &b));
}

#[test]
fn different_files_never_corroborate() {
    let a = finding("src/a.rs", Some(10), "x");
    let b = finding("src/b.rs", Some(10), "x");
    assert!(!corroborates(&a, &b));
}

#[test]
fn different_lanes_never_corroborate() {
    // `Finding::fingerprint` hashes the lane first, so critique and security
    // can never collide — and two lanes are two subjects, not two opinions.
    let a = finding("src/a.rs", Some(10), "x");
    let mut b = finding("src/a.rs", Some(10), "x");
    b.lane = LaneId::Security;
    assert!(!corroborates(&a, &b));
}

#[test]
fn the_tolerance_is_three_lines_either_way() {
    let anchor = finding("src/a.rs", Some(10), "x");
    for line in [7, 10, 13] {
        assert!(
            corroborates(&anchor, &finding("src/a.rs", Some(line), "y")),
            "line {line} should corroborate"
        );
    }
    for line in [6, 14] {
        assert!(
            !corroborates(&anchor, &finding("src/a.rs", Some(line), "y")),
            "line {line} should not"
        );
    }
}

#[test]
fn overlapping_ranges_corroborate_even_when_the_starts_differ() {
    let mut a = finding("src/a.rs", Some(10), "x");
    a.end_line = Some(40);
    let b = finding("src/a.rs", Some(35), "y");
    assert!(corroborates(&a, &b));
}

#[test]
fn two_unplaceable_findings_on_one_file_corroborate_only_on_the_same_rule() {
    // Both were demoted to the check-run summary, so there is no line to
    // compare and the rule id is the only evidence left. Same rule is a repeat;
    // different rules are two defects that happen to share a file, and merging
    // those throws one away. `.github/workflows/eval.yml` on `tinysweeper#86`
    // produced exactly that: an unpinned `dtolnay/rust-toolchain` and an
    // unpinned `Swatinem/rust-cache`, both real, both unplaceable.
    let unpinned_toolchain = finding(".github/workflows/eval.yml", None, "unpinned-action");
    let also_unpinned_toolchain = finding(".github/workflows/eval.yml", None, "unpinned-action");
    assert!(corroborates(&unpinned_toolchain, &also_unpinned_toolchain));

    let unpinned_cache = finding(".github/workflows/eval.yml", None, "mutable-action-tag");
    assert!(!corroborates(&unpinned_toolchain, &unpinned_cache));
}

#[test]
fn a_placed_finding_does_not_absorb_an_unplaceable_one() {
    // They may well be the same defect, but there is no evidence of it, and
    // merging on no evidence silently deletes a finding.
    let placed = finding("src/a.rs", Some(10), "x");
    let floating = finding("src/a.rs", None, "y");
    assert!(!corroborates(&placed, &floating));
    assert!(!corroborates(&floating, &placed));
}

#[test]
fn an_identical_fingerprint_is_conclusive() {
    // Same lane, path, rule and anchored code is the same finding by the
    // strictest rule the crate has — so it corroborates even where the line
    // numbers have drifted apart.
    let mut a = finding("src/a.rs", Some(10), "x");
    a.identity = Some("deadbeef".into());
    let mut b = finding("src/a.rs", Some(400), "x");
    b.identity = Some("deadbeef".into());

    assert!(corroborates(&a, &b));
}

#[test]
fn corroboration_is_symmetric() {
    let a = finding("src/a.rs", Some(10), "x");
    let b = finding("src/a.rs", Some(12), "y");
    assert_eq!(corroborates(&a, &b), corroborates(&b, &a));
}
