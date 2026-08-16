//! Turning a model's replacement into a GitHub suggestion GitHub will accept.
//!
//! Always compiled. A pure function over a finding and the parsed diff.
//!
//! A `suggestion` fenced block is the one thing in a review a maintainer can
//! act on without leaving the page: GitHub renders it as a diff with a
//! **Commit suggestion** button. `Finding::suggestion` has always carried the
//! replacement, and it has only ever been rendered as an inert ``` fence in the
//! folded summary — a paragraph the maintainer retypes by hand. Everything
//! needed to make it a button was already there and simply not wired up.
//!
//! It is not wired up naively, because a wrong suggestion is worse than none.
//! An inert fence that misindents its replacement is a typo in a comment; an
//! applicable one is a button that silently breaks the build. So this module is
//! mostly refusals, and every one of them is a case where the block would have
//! been *accepted* by GitHub and *wrong*.
//!
//! ## The comment range is the replaced range
//!
//! GitHub replaces exactly the lines the comment is anchored to — not the lines
//! the prose talks about, and not the lines the replacement was written for.
//! The anchor comes from `existing_code`, which the model is asked to keep to
//! the smallest span that shows the problem; the replacement is whatever it
//! thinks the fix is. Those are two different spans, and only the first one
//! reaches GitHub.
//!
//! So the whole span has to be in the diff — GitHub rejects a comment anchored
//! outside it, and rejects the *entire review* with it, so one over-reaching
//! range would drop every other comment too — and a replacement covering more
//! lines than the anchor does is refused outright. Anchored to one line, a
//! five-line replacement does not overwrite the block it was about: it is
//! inserted in place of the first line and the other four survive underneath
//! it, leaving code that has the fix and the bug in it at once.
//!
//! ## Indentation has to be put back
//!
//! `src/position` normalises leading whitespace away on both sides when it
//! matches a snippet, which is what lets a model that re-indented its quotation
//! still anchor correctly. The replacement arrives re-indented the same way,
//! and GitHub substitutes it verbatim. Left alone, applying a suggestion to a
//! method body dedents it to column zero.
//!
//! aider hits this from the other direction — it applies edits to a working
//! tree rather than proposing them — and its `editblock_coder.py` carries
//! `replace_part_with_missing_leading_whitespace` for exactly this case. The
//! rule taken from it: if every non-blank line of the replacement is short of
//! the same prefix, that prefix was lost in transit and is restored. If the
//! shortfall is *not* uniform the replacement has its own internal structure,
//! nothing can be inferred, and it is left alone.

use crate::evidence::diff::{FileDiff, LineKind};
use crate::findings::types::{Finding, Suggestion};

/// Build the applicable suggestion for `finding`, or `None` if it cannot be
/// made safe.
///
/// `diffs` is the parsed diff of the same review. This runs during review, not
/// during apply, for the same reason
/// [`anchor_context`](crate::findings::anchor::anchor_context) does: the apply
/// path holds a proposal and no diff, so anything needing the diff has to be
/// decided here and travel on the finding.
pub fn applicable(finding: &Finding, diffs: &[FileDiff]) -> Option<Suggestion> {
    let replacement = finding.suggestion.as_deref()?;
    if replacement.trim().is_empty() {
        return None;
    }
    // A replacement that is itself a fenced block would nest inside the
    // suggestion fence and render as literal backticks.
    if replacement.contains("```") {
        return None;
    }

    let (start, end) = finding.range()?;

    // A multi-line replacement anchored to a single line is the one wrong block
    // GitHub accepts without complaint, and it is the shape the schema makes
    // easy to produce: `existing_code` is quoted as the *smallest span that
    // shows the problem* while the replacement is written for the whole
    // construct the problem lives in. Quote one line of a block, rewrite all of
    // it, and the button replaces that one line — inserting the rewrite above
    // the lines it was meant to replace, which stay exactly where they were.
    //
    // The two spans disagree and nothing in the reply says which one was meant,
    // so there is nothing to infer. Widening the anchor to fit the replacement
    // would be a guess about code the model never quoted, so the block is
    // refused and the replacement survives as the inert fence in the summary.
    if start == end && replacement.lines().count() > 1 {
        return None;
    }

    let diff = diffs.iter().find(|d| d.path == finding.path)?;

    // Every line of the span, in order, as it exists in the head revision.
    // `Removed` lines are skipped: they are not in the head revision, so GitHub
    // cannot anchor to them and their text is not what is being replaced.
    let mut original: Vec<&str> = Vec::new();
    for line in start..=end {
        let text = diff
            .hunks
            .iter()
            .flat_map(|hunk| hunk.lines.iter())
            .find(|candidate| {
                candidate.kind != LineKind::Removed && candidate.new_line == Some(line)
            })
            .map(|candidate| candidate.text.as_str())?;
        original.push(text);
    }

    Some(Suggestion {
        start_line: start,
        end_line: end,
        replacement: reindent(replacement, &original),
    })
}

/// Restore the indentation the replacement lost, when it lost a uniform amount.
///
/// The target is the smallest indent across the non-blank lines being
/// replaced — the block's own base level, which is what a replacement written
/// flush-left is missing. Deeper lines inside the block keep their relative
/// extra indent, because only the shortfall is added.
///
/// Returns the replacement unchanged whenever the inference is not safe: it
/// already has that indent, the shortfall differs line to line, or there is
/// nothing to compare against.
fn reindent(replacement: &str, original: &[&str]) -> String {
    let Some(target) = base_indent(original.iter().copied()) else {
        return replacement.to_string();
    };
    if target.is_empty() {
        return replacement.to_string();
    }

    let lines: Vec<&str> = replacement.lines().collect();
    let Some(current) = base_indent(lines.iter().copied()) else {
        return replacement.to_string();
    };
    // Already indented to at least the block's level, or indented some other
    // way entirely. Either way there is no shortfall to infer.
    if !target.starts_with(current) || current.len() == target.len() {
        return replacement.to_string();
    }
    let missing = &target[current.len()..];

    lines
        .iter()
        .map(|line| {
            // A blank line stays blank. Padding it out would add trailing
            // whitespace to the committed file.
            if line.trim().is_empty() {
                (*line).to_string()
            } else {
                format!("{missing}{line}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The longest common leading whitespace across the non-blank lines.
///
/// `None` when every line is blank — there is nothing to infer from. Blank
/// lines are excluded rather than treated as zero-indent, or a single empty
/// line in the middle of a block would report an indent of nothing.
fn base_indent<'a>(lines: impl Iterator<Item = &'a str>) -> Option<&'a str> {
    let mut common: Option<&str> = None;
    for line in lines {
        if line.trim().is_empty() {
            continue;
        }
        let indent = &line[..line.len() - line.trim_start().len()];
        common = Some(match common {
            None => indent,
            Some(previous) => {
                let shared = previous
                    .bytes()
                    .zip(indent.bytes())
                    .take_while(|(a, b)| a == b)
                    .count();
                &previous[..shared]
            }
        });
    }
    common
}

#[cfg(test)]
#[path = "suggest_test.rs"]
mod tests;
