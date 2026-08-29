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
use crate::pr_triage::landed::{Hunk, hunks};

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
    /// One fingerprint per hunk: its path, its before image and its after
    /// image, context included.
    ///
    /// A **hunk** rather than a line, and that is the whole of the precision
    /// here. A set of changed lines cannot tell two edits apart when they read
    /// the same: flipping `enabled = false` to `true` in two unrelated
    /// configuration blocks produces identical added and removed lines, and a
    /// line-set comparison scores it a confident 100%. The hunk's context lines
    /// are what say *where*, so two identical edits in two different places
    /// fingerprint differently.
    ///
    /// A set rather than a sequence, because two contributors solving the same
    /// problem order their hunks differently often enough that sequence
    /// comparison would miss real duplicates.
    pub edits: BTreeSet<String>,
    /// Whether every changed file came with a readable patch.
    ///
    /// A binary or truncated file contributes its *path* and no content. Two
    /// pull requests that make the same one-line edit and also touch the same
    /// image would otherwise score 100% on both axes while carrying completely
    /// different bytes, and the newer one would be closed for it.
    pub every_file_readable: bool,
}

impl Shape {
    /// Reduce one pull request's changed files to its comparable shape.
    pub fn of(number: u64, files: &[ChangedFile]) -> Self {
        let mut paths = BTreeSet::new();
        let mut edits = BTreeSet::new();
        let mut every_file_readable = true;

        for file in files {
            paths.insert(file.path.clone());
            let Some(patch) = file.patch.as_deref() else {
                every_file_readable = false;
                continue;
            };
            for hunk in hunks(patch) {
                edits.insert(fingerprint(&file.path, &hunk));
            }
        }

        Shape {
            number,
            paths,
            edits,
            every_file_readable,
        }
    }

    /// Whether there is anything here to compare.
    ///
    /// A pull request with no readable hunks — all binary, or a diff the forge
    /// truncated — cannot be shown to duplicate anything, and a comparison of
    /// two empty sets would score a confident 1.0.
    pub fn is_comparable(&self) -> bool {
        !self.paths.is_empty() && !self.edits.is_empty() && self.every_file_readable
    }
}

/// One hunk's identity: where it is, what it replaced, and with what.
///
/// Unit and record separators, like the index's document ids, so a path or a
/// line containing the delimiter cannot forge another hunk's fingerprint.
fn fingerprint(path: &str, hunk: &Hunk) -> String {
    format!(
        "{path}\u{1f}{}\u{1e}{}",
        hunk.before.join("\u{1f}"),
        hunk.after.join("\u{1f}")
    )
}

/// How much two shapes overlap.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Overlap {
    /// Jaccard overlap of the changed-path sets, 0..=1.
    pub paths: f64,
    /// Jaccard overlap of the hunk fingerprints, 0..=1.
    pub lines: f64,
}

/// Score one pair.
pub fn overlap(left: &Shape, right: &Shape) -> Overlap {
    Overlap {
        paths: jaccard(&left.paths, &right.paths),
        lines: jaccard(&left.edits, &right.edits),
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
