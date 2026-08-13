//! Turning several cheap opinions into one lane's findings.
//!
//! ## Why this is two stages and not a vote
//!
//! The obvious design — run N panellists, keep what most of them said — is
//! wrong for a *specialised* panel, and wrong in the direction that costs the
//! most. A panellist reading for missing test coverage is not competent to
//! notice a widened trust boundary, so requiring it to agree before a security
//! finding survives discards exactly the findings specialisation exists to
//! produce. Majority voting only means anything when the voters were asked the
//! same question.
//!
//! So the panel is split in two, and only the second half votes:
//!
//! 1. **Propose.** Each lens reads the same evidence with a different emphasis
//!    and reports what it saw. Their output is **unioned**, not voted on —
//!    deduplicated so two lenses noticing one problem report it once.
//! 2. **Verify.** Every surviving proposal is put to several independent
//!    verifiers, each asked one narrow question: *given this diff, is this
//!    real?* They were all asked the same thing, so a majority among them
//!    means something.
//!
//! That is also where the economics land. The expensive tier bought noise
//! control by being a better reader; a verify round buys it by making a claim
//! survive scrutiny, and several `flash` calls cost less than the one `deep`
//! call they replace (see `config/defaults.toml`).
//!
//! ## Deduplication keys on the anchor, never the wording
//!
//! Two models describing one problem write two different sentences. Keying on
//! the title would treat those as separate findings and report both, which is
//! precisely the double-reporting the lane partition in `lanes` exists to
//! prevent. The key is the *place and category* — path, rule, and the code
//! quoted — because that is what two reports of one problem actually share.

use std::collections::BTreeMap;

use crate::harness::schema::{LaneResponse, RawFinding};

/// One panellist's answer.
#[derive(Debug, Clone)]
pub struct Opinion {
    /// Which lens produced it, for attribution in the summary.
    pub lens: String,
    /// The model that actually answered. A fallback answering is worth knowing
    /// about by the time findings are merged, and is otherwise invisible.
    pub model: String,
    /// What it reported.
    pub response: LaneResponse,
}

/// A candidate finding, with the lenses that proposed it.
#[derive(Debug, Clone)]
pub struct Proposal {
    /// The finding itself, taken from the most confident proposer.
    pub finding: RawFinding,
    /// Every lens that reported this, in first-seen order. More than one is a
    /// meaningful signal and is surfaced in the body.
    pub lenses: Vec<String>,
}

/// One verifier's answer about one proposal.
#[derive(Debug, Clone, Copy)]
pub struct Verdict {
    /// Whether the verifier judged the finding real.
    pub real: bool,
}

/// The identity two reports of one problem share.
///
/// The quoted code is whitespace-normalised because models re-indent what they
/// quote, and an indentation difference is not a different finding. It is
/// truncated because a verbose quote of the same hunk should still collide;
/// what distinguishes two findings in one file under one rule is *which* code,
/// and the opening of the quote carries that.
fn identity(finding: &RawFinding) -> (String, String, String) {
    let anchor = finding
        .existing_code
        .as_deref()
        .unwrap_or_default()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(120)
        .collect::<String>();

    (
        finding.path.trim().to_string(),
        finding.rule.trim().to_lowercase(),
        anchor,
    )
}

/// Union the panel's proposals, deduplicated by [`identity`].
///
/// Order is the panel's: proposals come out in the order they were first seen,
/// so a run over the same evidence produces the same sequence and the golden
/// tests can assert on it.
pub fn propose(opinions: &[Opinion]) -> Vec<Proposal> {
    // Insertion order is carried separately because `BTreeMap` orders by key,
    // and the key is an identity tuple whose sort order means nothing to a
    // reader of the resulting comment.
    let mut order: Vec<(String, String, String)> = Vec::new();
    let mut merged: BTreeMap<(String, String, String), Proposal> = BTreeMap::new();

    for opinion in opinions {
        for finding in &opinion.response.findings {
            let key = identity(finding);

            match merged.get_mut(&key) {
                Some(existing) => {
                    if !existing.lenses.contains(&opinion.lens) {
                        existing.lenses.push(opinion.lens.clone());
                    }
                    // Keep the better-argued version of the same finding. A
                    // panellist that merely *also* noticed something should not
                    // overwrite the one that explained it.
                    if finding.confidence > existing.finding.confidence {
                        let lenses = std::mem::take(&mut existing.lenses);
                        *existing = Proposal {
                            finding: finding.clone(),
                            lenses,
                        };
                    }
                }
                None => {
                    order.push(key.clone());
                    merged.insert(
                        key,
                        Proposal {
                            finding: finding.clone(),
                            lenses: vec![opinion.lens.clone()],
                        },
                    );
                }
            }
        }
    }

    order
        .into_iter()
        .filter_map(|key| merged.remove(&key))
        .collect()
}

/// Keep the proposals a majority of their verifiers judged real.
///
/// `verdicts` is parallel to `proposals`: one inner vector of verdicts per
/// proposal. A proposal nobody verified is **dropped**, not kept — an unverified
/// claim is exactly the thing this stage exists to stop, and defaulting to
/// "keep" would make a verifier outage silently restore the old noise level.
pub fn settle(proposals: Vec<Proposal>, verdicts: &[Vec<Verdict>]) -> Vec<RawFinding> {
    proposals
        .into_iter()
        .zip(verdicts)
        .filter(|(_, votes)| {
            let real = votes.iter().filter(|v| v.real).count();
            !votes.is_empty() && real * 2 > votes.len()
        })
        .map(|(proposal, _)| proposal.finding)
        .collect()
}

/// Everything the panel agreed had already been fixed.
///
/// Unioned rather than voted on, and deliberately: `resolved` retires a finding
/// from a previous cycle, so the failure modes are asymmetric. Retiring one
/// early costs a re-report next cycle if it was wrong; leaving a fixed finding
/// standing means arguing with a contributor who already did the work.
pub fn resolved(opinions: &[Opinion]) -> Vec<String> {
    let mut seen: Vec<String> = Vec::new();

    for opinion in opinions {
        for title in &opinion.response.resolved {
            if !seen.iter().any(|t| t.eq_ignore_ascii_case(title)) {
                seen.push(title.clone());
            }
        }
    }

    seen
}

#[cfg(test)]
mod tests;
