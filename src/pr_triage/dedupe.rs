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

use std::collections::{BTreeMap, BTreeSet};

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
    /// The branch it targets.
    ///
    /// Compared before anything else. A backport and the change it backports
    /// are the same diff and are *not* duplicates: one still has to land on the
    /// release branch. Closing the newer of the pair loses the backport.
    pub base_ref: String,
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
    /// A **multiset**, not a set: the count matters. An older pull request that
    /// changes one occurrence of a repeated block and a newer one that changes
    /// two identical occurrences collapse to the same single fingerprint under
    /// a set, score 1.0, and the newer one is closed despite its extra edit.
    ///
    /// Counted rather than ordered, because two contributors solving the same
    /// problem order their hunks differently often enough that sequence
    /// comparison would miss real duplicates.
    pub edits: BTreeMap<String, usize>,
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
    pub fn of(number: u64, base_ref: &str, files: &[ChangedFile]) -> Self {
        let mut paths = BTreeSet::new();
        let mut edits: BTreeMap<String, usize> = BTreeMap::new();
        let mut every_file_readable = true;

        for file in files {
            paths.insert(file.path.clone());
            let Some(patch) = file.patch.as_deref() else {
                every_file_readable = false;
                continue;
            };
            for hunk in hunks(patch) {
                *edits.entry(fingerprint(file, &hunk)).or_default() += 1;
            }
        }

        Shape {
            number,
            base_ref: base_ref.to_string(),
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

/// One hunk's identity: which file, what kind of change, where in it, what it
/// replaced, and with what.
///
/// Unit and record separators, like the index's document ids, so a path or a
/// line containing the delimiter cannot forge another hunk's fingerprint.
fn fingerprint(file: &ChangedFile, hunk: &Hunk) -> String {
    format!(
        "{}\u{1f}{:?}\u{1f}{}\u{1f}{}\u{1e}{}\u{1e}{}",
        file.path,
        // The *operation*, not only the text. Emptying a file and deleting it
        // can produce the same patch body and are materially different results,
        // and a rename's source is part of what it does.
        file.status,
        file.previous_path.clone().unwrap_or_default(),
        // And where in the file. Two distant blocks with identical surrounding
        // context are told apart by nothing else, so two pull requests editing
        // one different block each would otherwise fingerprint the same.
        hunk.start,
        hunk.before.join("\u{1f}"),
        hunk.after.join("\u{1f}")
    )
}

/// How much two shapes overlap.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Overlap {
    /// Jaccard overlap of the changed-path sets, 0..=1.
    pub paths: f64,
    /// Multiset Jaccard overlap of the hunk fingerprints, 0..=1.
    ///
    /// Named for what it measures. It was `lines` for a while and that invited
    /// exactly the misreading the fingerprint exists to prevent — a hunk
    /// overlap is not a line overlap, and two identical edits in different
    /// places score 0 here where a line comparison would score 1.
    pub edits: f64,
}

/// Score one pair.
pub fn overlap(left: &Shape, right: &Shape) -> Overlap {
    Overlap {
        paths: jaccard(&left.paths, &right.paths),
        edits: multiset_jaccard(&left.edits, &right.edits),
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

/// Overlap of two multisets, 0..=1: summed minimums over summed maximums.
///
/// The generalisation of Jaccard that keeps counts, so one occurrence of an
/// edit against two of the same edit scores 0.5 rather than 1.0.
fn multiset_jaccard(left: &BTreeMap<String, usize>, right: &BTreeMap<String, usize>) -> f64 {
    let mut intersection = 0usize;
    let mut union = 0usize;

    for key in left.keys().chain(right.keys()).collect::<BTreeSet<_>>() {
        let mine = left.get(key).copied().unwrap_or(0);
        let theirs = right.get(key).copied().unwrap_or(0);
        intersection += mine.min(theirs);
        union += mine.max(theirs);
    }

    if union == 0 {
        return 0.0;
    }
    intersection as f64 / union as f64
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
        // Same target branch only. Two identical diffs aimed at `main` and at
        // `release/2.1` are a change and its backport, and the backport still
        // has to land.
        if other.base_ref != subject.base_ref {
            continue;
        }
        let score = overlap(subject, other);
        if score.paths < path_min || score.edits < line_min {
            continue;
        }
        let better = match best {
            None => true,
            Some((number, current)) => {
                score.edits > current.edits
                    || (score.edits == current.edits && other.number < number)
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
