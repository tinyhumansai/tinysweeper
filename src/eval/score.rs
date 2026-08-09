//! Matching produced findings to labelled expectations.
//!
//! Deterministic, offline, and a pure function of a proposal plus a case — so
//! the matching rule can be argued with and rewritten without spending a cent.
//!
//! # Two stages, and why the second one exists
//!
//! Stage one is structural: same file, overlapping lines, right lane, and the
//! finding actually cleared the config's posting gate. Stage two is a keyword
//! check against the finding's own text.
//!
//! The second stage is the one that looks like overreach and is not. A lane
//! will happily leave a naming nit on the exact line that holds the real bug.
//! Scoring purely on path and line overlap counts that as a hit, and the
//! harness then **rewards commenting on hot lines** — which is precisely the
//! behaviour it was built to catch. Keywords are crude, but they are
//! deterministic, free, reviewable in a diff, and every decision they make is
//! written into the report with its reason, so a wrong one is arguable rather
//! than invisible.
//!
//! # What is scored is what would be posted
//!
//! Findings are taken from the `LaneProposal`s, which have already been through
//! `severity_gate`, `confidence_min`, dedupe and `max_comments`. Scoring the
//! model's raw output instead would measure a review nobody receives.

use std::collections::BTreeMap;
use std::time::Duration;

use crate::app::review::Proposal;
use crate::eval::types::{Case, CaseScore, Expected, Forbidden, Judged, Verdict};
use crate::findings::types::Finding;

/// How far off a finding may anchor and still be the same observation.
///
/// A lane anchors to the guard, the call, or the line under it, depending on
/// what it quoted. `Finding::fingerprint` already treats a finding that moved
/// down three lines as the same finding, so the scorer uses the same tolerance
/// rather than inventing a stricter one.
const LINE_TOLERANCE: u64 = 3;

