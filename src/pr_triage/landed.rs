//! "Already implemented": deciding whether a pull request's change is already
//! on the base branch.
//!
//! The question is asked in a form that has a checkable answer:
//!
//! > Would applying this pull request change anything?
//!
//! If every run of lines it adds is already there, and every run of lines it
//! removes is already gone, then the answer is no and the pull request is a
//! no-op against the branch it targets. That is a fact a maintainer can verify
//! with `git grep`, not a judgement — which is why this path calls no model and
//! why its verdict is allowed to close a pull request.
//!
//! ## Why runs, and not lines
//!
//! Matching added lines individually would be far too generous. A pull request
//! adding a single `}` would find one on the base branch and declare itself
//! landed. So the unit of comparison is a **run**: a maximal stretch of
//! consecutive added lines in a hunk, which must appear as a consecutive
//! stretch on the base branch. Ten lines in the same order in the same file is
//! not a coincidence.
//!
//! The removed runs carry the other half of the argument, and it is the half
//! that makes single-line changes safe. A pull request changing `1.93.0` to
//! `1.96.1` adds one line and removes one. If the change already landed, the
//! base has the new line and not the old one. If it did not, the base still has
//! the old line — the removed run is *present* — and this module refuses.
//!
//! ## What it refuses to judge
//!
//! Renames, files whose patch the forge would not give us, and changes smaller
//! than `pr_triage.min_landed_lines`. Each is a case where the diff does not
//! contain enough to answer the question, and answering it anyway would mean
//! closing somebody's pull request on a guess.

use crate::forge::types::{ChangedFile, FileStatus};

/// Why a pull request could not be called superseded.
///
/// Carried as a reason rather than a bare `false` because "we could not tell"
/// and "we checked, and it has not landed" are different answers, and only the
/// second one is worth putting in a comment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotLanded {
    /// The pull request changes nothing this module can read.
    NoReadableDiff,
    /// A file was renamed, so "the same path on the base branch" is not a
    /// question with one answer.
    Renamed,
    /// The forge gave no patch for a file — a binary, or a truncated diff.
    OpaqueFile,
    /// Too few substantive lines for a match to be evidence.
    TooSmall,
    /// A line it adds is not on the base branch.
    AddedLineMissing,
    /// A line it removes is still on the base branch.
    RemovedLineStillPresent,
}

impl NotLanded {
    /// One phrase, for the log and the triage comment.
    pub fn reason(self) -> &'static str {
        match self {
            NotLanded::NoReadableDiff => "the diff had nothing readable in it",
            NotLanded::Renamed => "it renames a file, so the base-branch path is ambiguous",
            NotLanded::OpaqueFile => "the forge gave no patch for one of its files",
            NotLanded::TooSmall => "the change is too small for a match to mean anything",
            NotLanded::AddedLineMissing => "it adds lines the base branch does not have",
            NotLanded::RemovedLineStillPresent => {
                "it removes lines the base branch still has"
            }
        }
    }
}

/// One file's diff, reduced to the runs that decide the question.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Runs {
    /// Maximal stretches of consecutive added lines, normalised.
    pub added: Vec<Vec<String>>,
    /// Maximal stretches of consecutive removed lines, normalised.
    pub removed: Vec<Vec<String>>,
}

impl Runs {
    /// How many substantive lines this file contributes.
    pub fn line_count(&self) -> usize {
        self.added.iter().chain(&self.removed).map(Vec::len).sum()
    }
}

/// Split a unified diff into its added and removed runs.
///
/// Blank and whitespace-only lines are dropped rather than kept, and dropping
/// them *breaks* a run rather than joining across it — otherwise two unrelated
/// stretches separated by a blank line would be compared as one block that
/// exists nowhere, and every reformatted file would read as "not landed".
///
/// `\ No newline at end of file` is diff chrome, not content, and is skipped
/// without breaking the run it sits inside.
pub fn runs(patch: &str) -> Runs {
    let mut out = Runs::default();
    let mut adding: Vec<String> = Vec::new();
    let mut removing: Vec<String> = Vec::new();

    let flush = |run: &mut Vec<String>, into: &mut Vec<Vec<String>>| {
        if !run.is_empty() {
            into.push(std::mem::take(run));
        }
    };

    for line in patch.lines() {
        if line.starts_with("\\") {
            continue;
        }
        match line.as_bytes().first() {
            Some(b'+') if !line.starts_with("+++") => {
                flush(&mut removing, &mut out.removed);
                push_normalised(&line[1..], &mut adding, &mut out.added);
            }
            Some(b'-') if !line.starts_with("---") => {
                flush(&mut adding, &mut out.added);
                push_normalised(&line[1..], &mut removing, &mut out.removed);
            }
            _ => {
                flush(&mut adding, &mut out.added);
                flush(&mut removing, &mut out.removed);
            }
        }
    }

    flush(&mut adding, &mut out.added);
    flush(&mut removing, &mut out.removed);
    out
}

