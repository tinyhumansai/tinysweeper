//! Turning a quoted snippet into a line range.
//!
//! Models are bad at line arithmetic and good at copying. So a lane's model
//! never emits a line number: it emits `existing_code`, a verbatim snippet of
//! what it is complaining about, and the host computes the range. Everything a
//! model gets wrong about counting lines becomes irrelevant, and the failure
//! mode changes from *a good finding posted on the wrong line* — or dropped for
//! having the wrong number — to *a good finding without a line*, which is
//! recoverable.
//!
//! Resolution runs in three stages, **first hit wins**:
//!
//! 1. [`locate`] slides the snippet over every hunk, new side first, then the
//!    old side. This is where nearly everything resolves, and it costs nothing.
//! 2. [`locate`] slides it over the whole head-revision file, when the caller
//!    has the file. This catches a finding anchored to context the diff never
//!    included.
//! 3. [`Positioner::resolve`] spends one cheap model call whose only job is to
//!    extract the exact snippet from the diff that the comment refers to, then
//!    re-runs stages 1 and 2 with it. If that fails too, the original snippet
//!    is restored and the finding is reported [`Unanchored`] rather than
//!    dropped.
//!
//! Stage 3 uses the **scan** tier deliberately. Extracting a quoted span is not
//! a reasoning task, and paying deep-tier prices per unresolved finding would
//! cost more than the findings are worth.
//!
//! Nothing here writes anything. A resolution is advisory input to a lane, and
//! lanes cannot mutate a pull request at all.

pub mod relocate;
pub mod snippet;
pub mod types;

#[cfg(test)]
mod test;

use crate::config::types::Config;
use crate::evidence::diff::{FileDiff, Hunk, LineKind};
use crate::ports::model::{Model, Usage};

pub use crate::position::types::{Anchor, Resolution, Side, Stage, Unanchored};

/// Everything needed to place one finding.
#[derive(Debug, Clone, Copy)]
pub struct PositionRequest<'a> {
    /// The snippet the model quoted.
    pub snippet: &'a str,
    /// The parsed diff of the file the finding is about, when the pull request
    /// touched it.
    pub diff: Option<&'a FileDiff>,
    /// The head-revision content of that file, when the caller has it. Absent
    /// on a forge-only run, which simply skips stage 2.
    pub file: Option<&'a str>,
    /// What the finding says, given to the re-location call so it knows what
    /// to look for.
    pub comment: &'a str,
    /// The rendered diff the lane showed the model, reused verbatim by the
    /// re-location call.
    pub rendered_diff: &'a str,
}

/// Stages 1 and 2: pure, offline, and the ones that resolve almost everything.
pub fn locate(snippet: &str, diff: Option<&FileDiff>, file: Option<&str>) -> Resolution {
    let needle = snippet::needle(snippet);
    if needle.is_empty() {
        return Resolution::Unanchored(Unanchored::NoSnippet);
    }

    // Stage 1. The new side first: a finding is about the code as it will be
    // after the merge, so a match there is the one we want even when the same
    // text also appears among the deleted lines.
    if let Some(diff) = diff {
        for hunk in &diff.hunks {
            if let Some((start, end)) = snippet::find(&project_new(hunk), &needle) {
                return anchored(start, end, Side::New, Stage::Hunk);
            }
        }
        for hunk in &diff.hunks {
            if let Some((start, end)) = snippet::find(&project_old(hunk), &needle) {
                return anchored(start, end, Side::Old, Stage::Hunk);
            }
        }
    }

    // Stage 2.
    if let Some(file) = file
        && let Some((start, end)) = snippet::find(&snippet::haystack(file), &needle)
    {
        return anchored(start, end, Side::New, Stage::WholeFile);
    }

    Resolution::Unanchored(Unanchored::NoMatch)
}

/// Resolves snippets, escalating to one cheap model call when the offline
/// stages fail.
pub struct Positioner<'a> {
    model: &'a dyn Model,
    config: &'a Config,
}