/// Score one case's proposal against its labels.
pub fn score(case: &Case, proposal: &Proposal, wall: Duration) -> CaseScore {
    let findings = postable(proposal);

    let mut judged = Vec::with_capacity(findings.len());
    let mut claimed: BTreeMap<&str, usize> = BTreeMap::new();
    let mut optional_hits = 0usize;
    let mut false_positives = 0usize;
    let mut unscored = 0usize;
    let mut duplicates = 0usize;
    let mut forbidden_hits = Vec::new();

    // Ordered by what a reader sees first: the review's own ordering is
    // severity then confidence, so the strongest claim gets first refusal on an
    // expectation. A weaker finding on the same defect is then the duplicate,
    // which is the honest way round.
    let mut ordered: Vec<&Finding> = findings.clone();
    ordered.sort_by(|a, b| {
        b.severity
            .cmp(&a.severity)
            .then(
                b.confidence
                    .partial_cmp(&a.confidence)
                    .unwrap_or(std::cmp::Ordering::Equal),
            )
            .then(a.path.cmp(&b.path))
            .then(a.line.cmp(&b.line))
    });

    for finding in ordered {
        // Forbidden is checked first and wins. A finding that says something
        // the corpus has explicitly ruled out is not rescued by also landing
        // near a real defect.
        if let Some(forbidden) = case.forbidden.iter().find(|f| forbids(f, finding)) {
            forbidden_hits.push(forbidden.id.clone());
            judged.push(Judged {
                lane: finding.lane,
                path: finding.path.clone(),
                line: finding.line,
                title: finding.title.clone(),
                verdict: Verdict::Forbidden,
                matched: Some(forbidden.id.clone()),
                reason: format!("forbidden: {}", forbidden.reason),
            });
            continue;
        }

        // Prefer an expectation nothing has claimed yet. Two labels can both
        // match one finding — `lines: None` and no `must_mention` matches the
        // whole file, and `LINE_TOLERANCE` widens line-fenced expectations —
        // so taking the first match would report the second label as missed
        // while the finding that satisfies it sits in `judged` as a duplicate.
        let chosen = case
            .expected
            .iter()
            .find(|e| matches(e, finding) && !claimed.contains_key(e.id.as_str()))
            .or_else(|| case.expected.iter().find(|e| matches(e, finding)));
        match chosen {
            Some(expected) => {
                let count = claimed.entry(expected.id.as_str()).or_insert(0);
                *count += 1;
                if *count == 1 {
                    if expected.optional {
                        optional_hits += 1;
                    }
                    judged.push(Judged {
                        lane: finding.lane,
                        path: finding.path.clone(),
                        line: finding.line,
                        title: finding.title.clone(),
                        verdict: Verdict::TruePositive,
                        matched: Some(expected.id.clone()),
                        reason: "path, line and wording all matched".into(),
                    });
                } else {
                    duplicates += 1;
                    judged.push(Judged {
                        lane: finding.lane,
                        path: finding.path.clone(),
                        line: finding.line,
                        title: finding.title.clone(),
                        verdict: Verdict::Duplicate,
                        matched: Some(expected.id.clone()),
                        reason: format!("`{}` was already claimed", expected.id),
                    });
                }
            }
            None if case.exhaustive => {
                false_positives += 1;
                judged.push(Judged {
                    lane: finding.lane,
                    path: finding.path.clone(),
                    line: finding.line,
                    title: finding.title.clone(),
                    verdict: Verdict::FalsePositive,
                    matched: None,
                    reason: no_match_reason(case, finding),
                });
            }
            // The case never claimed to list every true finding, so this is not
            // evidence either way. Calling it a false positive would penalise
            // the reviewer for finding something real that nobody had got round
            // to labelling — which is exactly what happened the first time this
            // corpus ran.
            None => {
                unscored += 1;
                judged.push(Judged {
                    lane: finding.lane,
                    path: finding.path.clone(),
                    line: finding.line,
                    title: finding.title.clone(),
                    verdict: Verdict::Unscored,
                    matched: None,
                    reason: "the case does not claim to list every true finding".into(),
                });
            }
        }
    }

    let true_positives = case
        .expected
        .iter()
        .filter(|e| !e.optional && claimed.contains_key(e.id.as_str()))
        .count();
    let missed: Vec<String> = case
        .required()
        .filter(|e| !claimed.contains_key(e.id.as_str()))
        .map(|e| e.id.clone())
        .collect();

    // Re-sorted into reading order for the report, after the matching that
    // needed severity order is done.
    judged.sort_by(|a, b| a.path.cmp(&b.path).then(a.line.cmp(&b.line)));

    CaseScore {
        id: case.id.clone(),
        true_positives,
        missed,
        false_positives,
        unscored,
        exhaustive: case.exhaustive,
        duplicates,
        forbidden_hits,
        optional_hits,
        judged,
        cost_usd: proposal.cost_usd,
        over_budget: proposal.cost_usd > case.budget.max_cost_usd,
        input_tokens: proposal.input_tokens,
        output_tokens: proposal.output_tokens,
        cached_tokens: proposal.cached_tokens,
        wall_secs: wall.as_secs_f64(),
        models: proposal.models.clone(),
        lane_costs: proposal
            .lane_costs()
            .into_iter()
            .map(|(lane, usage, _)| (lane, usage.cost_usd))
            .collect(),
        error: None,
    }
}

/// A case whose review failed outright.
///
/// Scored as zero recall rather than skipped. A reviewer that crashes found
/// nothing, and dropping the case from the denominator would let a run improve
/// its own score by breaking.
pub fn failed(case: &Case, error: String, wall: Duration) -> CaseScore {
    CaseScore {
        id: case.id.clone(),
        true_positives: 0,
        missed: case.required().map(|e| e.id.clone()).collect(),
        false_positives: 0,
        unscored: 0,
        exhaustive: case.exhaustive,
        duplicates: 0,
        forbidden_hits: Vec::new(),
        optional_hits: 0,
        judged: Vec::new(),
        cost_usd: 0.0,
        over_budget: false,
        input_tokens: 0,
        output_tokens: 0,
        cached_tokens: 0,
        wall_secs: wall.as_secs_f64(),
        models: Vec::new(),
        lane_costs: BTreeMap::new(),
        error: Some(error),
    }
}

