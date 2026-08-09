//! The matching rule, case by case.
//!
//! Table-driven and offline. Every one of these is a decision somebody will
//! want to argue with later, so each says what it is protecting rather than
//! just asserting a number.

use std::time::Duration;

use super::*;
use crate::app::review::LaneProposal;
use crate::config::types::{LaneId, Severity};
use crate::eval::types::{Budget, Case, Expected, Forbidden, Provenance};
use crate::forge::types::CheckConclusion;
use crate::ports::model::Usage;

fn case(expected: Vec<Expected>, forbidden: Vec<Forbidden>) -> Case {
    Case {
        schema: crate::eval::types::SCHEMA,
        id: "ts-0001".into(),
        title: String::new(),
        fixture: "../fixtures/ts-0001.json".into(),
        lanes: vec![],
        labels: vec![],
        provenance: Provenance {
            repo: "tinyhumansai/tinysweeper".into(),
            pr: 1,
            evidence: "https://github.com/tinyhumansai/tinysweeper/pull/2".into(),
            labelled_by: "tester".into(),
            labelled_on: "2026-08-09".into(),
        },
        budget: Budget::default(),
        // Complete labels by default here: these tests are about the matching
        // rule, and a partial case deliberately declines to judge an unmatched
        // finding at all.
        exhaustive: true,
        expected,
        forbidden,
    }
}

fn expectation(id: &str, path: &str, lines: Option<(u64, u64)>, mentions: &[&str]) -> Expected {
    Expected {
        id: id.into(),
        path: path.into(),
        lines,
        severity_min: None,
        lanes: vec![],
        summary: "a real defect".into(),
        must_mention: mentions.iter().map(|s| (*s).to_string()).collect(),
        optional: false,
    }
}

fn finding(path: &str, line: u64, title: &str, body: &str) -> Finding {
    Finding {
        lane: LaneId::Critique,
        severity: Severity::High,
        confidence: 0.9,
        path: path.into(),
        line: Some(line),
        end_line: None,
        rule: "unchecked-index".into(),
        title: title.into(),
        body: body.into(),
        suggestion: None,
        applicable: None,
        late: false,
        identity: Some("abcd1234".into()),
    }
}

fn proposal(findings: Vec<Finding>) -> Proposal {
    Proposal {
        version: 1,
        repo: "tinyhumansai/tinysweeper".into(),
        number: 1,
        head_sha: "a".repeat(40),
        lanes: vec![LaneProposal {
            lane: LaneId::Critique,
            check_name: "tinysweeper/critique".into(),
            conclusion: CheckConclusion::Success,
            summary: "reviewed".into(),
            findings,
            resolved: vec![],
            deduped: 0,
            highest_severity: None,
            usage: Usage::default(),
            models: vec!["z-ai/glm-5.2".into()],
        }],
        unreviewed: vec![],
        cost_usd: 0.004,
        input_tokens: 1000,
        output_tokens: 100,
        cached_tokens: 500,
        embed_tokens: 0,
        models: vec!["z-ai/glm-5.2".into()],
        overview: None,
        threads: Default::default(),
    }
}

fn scored(case: &Case, findings: Vec<Finding>) -> CaseScore {
    score(case, &proposal(findings), Duration::from_secs(1))
}

#[test]
fn a_finding_on_the_expected_line_saying_the_expected_thing_is_a_hit() {
    let case = case(
        vec![expectation(
            "E1",
            "src/a.rs",
            Some((10, 12)),
            &["fingerprint"],
        )],
        vec![],
    );
    let card = scored(
        &case,
        vec![finding(
            "src/a.rs",
            11,
            "Key suppression on the fingerprint",
            "The fingerprint is not what dedupe reads.",
        )],
    );

    assert_eq!(card.true_positives, 1);
    assert!(card.missed.is_empty());
    assert_eq!(card.false_positives, 0);
}