impl<'a> Positioner<'a> {
    /// Build a positioner over `model`.
    pub fn new(model: &'a dyn Model, config: &'a Config) -> Self {
        Self { model, config }
    }

    /// Run all three stages. `usage` accumulates whatever stage 3 spent.
    ///
    /// Never fails: a re-location call that errors, times out or answers with
    /// nonsense leaves the finding unanchored, which is a degraded result
    /// rather than a failed review.
    pub async fn resolve(&self, request: PositionRequest<'_>, usage: &mut Usage) -> Resolution {
        let first = locate(request.snippet, request.diff, request.file);
        if first.is_anchored() {
            return first;
        }

        // Stage 3. Only worth a call when there is a diff to quote from.
        if request.rendered_diff.trim().is_empty() {
            return first;
        }

        let recovered = match relocate::relocate(
            self.model,
            self.config,
            request.comment,
            request.rendered_diff,
        )
        .await
        {
            Some((snippet, spent)) => {
                usage.add(spent);
                snippet
            }
            None => return first,
        };

        match locate(&recovered, request.diff, request.file) {
            Resolution::Anchored(anchor) => Resolution::Anchored(Anchor {
                relocated: true,
                ..anchor
            }),
            // The original snippet is what the finding quotes to the author, so
            // a failed re-location must leave the finding exactly as it was.
            Resolution::Unanchored(_) => first,
        }
    }
}

fn anchored(start: u64, end: u64, side: Side, stage: Stage) -> Resolution {
    Resolution::Anchored(Anchor {
        start,
        end,
        side,
        stage,
        relocated: false,
    })
}

/// Project a hunk onto the head revision: context and added lines.
fn project_new(hunk: &Hunk) -> Vec<(u64, &str)> {
    hunk.lines
        .iter()
        .filter(|line| line.kind != LineKind::Removed)
        .filter_map(|line| Some((line.new_line?, snippet::normalize(&line.text))))
        .filter(|(_, text)| !text.is_empty())
        .collect()
}

/// Project a hunk onto the base revision: context and deleted lines.
///
/// The key is still a **head-revision** line number. A deleted line has none of
/// its own, so it borrows the nearest one in the hunk — the comment then lands
/// beside the deletion instead of on whatever unrelated code now occupies the
/// base-revision number.
fn project_old(hunk: &Hunk) -> Vec<(u64, &str)> {
    let anchors = nearest_new_lines(hunk);
    hunk.lines
        .iter()
        .zip(anchors)
        .filter(|(line, _)| line.kind != LineKind::Added)
        .map(|(line, anchor)| (anchor, snippet::normalize(&line.text)))
        .filter(|(_, text)| !text.is_empty())
        .collect()
}

/// For each line of a hunk, the head-revision line a comment about it should
/// sit on.
fn nearest_new_lines(hunk: &Hunk) -> Vec<u64> {
    let mut anchors: Vec<Option<u64>> = vec![None; hunk.lines.len()];

    // Backwards first, so every line inherits the *next* surviving line: a
    // comment about a deleted block reads best against the code that replaced
    // it, which is what follows the deletion.
    let mut next: Option<u64> = None;
    for (index, line) in hunk.lines.iter().enumerate().rev() {
        if let Some(new_line) = line.new_line {
            next = Some(new_line);
        }
        anchors[index] = next;
    }

    // Then forwards, for a deletion at the end of the hunk with nothing after
    // it to inherit from.
    let mut previous: Option<u64> = None;
    for (index, line) in hunk.lines.iter().enumerate() {
        if let Some(new_line) = line.new_line {
            previous = Some(new_line);
        }
        if anchors[index].is_none() {
            anchors[index] = previous;
        }
    }

    // A hunk of nothing but deletions leaves the whole file shorter; its head
    // revision position is where the hunk starts. Line 0 does not exist.
    let fallback = hunk.new_start.max(1);
    anchors
        .into_iter()
        .map(|anchor| anchor.unwrap_or(fallback))
        .collect()
}