/// Every finding the proposal would actually post or summarise.
fn postable(proposal: &Proposal) -> Vec<&Finding> {
    proposal
        .lanes
        .iter()
        .flat_map(|lane| &lane.findings)
        .collect()
}

/// Whether `finding` is the defect `expected` describes.
fn matches(expected: &Expected, finding: &Finding) -> bool {
    if !path_matches(&expected.path, &finding.path) {
        return false;
    }
    if !expected.lanes.is_empty() && !expected.lanes.contains(&finding.lane) {
        return false;
    }
    if let Some(minimum) = expected.severity_min
        && finding.severity < minimum
    {
        return false;
    }
    if let Some((start, end)) = expected.lines
        && !overlaps(finding, start, end)
    {
        return false;
    }
    let haystack = haystack(finding);
    expected
        .must_mention
        .iter()
        .all(|slot| slot_matches(slot, &haystack))
}

/// Whether `finding` says the thing `forbidden` rules out.
fn forbids(forbidden: &Forbidden, finding: &Finding) -> bool {
    if !path_matches(&forbidden.path, &finding.path) {
        return false;
    }
    if !forbidden.lanes.is_empty() && !forbidden.lanes.contains(&finding.lane) {
        return false;
    }
    if let Some((start, end)) = forbidden.lines
        && !overlaps(finding, start, end)
    {
        return false;
    }
    // No keywords means the structural constraints above *are* the rule — "the
    // description lane must not anchor to this file" needs no vocabulary.
    if forbidden.matches.is_empty() {
        return true;
    }
    let haystack = haystack(finding);
    // Any slot, not all: this is looking for one specific wrong claim, where an
    // expectation is confirming a whole right one.
    forbidden
        .matches
        .iter()
        .any(|slot| slot_matches(slot, &haystack))
}

/// `*` matches any file; everything else is an exact path.
fn path_matches(expected: &str, actual: &str) -> bool {
    expected == "*" || expected == actual
}

/// Whether a finding's range comes within [`LINE_TOLERANCE`] of `start..=end`.
///
/// A finding with no line still matches: it was demoted to the check-run
/// summary rather than dropped, and the reviewer did find the defect. Requiring
/// an anchor would score `src/position`'s honest failure mode as a miss.
fn overlaps(finding: &Finding, start: u64, end: u64) -> bool {
    let Some((found_start, found_end)) = finding.range() else {
        return true;
    };
    let low = start.saturating_sub(LINE_TOLERANCE);
    let high = end.saturating_add(LINE_TOLERANCE);
    found_start <= high && found_end >= low
}

/// The finding's own words, normalised the way fingerprints are.
fn haystack(finding: &Finding) -> String {
    let joined = format!("{} {} {}", finding.title, finding.body, finding.rule);
    joined
        .to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Whether one `a|b|c` slot is satisfied.
fn slot_matches(slot: &str, haystack: &str) -> bool {
    slot.split('|')
        .map(str::trim)
        .filter(|alternative| !alternative.is_empty())
        .any(|alternative| haystack.contains(&alternative.to_lowercase()))
}

/// Why a finding matched nothing, in the terms the corpus is written in.
///
/// Written into the report so the common disagreement — "that *is* the bug, the
/// matcher is wrong" — can be settled by reading rather than by re-running.
fn no_match_reason(case: &Case, finding: &Finding) -> String {
    let same_path: Vec<&Expected> = case
        .expected
        .iter()
        .filter(|e| path_matches(&e.path, &finding.path))
        .collect();
    if same_path.is_empty() {
        return format!("nothing is expected in {}", finding.path);
    }
    let near: Vec<&&Expected> = same_path
        .iter()
        .filter(|e| e.lines.is_none_or(|(s, en)| overlaps(finding, s, en)))
        .collect();
    match near.as_slice() {
        [] => format!(
            "{} expectation(s) in this file, none within {LINE_TOLERANCE} lines",
            same_path.len()
        ),
        [expected] => format!(
            "landed on `{}` but did not mention {:?}",
            expected.id, expected.must_mention
        ),
        many => format!(
            "landed near {} expectations, matched none by wording",
            many.len()
        ),
    }
}

#[cfg(test)]
#[path = "score_test.rs"]
mod tests;
