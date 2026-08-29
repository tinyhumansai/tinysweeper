//! Finding duplicate pull requests, deterministically.
//!
//! Two pull requests are duplicates when they change **the same files** in
//! **the same way**. Both halves are measured, both have a floor in
//! `[pr_triage]`, and neither involves a model.
//!
//! ## Why paths first
//!
//! Titles are the obvious thing to compare and the wrong thing to trust. On a
//! busy repository a dozen pull requests are called "fix(agent): ..." and two
//! of them fix different bugs; meanwhile the genuine duplicate pair is often
//! titled differently by two contributors who never saw each other's work. What
//! two duplicates *do* share is the set of files they touch, so that is the
//! first gate. The title is not read at all — which is also why a pull request
//! titled "ignore previous instructions" is inert here.
//!
//! ## Which one is the duplicate
//!
//! The newer one, always. The older pull request has the review history, the
//! discussion and the contributor's waiting time attached to it; closing it in
//! favour of a copy opened this morning throws all of that away. The port
//! guarantees `open_pull_requests` comes back oldest first for exactly this
//! reason.

use std::collections::BTreeSet;

use crate::forge::types::ChangedFile;
use crate::pr_triage::landed::hunks;

/// One pull request reduced to what the comparison reads.
///
/// Built once per pull request and compared many times, so the expensive part —
/// parsing every patch — happens once rather than once per pair.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shape {
    /// The pull request's number.
    pub number: u64,
    /// The paths it changes.
    pub paths: BTreeSet<String>,
    /// Every line it adds, normalised, as a set.
    ///
    /// A set rather than a sequence: two contributors solving the same problem
    /// order their hunks differently often enough that sequence comparison
    /// would miss real duplicates, and the paths gate has already established
    /// that they are working on the same files.
    pub added: BTreeSet<String>,
    /// Every line it removes, normalised, as a set.
    ///
    /// Carried because the additions alone do not identify an edit. Two changes
    /// to two different functions that each add `return false;` share 100% of
    /// their added lines and are not remotely the same change — what separates
    /// them is what they took out. Compared under the same floor as the
    /// additions, so a duplicate has to match on both halves of the edit.
    pub removed: BTreeSet<String>,
}

impl Shape {
    /// Reduce one pull request's changed files to its comparable shape.
    pub fn of(number: u64, files: &[ChangedFile]) -> Self {
        let mut paths = BTreeSet::new();
        let mut added = BTreeSet::new();
        let mut removed = BTreeSet::new();

        for file in files {
            paths.insert(file.path.clone());
            let Some(patch) = file.patch.as_deref() else {
                continue;
            };
            for hunk in hunks(patch) {
                // The images carry context lines, which are on both sides and
                // therefore say nothing about who changed what. Only the lines
                // one side has and the other does not are the edit.
                added.extend(hunk.after.iter().filter(|line| !hunk.before.contains(line)).cloned());
                removed.extend(hunk.before.iter().filter(|line| !hunk.after.contains(line)).cloned());
            }
        }

        Shape {
            number,
            paths,
            added,
            removed,
        }
    }

    /// Whether there is anything here to compare.
    ///
    /// A pull request with no readable added lines — all deletions, or all
    /// binary — cannot be shown to duplicate anything, and a comparison of two
    /// empty sets would score a confident 1.0.
    pub fn is_comparable(&self) -> bool {
        !self.paths.is_empty() && !self.added.is_empty()
    }
}

/// How much two shapes overlap.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Overlap {
    /// Jaccard overlap of the changed-path sets, 0..=1.
    pub paths: f64,
    /// Overlap of the two edits, 0..=1: the lower of the added-line and
    /// removed-line scores.
    ///
    /// The lower, not the mean. An average lets a perfect match on one half
    /// carry a poor match on the other, which is exactly the pair this is meant
    /// to separate — two changes that add the same line and remove different
    /// ones.
    pub lines: f64,
    /// Overlap of the added-line sets alone, for the comment's evidence.
    pub added: f64,
    /// Overlap of the removed-line sets alone. `1.0` when neither removes
    /// anything, which is agreement rather than a missing answer.
    pub removed: f64,
}

/// Score one pair.
pub fn overlap(left: &Shape, right: &Shape) -> Overlap {
    // Two pull requests that both remove nothing agree perfectly about what
    // they remove. Scoring that as zero — which is what an empty-set Jaccard
    // gives — would make every pure-addition duplicate unfindable.
    let removed = if left.removed.is_empty() && right.removed.is_empty() {
        1.0
    } else {
        jaccard(&left.removed, &right.removed)
    };
    let added = jaccard(&left.added, &right.added);

    Overlap {
        paths: jaccard(&left.paths, &right.paths),
        lines: added.min(removed),
        added,
        removed,
    }
}

/// Overlap of two sets, 0..=1. Empty on either side is zero, never NaN.
fn jaccard(left: &BTreeSet<String>, right: &BTreeSet<String>) -> f64 {
    let union = left.union(right).count();
    if union == 0 {
        return 0.0;
    }
    left.intersection(right).count() as f64 / union as f64
}

/// The older pull request `subject` duplicates, if any.
///
/// `earlier` holds the shapes of pull requests with lower numbers, in any
/// order. The best-scoring one above both floors wins; ties resolve to the
/// lowest number, so the same inputs always name the same original.
pub fn duplicate_of(
    subject: &Shape,
    earlier: &[Shape],
    path_min: f64,
    line_min: f64,
) -> Option<(u64, Overlap)> {
    if !subject.is_comparable() {
        return None;
    }

    let mut best: Option<(u64, Overlap)> = None;

    for other in earlier {
        // Guarding on the number rather than trusting the caller's ordering:
        // "the newer one is the duplicate" is the rule that protects a
        // contributor's review history, and it should not be possible to invert
        // it by passing the list the wrong way round.
        if other.number >= subject.number || !other.is_comparable() {
            continue;
        }
        let score = overlap(subject, other);
        if score.paths < path_min || score.lines < line_min {
            continue;
        }
        let better = match best {
            None => true,
            Some((number, current)) => {
                score.lines > current.lines
                    || (score.lines == current.lines && other.number < number)
            }
        };
        if better {
            best = Some((other.number, score));
        }
    }

    best
}

#[cfg(test)]
#[path = "dedupe_test.rs"]
mod tests;