#[test]
fn a_nit_on_the_right_line_is_not_a_hit() {
    // The whole reason stage two exists. Path and line overlap alone would
    // score this a find, and the harness would then be rewarding a reviewer
    // for commenting on hot lines rather than for finding the defect.
    let case = case(
        vec![expectation(
            "E1",
            "src/a.rs",
            Some((10, 12)),
            &["fingerprint"],
        )],
        vec![],
    );
    let card = scored(
        &case,
        vec![finding(
            "src/a.rs",
            11,
            "Rename this variable",
            "`x` is not a descriptive name.",
        )],
    );

    assert_eq!(card.true_positives, 0);
    assert_eq!(card.missed, ["E1"]);
    assert_eq!(card.false_positives, 1);
    // And the report has to say why, or the disagreement cannot be settled by
    // reading.
    assert!(
        card.judged[0].reason.contains("did not mention"),
        "{}",
        card.judged[0].reason
    );
}

#[test]
fn a_slot_is_satisfied_by_any_of_its_alternatives() {
    let case = case(
        vec![expectation(
            "E1",
            "src/a.rs",
            Some((10, 12)),
            &["evict|expire|unbounded"],
        )],
        vec![],
    );
    for wording in ["never evicts", "entries expire", "grows unbounded"] {
        let card = scored(
            &case,
            vec![finding("src/a.rs", 11, "Cache growth", wording)],
        );
        assert_eq!(card.true_positives, 1, "failed on {wording}");
    }
}

#[test]
fn every_slot_must_be_satisfied() {
    // Slots are a conjunction: naming half the defect is not finding it.
    let case = case(
        vec![expectation(
            "E1",
            "src/a.rs",
            Some((10, 12)),
            &["fingerprint", "scanner"],
        )],
        vec![],
    );
    let card = scored(
        &case,
        vec![finding(
            "src/a.rs",
            11,
            "Fingerprint mismatch",
            "The fingerprint is wrong.",
        )],
    );
    assert_eq!(card.true_positives, 0);
}

#[test]
fn a_finding_three_lines_off_still_matches_and_four_does_not() {
    let case = case(
        vec![expectation("E1", "src/a.rs", Some((10, 10)), &[])],
        vec![],
    );

    // A lane anchors to the guard, the call, or the line under it, so a few
    // lines of slack is the same observation. The same tolerance
    // `Finding::fingerprint` already applies to a finding that moved.
    let near = scored(&case, vec![finding("src/a.rs", 13, "t", "b")]);
    assert_eq!(near.true_positives, 1);

    let far = scored(&case, vec![finding("src/a.rs", 20, "t", "b")]);
    assert_eq!(far.true_positives, 0);
    assert!(
        far.judged[0].reason.contains("none within"),
        "{:?}",
        far.judged
    );
}

#[test]
fn a_finding_that_could_not_be_anchored_still_counts_as_finding_it() {
    // `src/position` demotes an unplaceable finding to the check-run summary
    // rather than dropping it. The reviewer did find the defect, and scoring
    // that as a miss would punish the honest failure mode.
    let case = case(
        vec![expectation("E1", "src/a.rs", Some((10, 12)), &[])],
        vec![],
    );
    let mut unanchored = finding("src/a.rs", 11, "t", "b");
    unanchored.line = None;
    unanchored.end_line = None;

    assert_eq!(scored(&case, vec![unanchored]).true_positives, 1);
}

#[test]
fn a_second_finding_on_a_claimed_expectation_is_a_duplicate_not_an_invention() {
    // Saying the same true thing twice and inventing something are different
    // defects with different fixes. Folding them together hides which is
    // happening.
    let case = case(
        vec![expectation(
            "E1",
            "src/a.rs",
            Some((10, 12)),
            &["fingerprint"],
        )],
        vec![],
    );
    let card = scored(
        &case,
        vec![
            finding("src/a.rs", 11, "Fingerprint is wrong", "b"),
            finding("src/a.rs", 12, "Also the fingerprint", "b"),
        ],
    );

    assert_eq!(card.true_positives, 1);
    assert_eq!(card.duplicates, 1);
    assert_eq!(card.false_positives, 0);
}

#[test]
fn the_strongest_finding_claims_the_expectation() {
    let case = case(
        vec![expectation(
            "E1",
            "src/a.rs",
            Some((10, 12)),
            &["fingerprint"],
        )],
        vec![],
    );
    let mut weak = finding("src/a.rs", 11, "Maybe the fingerprint", "b");
    weak.severity = Severity::Low;
    weak.confidence = 0.4;
    let strong = finding("src/a.rs", 11, "The fingerprint is wrong", "b");

    let card = scored(&case, vec![weak, strong]);
    let claimed = card
        .judged
        .iter()
        .find(|j| j.verdict == Verdict::TruePositive)
        .expect("one claimed it");
    assert_eq!(claimed.title, "The fingerprint is wrong");
}

