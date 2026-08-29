//! Deciding when two reviewers found the same thing.
//!
//! Always compiled. Pure, offline, and deliberately separate from
//! [`crate::findings::anchor`]: the two answer different questions and
//! collapsing them would break cross-push dedupe.
//!
//! # Why not just compare fingerprints
//!
//! `Finding::fingerprint` hashes the lane, the path, the **rule** and the
//! anchored code. `rule` is model-authored free text, and two reviewers looking
//! at the same missing bounds check will write `unchecked-index` and
//! `missing-bounds-check`. Grouping on the fingerprint would call those two
//! separate findings and post both, which is the noise the council is supposed
//! to reduce rather than create.
//!
//! So corroboration uses a looser rule — same file, overlapping anchored lines.
//!
//! This module used to argue that the strict rule was nonetheless right for
//! *cross-push* dedupe, because a rule id is stable for one class of problem
//! across runs. It is not. On `tinyhumansai/backend#1295` one line collected
//! four comments with the same title and the same suggested patch, filed as
//! `discarded-error-handling`, `unhandled-error`, `discarded-error` and
//! `swallowed-error`. One model, one defect, four identities, four comments.
//! A rule id is as much free text on the second run as it is on the second
//! reviewer, and the objection above always applied to both.
//!
//! [`crate::findings::prior::PriorReview::covers`] is the same looseness
//! applied across pushes, sharing [`LINE_TOLERANCE`] with this module. It is
//! deliberately *not* the same function: this one requires the same lane and
//! groups two live findings, that one ignores the lane and compares a live
//! finding against a comment already on GitHub.

use crate::findings::types::Finding;

/// How far apart two anchors may sit and still be one observation.
///
/// Matches `crate::eval::score`'s tolerance, and for the same reason: a
/// reviewer anchors to the guard, the call, or the line under it depending on
/// what it quoted, so a few lines of slack is the same defect described from a
/// different angle.
pub const LINE_TOLERANCE: u64 = 3;

/// Whether `a` and `b` are the same observation by two reviewers.
///
/// Never compares titles or bodies. Two agents describing one defect will word
/// it differently by construction — that is the entire reason for running more
/// than one — so wording is evidence of nothing here.
pub fn corroborates(a: &Finding, b: &Finding) -> bool {
    if a.lane != b.lane || a.path != b.path {
        return false;
    }

    // An identical fingerprint is conclusive when both have one: same lane,
    // path, rule and anchored code is the same finding by the strictest rule
    // the crate has.
    if let (Some(left), Some(right)) = (&a.identity, &b.identity)
        && left == right
    {
        return true;
    }

    match (a.range(), b.range()) {
        (Some((a_start, a_end)), Some((b_start, b_end))) => {
            let low = b_start.saturating_sub(LINE_TOLERANCE);
            let high = b_end.saturating_add(LINE_TOLERANCE);
            a_start <= high && a_end >= low
        }
        // Neither could be placed. Both were demoted to the check-run summary
        // for the same file, and posting two unplaceable findings about one
        // file is the worst version of this noise: a reader cannot even tell
        // them apart by line.
        (None, None) => true,
        // One was placed and the other was not. They may well be the same
        // defect, but there is no evidence of it, and merging on no evidence
        // would silently delete a finding.
        _ => false,
    }
}

#[cfg(test)]
#[path = "agree_test.rs"]
mod tests;
