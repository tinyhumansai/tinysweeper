//! Golden tests for the propose/verify split.
//!
//! These are the tests that keep noise control honest: every one of them is a
//! way the panel could quietly get louder or quietly get blind.

use super::*;
use crate::config::types::Severity;

fn finding(path: &str, rule: &str, code: &str, title: &str, confidence: f64) -> RawFinding {
    RawFinding {
        path: path.into(),
        existing_code: Some(code.into()),
        line: None,
        end_line: None,
        rule: rule.into(),
        title: title.into(),
        body: format!("because of {rule}"),
        severity: Severity::High,
        confidence,
        suggestion: None,
        late: false,
    }
}

fn opinion(lens: &str, findings: Vec<RawFinding>) -> Opinion {
    Opinion {
        lens: lens.into(),
        model: "vendor/flash".into(),
        response: LaneResponse {
            summary: format!("{lens} looked"),
            findings,
            resolved: vec![],
        },
    }
}

fn all_real(n: usize, count: usize) -> Vec<Vec<Verdict>> {
    vec![vec![Verdict { real: true }; count]; n]
}

#[test]
fn two_lenses_reporting_one_problem_report_it_once() {
    // The double-reporting this whole design has to avoid: N panellists reading
    // the same diff all notice the same thing.
    let proposals = propose(&[
        opinion(
            "correctness",
            vec![finding("a.rs", "unwrap", "x.unwrap()", "Avoid unwrap", 0.6)],
        ),
        opinion(
            "security",
            vec![finding("a.rs", "unwrap", "x.unwrap()", "Panic on bad input", 0.9)],
        ),
    ]);

    assert_eq!(proposals.len(), 1);
    assert_eq!(proposals[0].lenses, vec!["correctness", "security"]);
}

#[test]
fn the_better_argued_version_of_a_shared_finding_wins() {
    // Merging must not let a panellist that merely also noticed something
    // overwrite the one that explained it best.
    let proposals = propose(&[
        opinion(
            "correctness",
            vec![finding("a.rs", "unwrap", "x.unwrap()", "Avoid unwrap", 0.6)],
        ),
        opinion(
            "security",
            vec![finding("a.rs", "unwrap", "x.unwrap()", "Panic on bad input", 0.9)],
        ),
    ]);

    assert_eq!(proposals[0].finding.title, "Panic on bad input");
    // ...and the attribution survives being overwritten.
    assert_eq!(proposals[0].lenses, vec!["correctness", "security"]);
}

#[test]
fn re_indented_quotes_of_one_hunk_are_one_finding() {
    // Models re-indent what they quote. An indentation difference is not a
    // different finding, and treating it as one reports the same problem twice.
    let proposals = propose(&[
        opinion("a", vec![finding("a.rs", "unwrap", "x.unwrap()", "One", 0.5)]),
        opinion(
            "b",
            vec![finding("a.rs", "unwrap", "    x.unwrap()\n", "Two", 0.5)],
        ),
    ]);

    assert_eq!(proposals.len(), 1);
}

#[test]
fn a_specialists_lone_finding_is_not_discarded_for_lack_of_agreement() {
    // The reason this is a union and not a vote. A tests-focused panellist
    // cannot see a trust boundary move, so requiring it to agree would throw
    // away precisely what the security lens was added to catch.
    let proposals = propose(&[
        opinion(
            "security",
            vec![finding("a.rs", "authz", "if admin {", "Check moved", 0.9)],
        ),
        opinion("tests", vec![]),
        opinion("correctness", vec![]),
    ]);

    assert_eq!(proposals.len(), 1);
    assert_eq!(proposals[0].lenses, vec!["security"]);
}

#[test]
fn different_code_under_one_rule_stays_two_findings() {
    let proposals = propose(&[opinion(
        "correctness",
        vec![
            finding("a.rs", "unwrap", "x.unwrap()", "One", 0.5),
            finding("a.rs", "unwrap", "y.unwrap()", "Two", 0.5),
        ],
    )]);

    assert_eq!(proposals.len(), 2);
}

#[test]
fn one_problem_in_two_files_stays_two_findings() {
    let proposals = propose(&[opinion(
        "correctness",
        vec![
            finding("a.rs", "unwrap", "x.unwrap()", "One", 0.5),
            finding("b.rs", "unwrap", "x.unwrap()", "One", 0.5),
        ],
    )]);

    assert_eq!(proposals.len(), 2);
}