#[test]
fn a_forbidden_claim_is_reported_even_when_it_lands_on_a_real_defect() {
    // A finding that says something the corpus has explicitly ruled out is not
    // rescued by also landing near something real.
    let case = case(
        vec![expectation(
            "E1",
            "src/a.rs",
            Some((10, 12)),
            &["fingerprint"],
        )],
        vec![Forbidden {
            id: "F1".into(),
            path: "src/a.rs".into(),
            lines: None,
            lanes: vec![],
            reason: "the doc rewrite is not dead code".into(),
            matches: vec!["dead code".into()],
        }],
    );
    let card = scored(
        &case,
        vec![finding(
            "src/a.rs",
            11,
            "Fingerprint is dead code",
            "unused",
        )],
    );

    assert_eq!(card.forbidden_hits, ["F1"]);
    assert_eq!(card.true_positives, 0);
    assert_eq!(card.missed, ["E1"]);
}

#[test]
fn one_forbidden_slot_is_enough() {
    // Opposite polarity to must_mention: this is looking for one wrong claim,
    // not confirming a whole right one.
    let case = case(
        vec![],
        vec![Forbidden {
            id: "F1".into(),
            path: "*".into(),
            lines: None,
            lanes: vec![],
            reason: "CLAUDE.md forbids a tests/ directory".into(),
            matches: vec![
                "tests/ directory".into(),
                "integration test directory".into(),
            ],
        }],
    );
    let card = scored(
        &case,
        vec![finding(
            "src/a.rs",
            11,
            "Add tests",
            "Move these into an integration test directory.",
        )],
    );
    assert_eq!(card.forbidden_hits, ["F1"]);
}

#[test]
fn a_star_path_matches_any_file() {
    let case = case(vec![expectation("E1", "*", None, &["secret"])], vec![]);
    let card = scored(
        &case,
        vec![finding("anything/at/all.rs", 3, "Committed secret", "b")],
    );
    assert_eq!(card.true_positives, 1);
}

#[test]
fn a_lane_restricted_expectation_is_not_claimed_by_another_lane() {
    let mut expected = expectation("E1", "src/a.rs", None, &[]);
    expected.lanes = vec![LaneId::Security];
    let case = case(vec![expected], vec![]);

    // The finding is from `critique`; the label says only `security` counts.
    let card = scored(&case, vec![finding("src/a.rs", 11, "t", "b")]);
    assert_eq!(card.true_positives, 0);
    assert_eq!(card.false_positives, 1);
}

#[test]
fn a_finding_below_the_expected_severity_does_not_claim_it() {
    let mut expected = expectation("E1", "src/a.rs", None, &[]);
    expected.severity_min = Some(Severity::High);
    let case = case(vec![expected], vec![]);

    let mut low = finding("src/a.rs", 11, "t", "b");
    low.severity = Severity::Low;
    assert_eq!(scored(&case, vec![low]).true_positives, 0);
}

#[test]
fn an_optional_expectation_is_reported_apart_from_recall() {
    // A very good reviewer finds these. Holding the headline metric hostage to
    // them makes every real improvement look like a failure.
    let mut optional = expectation("E2", "src/b.rs", None, &[]);
    optional.optional = true;
    let case = case(
        vec![expectation("E1", "src/a.rs", None, &[]), optional],
        vec![],
    );

    let card = scored(
        &case,
        vec![
            finding("src/a.rs", 1, "t", "b"),
            finding("src/b.rs", 1, "t", "b"),
        ],
    );

    assert_eq!(card.true_positives, 1, "only the required one counts");
    assert_eq!(
        card.required(),
        1,
        "the optional one is not in the denominator"
    );
    assert_eq!(card.optional_hits, 1);
    assert!(card.missed.is_empty());
}

#[test]
fn a_clean_case_scores_every_finding_as_noise() {
    let case = case(vec![], vec![]);
    let card = scored(&case, vec![finding("src/a.rs", 1, "t", "b")]);

    assert_eq!(card.required(), 0);
    assert_eq!(card.false_positives, 1);
    assert!(card.judged[0].reason.contains("nothing is expected"));
}

