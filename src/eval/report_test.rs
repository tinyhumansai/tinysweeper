//! Rendering and comparing scorecards.

use std::collections::BTreeMap;

use super::*;
use crate::config::types::LaneId;
use crate::eval::types::{Judged, Verdict};

fn case_score(id: &str, found: usize, missed: &[&str], fp: usize) -> CaseScore {
    CaseScore {
        id: id.into(),
        true_positives: found,
        missed: missed.iter().map(|s| (*s).to_string()).collect(),
        false_positives: fp,
        unscored: 0,
        exhaustive: true,
        duplicates: 0,
        forbidden_hits: Vec::new(),
        optional_hits: 0,
        judged: Vec::new(),
        cost_usd: 0.004,
        over_budget: false,
        input_tokens: 1_000,
        output_tokens: 100,
        cached_tokens: 700,
        wall_secs: 2.5,
        models: vec!["z-ai/glm-5.2".into()],
        lane_costs: BTreeMap::new(),
        error: None,
    }
}

fn card(cases: Vec<CaseScore>) -> Scorecard {
    Scorecard {
        corpus_digest: "aaaaaaaaaaaaaaaa".into(),
        config_digest: "bbbbbbbbbbbbbbbb".into(),
        loose_replays: 0,
        cases,
    }
}

#[test]
fn recall_and_precision_are_computed_over_the_whole_corpus() {
    let scored = card(vec![
        case_score("ts-0001", 2, &["E3"], 1),
        case_score("ts-0002", 1, &[], 0),
    ]);

    // 3 found of 4 required.
    assert_eq!(scored.recall(), Some(0.75));
    // 3 true of 4 posted.
    assert_eq!(scored.precision(), Some(0.75));
    assert_eq!(scored.f1(), Some(0.75));
}

#[test]
fn an_all_clean_corpus_reports_no_recall_rather_than_dividing_by_zero() {
    // A corpus that asserts nothing to find is a real corpus — it is how noise
    // is measured — and it must not render as 0% or NaN.
    let scored = card(vec![case_score("ts-clean", 0, &[], 0)]);
    assert_eq!(scored.recall(), None);
    assert!(markdown(&scored, None).contains("| recall | n/a |"));
}

#[test]
fn a_forbidden_finding_counts_against_precision_as_well_as_being_named() {
    let mut case = case_score("ts-0001", 1, &[], 0);
    case.forbidden_hits = vec!["F1".into()];
    let scored = card(vec![case]);

    // It is a false positive somebody already wrote down; excluding it from
    // precision would make the corpus reward findings it explicitly forbids.
    assert_eq!(scored.precision(), Some(0.5));
    assert_eq!(scored.forbidden_hits(), 1);
}

#[test]
fn findings_on_clean_pull_requests_are_reported_on_their_own() {
    let mut clean = case_score("ts-clean", 0, &[], 2);
    clean.judged = vec![
        Judged {
            lane: LaneId::Critique,
            path: "src/a.rs".into(),
            line: Some(4),
            title: "Rename this".into(),
            verdict: Verdict::FalsePositive,
            matched: None,
            reason: "nothing is expected in src/a.rs".into(),
        },
        Judged {
            lane: LaneId::Tests,
            path: "src/b.rs".into(),
            line: None,
            title: "Add a test".into(),
            verdict: Verdict::FalsePositive,
            matched: None,
            reason: "nothing is expected in src/b.rs".into(),
        },
    ];
    let scored = card(vec![clean, case_score("ts-0001", 1, &[], 0)]);

    assert_eq!(scored.clean_case_findings(), 2);
    let rendered = markdown(&scored, None);
    // The most legible noise number in the report: every one is a comment on a
    // pull request that had nothing wrong with it.
    assert!(
        rendered.contains("Noise on clean pull requests"),
        "{rendered}"
    );
    assert!(rendered.contains("Rename this"), "{rendered}");
}

#[test]
fn a_loose_replay_is_announced_before_any_number_is_read() {
    let mut scored = card(vec![case_score("ts-0001", 1, &[], 0)]);
    scored.loose_replays = 3;

    let rendered = markdown(&scored, None);
    let warning = rendered.find("replayed by call order").expect("warned");
    let totals = rendered.find("## Totals").expect("has totals");
    assert!(
        warning < totals,
        "the warning has to come before the table it invalidates"
    );
}

#[test]
fn rendering_is_stable_so_two_reports_diff_only_where_they_differ() {
    let scored = card(vec![case_score("ts-0001", 1, &["E2"], 1)]);
    assert_eq!(markdown(&scored, None), markdown(&scored, None));
}

#[test]
fn a_run_that_matches_its_baseline_passes() {
    let scored = card(vec![case_score("ts-0001", 2, &["E3"], 1)]);
    assert_eq!(compare(&scored, &scored, false), Comparison::Pass);
}

