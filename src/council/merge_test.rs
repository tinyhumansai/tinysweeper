//! Folding reviewers together, and the properties that make it safe to leave on.

use super::*;
use crate::config::types::{LaneId, Severity};

fn finding(path: &str, line: u64, rule: &str, confidence: f64) -> Finding {
    Finding {
        lane: LaneId::Critique,
        severity: Severity::Medium,
        confidence,
        path: path.into(),
        line: Some(line),
        end_line: None,
        rule: rule.into(),
        title: format!("Finding at {path}:{line}"),
        body: "why it matters".into(),
        suggestion: None,
        late: false,
        identity: None,
        applicable: None,
        corroboration: 1,
    }
}

#[test]
fn one_reviewer_is_returned_untouched() {
    // The property that makes turning the council on with one agent a provable
    // no-op, and therefore the one worth asserting first.
    let only = vec![
        finding("src/a.rs", 10, "unchecked-index", 0.7),
        finding("src/b.rs", 3, "leak", 0.9),
    ];
    let merged = merge(vec![only.clone()]);

    assert_eq!(merged.len(), 2);
    assert_eq!(merged[0].confidence, only[0].confidence);
    assert_eq!(merged[1].confidence, only[1].confidence);
    assert_eq!(merged[0].corroboration, 1);
    assert_eq!(merged[0].title, only[0].title);
}

#[test]
fn two_reviewers_on_the_same_line_become_one_finding() {
    let merged = merge(vec![
        vec![finding("src/a.rs", 10, "unchecked-index", 0.6)],
        // A different rule id for the same defect — which is the normal case,
        // because `rule` is model-authored free text.
        vec![finding("src/a.rs", 11, "missing-bounds-check", 0.6)],
    ]);

    assert_eq!(merged.len(), 1, "{merged:?}");
    assert_eq!(merged[0].corroboration, 2);
    // Noisy-OR: 1 - 0.4*0.4.
    assert!(
        (merged[0].confidence - 0.84).abs() < 1e-9,
        "{}",
        merged[0].confidence
    );
}

#[test]
fn merging_never_lowers_a_confidence() {
    // The invariant that makes the merge safe to leave on: no finding is ever
    // worse off for the council having run, so enabling it cannot silently
    // drop something below `confidence_min`.
    for a in [0.0, 0.1, 0.5, 0.75, 0.99, 1.0] {
        for b in [0.0, 0.1, 0.5, 0.75, 0.99, 1.0] {
            let merged = merge(vec![
                vec![finding("src/a.rs", 10, "x", a)],
                vec![finding("src/a.rs", 10, "y", b)],
            ]);
            let combined = merged[0].confidence;
            assert!(
                combined >= a.min(0.99) - 1e-9 && combined >= b.min(0.99) - 1e-9,
                "{a} + {b} = {combined}"
            );
            assert!(combined <= 0.99 + 1e-9, "{a} + {b} = {combined}");
        }
    }
}

#[test]
fn the_representative_is_one_reviewers_words_and_not_a_blend() {
    // A merge step that can author text is a second reviewer nobody gated.
    let mut weak = finding("src/a.rs", 10, "x", 0.4);
    weak.title = "Possibly unchecked".into();
    weak.body = "not sure".into();
    let mut strong = finding("src/a.rs", 10, "y", 0.9);
    strong.title = "Guard the index".into();
    strong.body = "`items[i]` panics on an empty slice.".into();

    let merged = merge(vec![vec![weak], vec![strong]]);

    assert_eq!(merged[0].title, "Guard the index");
    assert_eq!(merged[0].body, "`items[i]` panics on an empty slice.");
}

#[test]
fn merging_keeps_the_highest_severity_anyone_assigned() {
    // Merging must not talk a review down. A reviewer outvoted on wording keeps
    // its opinion about how much the defect matters.
    let mut minor = finding("src/a.rs", 10, "x", 0.9);
    minor.severity = Severity::Low;
    let mut major = finding("src/a.rs", 10, "y", 0.5);
    major.severity = Severity::Critical;

    let merged = merge(vec![vec![minor], vec![major]]);
    assert_eq!(merged[0].severity, Severity::Critical);
    // ...while the clearer statement still supplies the words.
    assert!(merged[0].title.contains("src/a.rs:10"));
}