#[test]
fn proposal_order_is_the_panels_order_not_the_keys() {
    // `BTreeMap` sorts by an identity tuple whose order means nothing to
    // someone reading the resulting comment. Insertion order is tracked
    // separately so a run over the same evidence is reproducible *and* legible.
    let proposals = propose(&[opinion(
        "correctness",
        vec![
            finding("z.rs", "zeta", "z()", "Last alphabetically first seen", 0.5),
            finding("a.rs", "alpha", "a()", "First alphabetically seen second", 0.5),
        ],
    )]);

    assert_eq!(proposals[0].finding.path, "z.rs");
    assert_eq!(proposals[1].finding.path, "a.rs");
}

#[test]
fn a_majority_of_verifiers_keeps_a_finding() {
    let proposals = propose(&[opinion(
        "security",
        vec![finding("a.rs", "authz", "if admin {", "Check moved", 0.9)],
    )]);

    let kept = settle(
        proposals,
        &[vec![
            Verdict { real: true },
            Verdict { real: true },
            Verdict { real: false },
        ]],
    );

    assert_eq!(kept.len(), 1);
}

#[test]
fn a_minority_of_verifiers_drops_it() {
    let proposals = propose(&[opinion(
        "security",
        vec![finding("a.rs", "authz", "if admin {", "Check moved", 0.9)],
    )]);

    let kept = settle(
        proposals,
        &[vec![
            Verdict { real: true },
            Verdict { real: false },
            Verdict { real: false },
        ]],
    );

    assert!(kept.is_empty());
}

#[test]
fn an_even_split_drops_it() {
    // A tie is not agreement. Keeping on a tie makes two verifiers strictly
    // worse than one, which would be a strange thing for a panel to do.
    let proposals = propose(&[opinion(
        "security",
        vec![finding("a.rs", "authz", "if admin {", "Check moved", 0.9)],
    )]);

    let kept = settle(
        proposals,
        &[vec![Verdict { real: true }, Verdict { real: false }]],
    );

    assert!(kept.is_empty());
}

#[test]
fn an_unverified_proposal_is_dropped_rather_than_trusted() {
    // Defaulting to "keep" would make a verifier outage silently restore the
    // pre-panel noise level — the failure would look exactly like a clean run.
    let proposals = propose(&[opinion(
        "security",
        vec![finding("a.rs", "authz", "if admin {", "Check moved", 0.9)],
    )]);

    assert!(settle(proposals, &[vec![]]).is_empty());
}

#[test]
fn settling_keeps_findings_aligned_with_their_own_verdicts() {
    // `zip` over two parallel vectors is the kind of thing that silently
    // reports finding A's text against finding B's verdict.
    let proposals = propose(&[opinion(
        "correctness",
        vec![
            finding("a.rs", "one", "a()", "Keep me", 0.5),
            finding("b.rs", "two", "b()", "Drop me", 0.5),
            finding("c.rs", "three", "c()", "Keep me too", 0.5),
        ],
    )]);

    let kept = settle(
        proposals,
        &[
            vec![Verdict { real: true }],
            vec![Verdict { real: false }],
            vec![Verdict { real: true }],
        ],
    );

    let titles: Vec<&str> = kept.iter().map(|f| f.title.as_str()).collect();
    assert_eq!(titles, vec!["Keep me", "Keep me too"]);
}

#[test]
fn an_empty_panel_proposes_nothing_and_settles_to_nothing() {
    assert!(propose(&[]).is_empty());
    assert!(settle(vec![], &[]).is_empty());
}

#[test]
fn resolutions_are_unioned_across_the_panel() {
    // Asymmetric on purpose: retiring a finding early costs a re-report, while
    // leaving a fixed one standing means arguing with someone who already did
    // the work.
    let titles = resolved(&[
        Opinion {
            lens: "a".into(),
            model: "vendor/flash".into(),
            response: LaneResponse {
                summary: String::new(),
                findings: vec![],
                resolved: vec!["Handle the empty case".into()],
            },
        },
        Opinion {
            lens: "b".into(),
            model: "vendor/flash".into(),
            response: LaneResponse {
                summary: String::new(),
                findings: vec![],
                resolved: vec!["handle the empty case".into(), "Name the error".into()],
            },
        },
    ]);

    assert_eq!(titles, vec!["Handle the empty case", "Name the error"]);
}
