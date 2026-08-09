//! The change-map comment: one durable comment per pull request.
//!
//! Always compiled. This renders the body; `crate::app::apply` is what puts it
//! on GitHub, and it edits the same comment forever rather than adding one per
//! push — a bot that appends a fresh diagram to every push is a bot whose
//! diagram nobody scrolls to.
//!
//! The comment answers one question a diff cannot: *what does this change
//! reach?* It deliberately does not summarise intent. Intent is what the
//! author's description is for, and the `description` lane already objects when
//! that is missing; a bot writing the summary its author should have written
//! removes the reason to write one.

use std::fmt::Write as _;

use crate::VERSION;
use crate::findings::render::escape_cell;
use crate::overview::mermaid;
use crate::overview::types::{ChangeMap, Component, GraphStatus, Role};

/// The marker that identifies tinysweeper's own change-map comment.
///
/// Kept in the body so a later push finds and edits the same comment instead of
/// posting a second one. Spelled out rather than composed from
/// [`MARKER_PREFIX`] because a `const` cannot call `format!`; the test below
/// keeps the two from drifting.
pub const MARKER: &str = "<!-- tinysweeper:change-map -->";

/// Render the comment for a map, or `None` when there is nothing to say.
///
/// `None` for a change that is one component with nothing reaching out of it:
/// the diagram would be a single box, the table would restate the files tab,
/// and a comment that adds nothing is the noise this whole tool exists to
/// avoid.
pub fn comment(map: &ChangeMap) -> Option<String> {
    let diagram = mermaid::flowchart(map)?;

    let mut body = format!("{MARKER}\n\n### What this change touches\n\n");
    let _ = writeln!(body, "{}\n", headline(map));
    body.push_str(&diagram);
    body.push('\n');
    body.push_str(LEGEND);
    body.push('\n');
    body.push_str(&table(map));
    body.push_str(&files(map));
    let _ = write!(
        body,
        "\n![tinysweeper {VERSION}](https://img.shields.io/badge/tinysweeper-{}-8b949e?style=flat-square)\n",
        VERSION.replace('-', "--")
    );
    Some(body)
}

/// The one-line count, plus what the graph could and could not contribute.
fn headline(map: &ChangeMap) -> String {
    let changed = map.changed().count();
    let mut line = format!(
        "**{} file{}, +{} -{}** across {changed} component{}.",
        map.files,
        if map.files == 1 { "" } else { "s" },
        map.additions,
        map.deletions,
        if changed == 1 { "" } else { "s" }
    );

    // Said out loud in all three cases. An absent code graph and a change that
    // genuinely reaches nothing produce the same empty diagram, and letting a
    // reviewer read the first as the second is the failure this line exists to
    // prevent.
    let reached = map.impacted().count();
    match map.graph {
        GraphStatus::Off => line.push_str(
            " No code graph is attached to this repository, so this shows the change's own \
             shape and not what it reaches.",
        ),
        GraphStatus::Unavailable => line.push_str(
            " The code graph did not answer for this review, so what the change reaches is \
             not shown. This is an outage, not an empty result.",
        ),
        GraphStatus::Cold => line.push_str(
            " The code graph knows nothing about these files yet — normal for newly added \
             files, and a cold index otherwise.",
        ),
        GraphStatus::Walked { nodes } if reached > 0 => {
            let _ = write!(
                line,
                " It reaches {reached} untouched component{} ({nodes} graph nodes walked).",
                if reached == 1 { "" } else { "s" }
            );
        }
        GraphStatus::Walked { nodes } => {
            let _ = write!(
                line,
                " Nothing outside it imports these files ({nodes} graph nodes walked)."
            );
        }
    }

    if map.folded > 0 {
        let _ = write!(
            line,
            " {} further component{} left out to keep the diagram readable.",
            map.folded,
            if map.folded == 1 { "" } else { "s" }
        );
    }
    line
}

/// What the colours mean, since a colour that has to be guessed carries nothing.
const LEGEND: &str = "<sub>Green: changed. Grey: untouched, reached through an import or a call. \
     Orange: has findings. Red: has a finding that blocks the merge.</sub>\n";

/// One row per component.
fn table(map: &ChangeMap) -> String {
    let mut out =
        String::from("\n| Component | | Files | Lines | Findings |\n|---|---|---:|---:|---:|\n");
    for component in &map.components {
        let _ = writeln!(
            out,
            "| `{}` | {} | {} | {} | {} |",
            escape_cell(&component.name),
            match component.role {
                Role::Changed => "changed",
                Role::Impacted => "reached",
            },
            component.files,
            match component.role {
                Role::Changed => format!("+{} -{}", component.additions, component.deletions),
                // An impacted component has no churn by definition; a `+0 -0`
                // would read as "we looked and it did not move", which is a
                // different and false claim.
                Role::Impacted => "—".to_string(),
            },
            findings_cell(component)
        );
    }
    out
}

/// The findings cell: the count and the worst severity, or nothing at all.
fn findings_cell(component: &Component) -> String {
    match (component.findings, component.worst) {
        (0, _) | (_, None) => "—".to_string(),
        (count, Some(worst)) => format!("{count} ({worst})"),
    }
}

/// The changed files themselves, folded away.
///
/// Folded because GitHub already has a files tab and this is a cross-reference,
/// not a replacement for it — but present, because the diagram names
/// directories and a reviewer asking "which file in `src/lanes`?" should not
/// have to leave the comment to find out.
fn files(map: &ChangeMap) -> String {
    let mut out = String::from("\n<details>\n<summary>Changed files</summary>\n\n");
    for component in map.changed() {
        let _ = writeln!(out, "**`{}`**\n", escape_cell(&component.name));
        for path in &component.paths {
            let _ = writeln!(out, "- `{}`", escape_cell(path));
        }
        let hidden = component.files.saturating_sub(component.paths.len());
        if hidden > 0 {
            let _ = writeln!(
                out,
                "- _…and {hidden} more file{}_",
                if hidden == 1 { "" } else { "s" }
            );
        }
        out.push('\n');
    }
    out.push_str("</details>\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MARKER_PREFIX;

    #[test]
    fn the_marker_carries_the_crate_marker_prefix() {
        // The prefix is how `apply` recognises its own comments; a marker that
        // drifted from it would silently start posting a second comment on
        // every push instead of editing the first.
        assert!(MARKER.contains(MARKER_PREFIX));
    }
}