#[test]
fn the_identity_of_the_first_sighting_survives_a_replacement() {
    // `identity` is what the `tinysweeper:fp=` marker carries and what
    // suppression reads back. Letting the representative bring its own would
    // repost a finding that had already been answered.
    let mut first = finding("src/a.rs", 10, "x", 0.4);
    first.identity = Some("firstfp0".into());
    let mut second = finding("src/a.rs", 10, "y", 0.9);
    second.identity = Some("secondfp".into());

    let merged = merge(vec![vec![first], vec![second]]);
    assert_eq!(merged[0].identity.as_deref(), Some("firstfp0"));
}

#[test]
fn a_singleton_passes_through_on_its_own_merit() {
    // The finding only one reviewer could see is the entire reason for running
    // more than one. Gating on agreement would delete exactly these.
    let merged = merge(vec![
        vec![finding("src/a.rs", 10, "x", 0.8)],
        vec![finding("src/b.rs", 99, "y", 0.65)],
    ]);

    assert_eq!(merged.len(), 2);
    let lonely = merged.iter().find(|f| f.path == "src/b.rs").expect("kept");
    assert_eq!(lonely.confidence, 0.65, "unchanged");
    assert_eq!(lonely.corroboration, 1);
}

#[test]
fn the_same_rule_on_two_call_sites_stays_two_findings() {
    let merged = merge(vec![
        vec![
            finding("src/a.rs", 10, "unchecked-index", 0.8),
            finding("src/a.rs", 90, "unchecked-index", 0.8),
        ],
        vec![finding("src/a.rs", 91, "unchecked-index", 0.8)],
    ]);

    assert_eq!(merged.len(), 2, "{merged:?}");
    assert_eq!(merged[0].corroboration, 1);
    assert_eq!(merged[1].corroboration, 2);
}

#[test]
fn three_reviewers_accumulate_rather_than_pairing_off() {
    let merged = merge(vec![
        vec![finding("src/a.rs", 10, "x", 0.5)],
        vec![finding("src/a.rs", 10, "y", 0.5)],
        vec![finding("src/a.rs", 10, "z", 0.5)],
    ]);

    assert_eq!(merged.len(), 1);
    assert_eq!(merged[0].corroboration, 3);
    // 1 - 0.5^3.
    assert!((merged[0].confidence - 0.875).abs() < 1e-9);
}

#[test]
fn an_empty_council_merges_to_nothing_rather_than_panicking() {
    assert!(merge(vec![]).is_empty());
    assert!(merge(vec![vec![], vec![]]).is_empty());
}

#[test]
fn one_reviewers_own_findings_are_never_merged_into_each_other() {
    // The council must not change behaviour at a single agent. Two findings
    // three lines apart from one reviewer stay two — merging them would make
    // enabling the merge a silent behaviour change, and it is not the council's
    // job anyway: one reviewer repeating itself is a dedupe question that
    // `lane_proposal` already owns.
    //
    // Regression: this shipped, and `eval` caught it on `ts-0068`.
    let merged = merge(vec![vec![
        finding("src/a.rs", 10, "x", 0.7),
        finding("src/a.rs", 11, "y", 0.7),
    ]]);

    assert_eq!(merged.len(), 2, "{merged:?}");
    assert_eq!(merged[0].corroboration, 1);
    assert_eq!(merged[1].corroboration, 1);
    assert_eq!(merged[0].confidence, 0.7);
}

#[test]
fn a_second_reviewer_absorbs_each_finding_at_most_once() {
    // Two findings from the second reviewer both land near one from the first.
    // Letting both absorb would report three reviewers agreeing when only two
    // ran, and would delete a finding the second reviewer meant separately.
    let merged = merge(vec![
        vec![finding("src/a.rs", 10, "x", 0.5)],
        vec![
            finding("src/a.rs", 10, "y", 0.5),
            finding("src/a.rs", 11, "z", 0.5),
        ],
    ]);

    assert_eq!(merged.len(), 2, "{merged:?}");
    assert_eq!(merged[0].corroboration, 2);
    assert_eq!(merged[1].corroboration, 1);
}