/// Add one diff line to the run being built, or end the run if it is blank.
fn push_normalised(text: &str, run: &mut Vec<String>, into: &mut Vec<Vec<String>>) {
    let normalised = normalise(text);
    if normalised.is_empty() {
        if !run.is_empty() {
            into.push(std::mem::take(run));
        }
        return;
    }
    run.push(normalised);
}

/// The comparable form of a line.
///
/// Leading and trailing whitespace is dropped, and interior runs of whitespace
/// are collapsed to one space. Re-indenting a block is the single most common
/// reason the same code looks different in two places, and treating that as a
/// different change would make this module useless on exactly the pull requests
/// it is for.
pub fn normalise(line: &str) -> String {
    line.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The base-branch text of a file, in comparable form.
///
/// A file that does not exist on the base branch is an empty list, not an
/// error: a pull request deleting a file somebody already deleted removes lines
/// that are already gone, which is precisely a superseded pull request.
pub fn base_lines(content: Option<&str>) -> Vec<String> {
    content
        .unwrap_or_default()
        .lines()
        .map(normalise)
        .filter(|line| !line.is_empty())
        .collect()
}

/// Whether `run` appears as consecutive lines of `haystack`.
fn contains_run(haystack: &[String], run: &[String]) -> bool {
    if run.is_empty() {
        return true;
    }
    if run.len() > haystack.len() {
        return false;
    }
    haystack
        .windows(run.len())
        .any(|window| window == run)
}

/// Whether one file's change is already on the base branch.
///
/// `base` is that file's content on the base branch, `None` when it is not
/// there at all.
pub fn file_landed(file: &ChangedFile, base: Option<&str>) -> Result<Runs, NotLanded> {
    if file.previous_path.is_some() || file.status == FileStatus::Renamed {
        return Err(NotLanded::Renamed);
    }
    let Some(patch) = file.patch.as_deref() else {
        return Err(NotLanded::OpaqueFile);
    };

    let runs = runs(patch);
    let base = base_lines(base);

    for run in &runs.added {
        if !contains_run(&base, run) {
            return Err(NotLanded::AddedLineMissing);
        }
    }
    for run in &runs.removed {
        if contains_run(&base, run) {
            return Err(NotLanded::RemovedLineStillPresent);
        }
    }

    Ok(runs)
}

/// Whether a whole pull request's change is already on the base branch.
///
/// `bases` supplies each changed file's base-branch content in the same order
/// as `files`; a caller that could not read one passes `None`, which is the
/// same as the file not existing there.
///
/// Returns the number of substantive lines checked, which the verdict carries
/// so a maintainer reading the comment knows how much evidence is behind it.
pub fn landed(
    files: &[ChangedFile],
    bases: &[Option<String>],
    min_lines: usize,
) -> Result<usize, NotLanded> {
    if files.is_empty() {
        return Err(NotLanded::NoReadableDiff);
    }

    let mut lines = 0usize;
    for (file, base) in files.iter().zip(bases) {
        lines += file_landed(file, base.as_deref())?.line_count();
    }

    if lines == 0 {
        return Err(NotLanded::NoReadableDiff);
    }
    // Checked last, deliberately: a small pull request that has *not* landed
    // should say so, and reporting "too small to tell" about a change that
    // plainly conflicts with the base branch would be a worse answer than the
    // true one.
    if lines < min_lines {
        return Err(NotLanded::TooSmall);
    }

    Ok(lines)
}

#[cfg(test)]
mod tests;