#[test]
fn a_drop_in_recall_fails_but_noise_within_epsilon_does_not() {
    // 20 required expectations, so one finding is 5% — outside the tolerance.
    let baseline = card(vec![case_score("ts-0001", 20, &[], 0)]);
    let regressed = card(vec![case_score("ts-0001", 18, &["E19", "E20"], 0)]);

    match compare(&regressed, &baseline, false) {
        Comparison::Fail(reasons) => {
            assert!(
                reasons.iter().any(|r| r.contains("recall fell")),
                "{reasons:?}"
            )
        }
        other => panic!("expected a failure, got {other:?}"),
    }

    // 100 expectations, one lost: 1%, inside EPSILON. A gate with no tolerance
    // fails on provider routing noise and teaches people to re-run CI.
    let big = card(vec![case_score("ts-0001", 100, &[], 0)]);
    let jittered = card(vec![case_score("ts-0001", 99, &["E100"], 0)]);
    assert_eq!(compare(&jittered, &big, false), Comparison::Pass);
}

#[test]
fn more_forbidden_findings_fails_with_no_tolerance_at_all() {
    let baseline = card(vec![case_score("ts-0001", 1, &[], 0)]);
    let mut worse_case = case_score("ts-0001", 1, &[], 0);
    worse_case.forbidden_hits = vec!["F1".into()];
    let worse = card(vec![worse_case]);

    // Saying a thing the corpus explicitly rules out is never noise.
    match compare(&worse, &baseline, false) {
        Comparison::Fail(reasons) => {
            assert!(
                reasons.iter().any(|r| r.contains("forbidden")),
                "{reasons:?}"
            )
        }
        other => panic!("expected a failure, got {other:?}"),
    }
}

#[test]
fn a_run_that_broke_more_cases_fails_rather_than_scoring_well() {
    let baseline = card(vec![case_score("ts-0001", 1, &[], 0)]);
    let mut broken_case = case_score("ts-0001", 1, &[], 0);
    broken_case.error = Some("provider timed out".into());
    let broken = card(vec![broken_case]);

    match compare(&broken, &baseline, false) {
        Comparison::Fail(reasons) => assert!(
            reasons.iter().any(|r| r.contains("failed to review")),
            "{reasons:?}"
        ),
        other => panic!("expected a failure, got {other:?}"),
    }
}

#[test]
fn a_corpus_that_moved_is_not_comparable_without_saying_so() {
    let baseline = card(vec![case_score("ts-0001", 1, &[], 0)]);
    let mut relabelled = card(vec![case_score("ts-0001", 1, &[], 0)]);
    relabelled.corpus_digest = "cccccccccccccccc".into();

    match compare(&relabelled, &baseline, false) {
        Comparison::Incomparable(why) => assert!(why.contains("corpus changed"), "{why}"),
        other => panic!("expected incomparable, got {other:?}"),
    }
    // Explicitly overridable, because sometimes you do want to look.
    assert_eq!(compare(&relabelled, &baseline, true), Comparison::Pass);
}

#[test]
fn a_configuration_that_moved_is_not_comparable_without_saying_so() {
    // A stricter gate finds fewer things without the reviewer having got worse.
    let baseline = card(vec![case_score("ts-0001", 1, &[], 0)]);
    let mut reconfigured = card(vec![case_score("ts-0001", 1, &[], 0)]);
    reconfigured.config_digest = "dddddddddddddddd".into();

    match compare(&reconfigured, &baseline, false) {
        Comparison::Incomparable(why) => assert!(why.contains("configuration changed"), "{why}"),
        other => panic!("expected incomparable, got {other:?}"),
    }
}

#[test]
fn the_baseline_section_shows_both_sides_of_every_number() {
    let baseline = card(vec![case_score("ts-0001", 2, &[], 0)]);
    let current = card(vec![case_score("ts-0001", 2, &[], 1)]);

    let rendered = markdown(&current, Some(&baseline));
    assert!(rendered.contains("## Against the baseline"), "{rendered}");
    assert!(
        rendered.contains("| metric | baseline | now |"),
        "{rendered}"
    );
    assert!(rendered.contains("**PASS**"), "{rendered}");
}

#[test]
fn what_it_missed_names_every_expectation_by_id() {
    let scored = card(vec![case_score("ts-0001", 1, &["E2", "E3"], 0)]);
    let rendered = markdown(&scored, None);

    // A report that says "recall 33%" and not *which* defects were missed is a
    // number nobody can act on.
    assert!(rendered.contains("missed `E2`"), "{rendered}");
    assert!(rendered.contains("missed `E3`"), "{rendered}");
}
