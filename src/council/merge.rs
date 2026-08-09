//! Folding several reviewers' findings into one set.
//!
//! Always compiled, pure, and offline.
//!
//! # Agreement raises confidence. It never gates posting.
//!
//! The obvious design is a quorum: post a finding when two of three reviewers
//! raised it. It is the falsification failure wearing a different hat, and
//! `docs/modules/falsify/README.md` already explains why — a checker that saw
//! less than the reviewer rejects what it cannot confirm, and what it cannot
//! confirm is the good half.
//!
//! Here it is worse than that, because the architecture is *deliberately built*
//! so reviewers do not overlap. `harness::prompt::ISOLATION_CLAUSE` exists
//! precisely to stop N conversations noticing the same cross-file problem. A
//! reviewer given a different persona, a different slice, or — later — a tool
//! belt the others do not have, will find things the others structurally cannot
//! see. Gating on agreement deletes exactly those, which is to say exactly the
//! findings that justify running more than one reviewer at all.
//!
//! So the merge is **monotone**: no finding is ever worse off for the council
//! having run. Corroboration raises confidence and breaks ties in the comment
//! cap, and a singleton passes through on its own merit, judged by the same
//! `confidence_min` it would have faced alone.
//!
//! # The representative is verbatim
//!
//! When several reviewers describe one defect, one of their findings is chosen
//! whole — highest confidence, then highest severity. Nothing is blended,
//! rewritten or summarised. A merge step that can author text is a second
//! reviewer nobody gated, which is the same objection `src/falsify` raises to a
//! filter that can return findings of its own.

use crate::council::agree::corroborates;
use crate::findings::types::Finding;

/// The ceiling on merged confidence.
///
/// Noisy-OR climbs towards 1.0 fast, and a finding reported as *certain*
/// because three models happened to agree is a claim none of them made. The cap
/// keeps agreement a strong signal rather than an infallible one.
const CONFIDENCE_CEILING: f64 = 0.99;

/// Fold per-reviewer findings into one set.
///
/// `reviews` is one vector per reviewer, in configuration order. The result is
/// ordered by first appearance, so a single-reviewer council returns its input
/// untouched — which is what makes turning the council on with one agent a
/// provable no-op.
pub fn merge(reviews: Vec<Vec<Finding>>) -> Vec<Finding> {
    let mut merged: Vec<Finding> = Vec::new();

    for review in reviews {
        // Only what earlier reviewers contributed may absorb this reviewer's
        // findings, and each of those may absorb at most once per round.
        //
        // Both halves matter. Merging *within* one reviewer's output would make
        // the council change behaviour at a single agent — two findings three
        // lines apart from one reviewer would silently become one — and that
        // breaks the property this whole slice rests on. It is also not the
        // council's job: one reviewer reporting the same defect twice is a
        // dedupe question, and `lane_proposal` already owns it.
        //
        // This is not hypothetical. It shipped, and `eval` caught it on
        // `ts-0068` — where the critique lane legitimately raises two findings
        // on adjacent lines of one file.
        let settled = merged.len();
        let mut claimed = vec![false; settled];
        let mut additions = Vec::new();

        for finding in review {
            let hit = merged[..settled]
                .iter()
                .enumerate()
                .position(|(index, kept)| !claimed[index] && corroborates(kept, &finding));
            match hit {
                Some(index) => {
                    claimed[index] = true;
                    absorb(&mut merged[index], finding);
                }
                None => additions.push(finding),
            }
        }

        merged.extend(additions);
    }

    merged
}

/// Fold `other` into `kept`, keeping whichever is the better statement of it.
fn absorb(kept: &mut Finding, other: Finding) {
    // Noisy-OR: independent observers each with their own chance of being
    // wrong. Monotone by construction — the result is never below either input
    // — which is the property that makes the merge safe to leave on.
    let combined = 1.0 - (1.0 - kept.confidence) * (1.0 - other.confidence);
    let corroboration = kept.corroboration.saturating_add(other.corroboration);
    // The highest anyone assigned, read before either side is moved. Merging
    // must not talk a review *down*: if one reviewer thought this was critical,
    // that opinion survives being outvoted on wording.
    let severity = kept.severity.max(other.severity);

    // The stronger statement wins the body, and it is taken whole. Confidence
    // first, then severity: a reviewer that is sure of a medium-severity defect
    // has described it better than one guessing at a high-severity one.
    if (other.confidence, other.severity) > (kept.confidence, kept.severity) {
        // `identity` is what the posted marker carries and what suppression
        // reads back, so it belongs to the finding as first seen. Letting the
        // representative bring its own would repost a finding that had already
        // been answered.
        let identity = kept.identity.clone();
        *kept = other;
        kept.identity = identity;
    }

    kept.confidence = combined.min(CONFIDENCE_CEILING);
    kept.corroboration = corroboration;
    kept.severity = severity;
}

#[cfg(test)]
#[path = "merge_test.rs"]
mod tests;
