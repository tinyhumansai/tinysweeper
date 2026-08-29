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
    /// The base branch's copy of a file could not be read.
    ///
    /// Distinct from the file being *absent*, and the distinction is the whole
    /// point of the variant. A rate limit or a permission failure that read as
    /// "the file is not there" would tell a deletion-only pull request that
    /// everything it removes is already gone — and close it.
    BaseUnreadable,
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
            NotLanded::RemovedLineStillPresent => "it removes lines the base branch still has",
            NotLanded::BaseUnreadable => {
                "the base branch copy of one of its files could not be read"
            }
        }
    }
}

/// A file as the base branch has it.
///
/// Three states, not two, and the third is why this type exists. "Not there"
/// and "we could not find out" are opposite answers on the deletion path: a
/// pull request that removes a file is superseded if the file is already gone,
/// and completely unjudged if the forge simply would not say. Collapsing them
/// into `Option<String>` is how a rate limit closes somebody's pull request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Base {
    /// The forge served the file. Carries its text.
    Present(String),
    /// The forge answered, and the file is not on the branch.
    Absent,
    /// The forge did not answer: an error, a rate limit, a permission failure.
    Unreadable,
}

/// One hunk of a diff, as the two versions of the text it describes.
///
/// This is the shape that makes the question answerable. A hunk is a *before*
/// image and an *after* image over the same stretch of file, and "this hunk is
/// already applied" is exactly "the base branch contains the after image and
/// not the before image".
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Hunk {
    /// The stretch as it was: context plus removed lines, normalised.
    pub before: Vec<String>,
    /// The stretch as it would be: context plus added lines, normalised.
    pub after: Vec<String>,
    /// How many lines the hunk actually changes, context excluded.
    pub changed: usize,
}

/// Split a unified diff into its hunks.
///
/// Blank and whitespace-only lines are dropped from both images rather than
/// kept, so re-blanking a file is not mistaken for a change; the images are
/// still compared as consecutive runs, which is what the location argument in
/// [`file_landed`] rests on.
///
/// `\ No newline at end of file` is diff chrome, not content, and is skipped.
pub fn hunks(patch: &str) -> Vec<Hunk> {
    let mut out: Vec<Hunk> = Vec::new();
    let mut current = Hunk::default();

    let push = |current: &mut Hunk, out: &mut Vec<Hunk>| {
        if current.changed > 0 {
            out.push(std::mem::take(current));
        } else {
            *current = Hunk::default();
        }
    };

    for line in patch.lines() {
        if line.starts_with("@@") {
            push(&mut current, &mut out);
            continue;
        }
        if line.starts_with('\\') {
            continue;
        }
        match line.as_bytes().first() {
            Some(b'+') if !line.starts_with("+++") => {
                if push_normalised(&line[1..], &mut current.after) {
                    current.changed += 1;
                }
            }
            Some(b'-') if !line.starts_with("---") => {
                if push_normalised(&line[1..], &mut current.before) {
                    current.changed += 1;
                }
            }
            // Context, and the reason this module can say anything about
            // *location*: a line present on both sides anchors the change to a
            // place in the file rather than to the file as a whole.
            Some(b' ') | None => {
                let text = line.strip_prefix(' ').unwrap_or(line);
                let normalised = normalise(text);
                if !normalised.is_empty() {
                    current.before.push(normalised.clone());
                    current.after.push(normalised);
                }
            }
            // A `diff --git`/`index` header between hunks. Not content.
            _ => {}
        }
    }

    push(&mut current, &mut out);
    out
}

/// Add one diff line to an image, reporting whether it counted.
fn push_normalised(text: &str, into: &mut Vec<String>) -> bool {
    let normalised = normalise(text);
    if normalised.is_empty() {
        return false;
    }
    into.push(normalised);
    true
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
/// that are already gone, which is precisely a superseded pull request. The
/// caller is responsible for distinguishing "not there" from "could not read
/// it" — see [`landed`], which refuses the second.
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
    haystack.windows(run.len()).any(|window| window == run)
}

/// Whether one file's change is already on the base branch.
///
/// `base` is that file's content on the base branch, `None` when it is not
/// there at all. Returns the number of changed lines checked.
///
/// Each hunk has to clear both halves of the test, and both halves are
/// necessary:
///
/// - **the after image is on the branch**, as a consecutive run. Because the
///   image carries the hunk's context lines, this is a statement about a
///   *place* in the file and not merely about the lines existing somewhere in
///   it — which is what stops "adds three lines that already appear in some
///   other list" from reading as a change that already landed.
/// - **the before image is not on the branch**, where the hunk has context to
///   make that meaningful. If the change had not been applied, the stretch
///   would still read as it did before, and finding it says so conclusively.
pub fn file_landed(file: &ChangedFile, base: Option<&str>) -> Result<usize, NotLanded> {
    if file.previous_path.is_some() || file.status == FileStatus::Renamed {
        return Err(NotLanded::Renamed);
    }
    let Some(patch) = file.patch.as_deref() else {
        return Err(NotLanded::OpaqueFile);
    };

    let hunks = hunks(patch);
    let base = base_lines(base);
    let mut changed = 0usize;

    for hunk in &hunks {
        // The before image first: it is the conclusive half. A stretch that
        // still reads the way it did before the change proves the change has
        // not been applied, whatever the additions look like.
        //
        // Skipped when the hunk has no context of its own — a whole new file,
        // or an addition at a file boundary — because a before image that is
        // only the removed lines is empty for a pure addition, and "is the
        // empty run present" is true of every file.
        if !hunk.before.is_empty()
            && hunk.before != hunk.after
            && contains_run(&base, &hunk.before)
        {
            return Err(NotLanded::RemovedLineStillPresent);
        }
        if !contains_run(&base, &hunk.after) {
            return Err(NotLanded::AddedLineMissing);
        }
        changed += hunk.changed;
    }

    Ok(changed)
}

/// Whether a whole pull request's change is already on the base branch.
///
/// `bases` supplies each changed file's base-branch state in the same order as
/// `files`; a caller that could not read one passes [`Base::Unreadable`], which
/// refuses the whole pull request rather than being read as an absent file.
///
/// Returns the number of substantive lines checked, which the verdict carries
/// so a maintainer reading the comment knows how much evidence is behind it.
pub fn landed(
    files: &[ChangedFile],
    bases: &[Base],
    min_lines: usize,
) -> Result<usize, NotLanded> {
    // A short `bases` would let `zip` drop the tail of `files` silently, and a
    // pull request judged on the first two of its twenty files is exactly the
    // wrong thing to close. Callers build the two lists together; a mismatch is
    // a bug, and the safe reading of a bug here is "we cannot tell".
    if files.is_empty() || files.len() != bases.len() {
        return Err(NotLanded::NoReadableDiff);
    }

    let mut lines = 0usize;
    for (file, base) in files.iter().zip(bases) {
        let base = match base {
            Base::Present(content) => Some(content.as_str()),
            Base::Absent => None,
            // Refused rather than guessed at, and refused for the whole pull
            // request: one file we could not read is one file whose change we
            // cannot say landed.
            Base::Unreadable => return Err(NotLanded::BaseUnreadable),
        };
        lines += file_landed(file, base)?;
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
#[path = "landed_test.rs"]
mod tests;