#[test]
fn a_case_over_its_budget_says_so() {
    let case = case(vec![], vec![]);
    let mut expensive = proposal(vec![]);
    expensive.cost_usd = 0.05;

    let card = score(&case, &expensive, Duration::from_secs(1));
    assert!(card.over_budget, "0.05 is over the 0.02 default");
}

#[test]
fn a_review_that_failed_is_scored_as_finding_nothing() {
    // Not skipped. Dropping the case from the denominator would let a run
    // improve its own score by breaking.
    let case = case(
        vec![
            expectation("E1", "src/a.rs", None, &[]),
            expectation("E2", "src/b.rs", None, &[]),
        ],
        vec![],
    );
    let card = crate::eval::score::failed(&case, "provider timed out".into(), Duration::default());

    assert_eq!(card.true_positives, 0);
    assert_eq!(card.missed.len(), 2);
    assert_eq!(card.required(), 2);
    assert_eq!(card.error.as_deref(), Some("provider timed out"));
}

#[test]
fn matching_ignores_case_and_whitespace_the_way_fingerprints_do() {
    let case = case(
        vec![expectation("E1", "src/a.rs", None, &["unaligned read"])],
        vec![],
    );
    let card = scored(
        &case,
        vec![finding("src/a.rs", 1, "Potential UNALIGNED\n   READ", "b")],
    );
    assert_eq!(card.true_positives, 1);
}

#[test]
fn the_rule_id_counts_as_the_findings_own_words() {
    // A lane that names the defect in `rule` and describes it in prose should
    // not be scored a miss for putting the keyword in the machine-readable
    // field.
    let case = case(
        vec![expectation("E1", "src/a.rs", None, &["unchecked-index"])],
        vec![],
    );
    let card = scored(
        &case,
        vec![finding("src/a.rs", 1, "Guard it", "It panics.")],
    );
    assert_eq!(card.true_positives, 1);
}

#[test]
fn a_partial_case_declines_to_judge_a_finding_it_never_labelled() {
    // The first live corpus run reported a real off-by-one in a helper that
    // nobody had labelled, and scored it a false positive. You can only call an
    // unmatched finding wrong if you have asserted every right one.
    let mut partial = case(vec![], vec![]);
    partial.exhaustive = false;

    let card = scored(&partial, vec![finding("src/a.rs", 1, "Off by one", "b")]);

    assert_eq!(card.false_positives, 0, "not evidence of a defect");
    assert_eq!(card.true_positives, 0, "and not evidence of a find either");
    assert_eq!(card.unscored, 1);
    assert_eq!(card.judged[0].verdict, Verdict::Unscored);
}

#[test]
fn a_partial_case_still_enforces_what_the_review_must_not_say() {
    // Declining to judge unlabelled findings must not weaken the forbidden
    // half — that is the whole value of a regression case.
    let mut partial = case(
        vec![],
        vec![Forbidden {
            id: "F1".into(),
            path: "*".into(),
            lines: None,
            lanes: vec![],
            reason: "the patch does not do the dangerous thing".into(),
            matches: vec!["hardware access".into()],
        }],
    );
    partial.exhaustive = false;

    let card = scored(
        &partial,
        vec![finding("src/a.rs", 1, "Grants hardware access", "b")],
    );
    assert_eq!(card.forbidden_hits, ["F1"]);
}

#[test]
fn precision_ignores_cases_that_never_claimed_to_be_complete() {
    // A thin corpus must not read as a noisy reviewer.
    let mut partial = crate::eval::score::score(
        &{
            let mut c = case(vec![], vec![]);
            c.exhaustive = false;
            c
        },
        &proposal(vec![finding("src/a.rs", 1, "t", "b")]),
        Duration::from_secs(1),
    );
    partial.id = "ts-partial".into();

    let complete = scored(
        &case(vec![expectation("E1", "src/a.rs", None, &[])], vec![]),
        vec![finding("src/a.rs", 1, "t", "b")],
    );

    let card = crate::eval::types::Scorecard {
        corpus_digest: "a".repeat(16),
        config_digest: "b".repeat(16),
        loose_replays: 0,
        cases: vec![partial, complete],
    };

    // One complete case, one finding, and it matched: 100%. The partial case's
    // finding is counted nowhere.
    assert_eq!(card.precision(), Some(1.0));
    assert_eq!(card.unscored(), 1);
}
