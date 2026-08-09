//! Complexity, measured rather than judged.
//!
//! "Is this pull request simple enough to merge without a human?" is exactly
//! the question a model would answer plausibly and unaccountably, so it is not
//! asked here. Every number below is arithmetic over the file list the forge
//! returned: lines added and removed, files touched, hunks in the diff, and
//! distinct directories reached. Same inputs, same answer, every time.

use std::collections::BTreeSet;

use crate::forge::types::ChangedFile;

/// The measured shape of a diff.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Complexity {
    /// How many files the pull request touches.
    pub files: usize,
    /// Additions plus deletions across every file.
    pub changed_lines: u64,
    /// How many `@@` hunks the diff contains.
    pub hunks: usize,
    /// How many distinct directories are reached, counting the repository
    /// root as one.
    pub directories: usize,
}

/// Measure a diff, or name the first file that could not be measured.
///
/// A file the forge returned no patch for — binary, or past its diff size
/// limit — has no countable hunks. Counting it as zero would let the largest
/// changes through the tightest cap, so it is reported as unmeasurable and the
/// caller refuses instead. An entry with no patch *and* no line movement is a
/// rename or a mode change: genuinely nothing to read, and nothing hiding.
pub fn measure(files: &[ChangedFile]) -> Result<Complexity, String> {
    let mut measured = Complexity {
        files: files.len(),
        ..Complexity::default()
    };
    let mut directories = BTreeSet::new();

    for file in files {
        measured.changed_lines = measured
            .changed_lines
            .saturating_add(file.additions)
            .saturating_add(file.deletions);
        directories.insert(directory_of(&file.path));
        if let Some(previous) = &file.previous_path {
            directories.insert(directory_of(previous));
        }

        match &file.patch {
            Some(patch) => measured.hunks += hunks_in(patch),
            None if file.additions == 0 && file.deletions == 0 => {}
            None => return Err(file.path.clone()),
        }
    }

    measured.directories = directories.len();
    Ok(measured)
}

/// The directory a path lives in. The repository root is `""`.
fn directory_of(path: &str) -> &str {
    match path.rsplit_once('/') {
        Some((dir, _)) => dir,
        None => "",
    }
}

/// How many hunks a unified patch contains.
///
/// Counted off the `@@` marker at the start of a line. A `@@` written inside
/// the code being changed is preceded by the diff's own space, `+` or `-`
/// prefix, so it does not start the line and is not counted — which is what
/// stops a contributor inflating or deflating their own hunk count.
fn hunks_in(patch: &str) -> usize {
    patch.lines().filter(|line| line.starts_with("@@")).count()
}
