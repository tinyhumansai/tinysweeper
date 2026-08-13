//! The change-map comment: one durable comment per pull request.
//!
//! Always compiled. This renders the body; `crate::app::apply` is what puts it
//! on GitHub, and it edits the same comment forever rather than adding one per
//! push — a bot that appends a fresh diagram to every push is a bot whose
//! diagram nobody scrolls to.
//!
//! The comment answers one question a diff cannot: *how does behaviour flow
//! through what changed?* It deliberately does not summarise intent. Intent is
//! what the author's description is for.

use std::fmt::Write as _;

use crate::VERSION;
use crate::overview::mermaid;
use crate::overview::types::{ChangeMap, GraphStatus};

/// The marker that identifies tinysweeper's own change-map comment.
///
/// Kept in the body so a later push finds and edits the same comment instead of
/// posting a second one. Spelled out rather than composed from
/// [`MARKER_PREFIX`] because a `const` cannot call `format!`; the test below
/// keeps the two from drifting.
pub const MARKER: &str = "<!-- tinysweeper:change-map -->";

/// Render the comment for a map, or `None` when there is nothing to say.
///
/// `None` when the graph has no behavioural relationship to explain. A file or
/// directory inventory is already available in GitHub and earns no comment.
pub fn comment(map: &ChangeMap) -> Option<String> {
    let diagram = mermaid::flowchart(map)?;

    let mut body = format!("{MARKER}\n\n### How this change flows\n\n");
    let _ = writeln!(body, "{}\n", headline(map));
    body.push_str(&diagram);
    body.push('\n');
    body.push_str(LEGEND);
    body.push('\n');
    let _ = write!(
        body,
        "\n![tinysweeper {VERSION}](https://img.shields.io/badge/tinysweeper-{}-8b949e?style=flat-square)\n",
        VERSION.replace('-', "--")
    );
    Some(body)
}

/// The one-line behavioural summary, plus what the graph could contribute.
fn headline(map: &ChangeMap) -> String {
    let changed = map.changed().count();
    let mut line = format!(
        "**{changed} changed behaviour{}** across {} relationship{}.",
        if changed == 1 { "" } else { "s" },
        map.links.len(),
        if map.links.len() == 1 { "" } else { "s" }
    );

    // Said out loud in all three cases. An absent code graph and a change that
    // genuinely reaches nothing produce the same empty diagram, and letting a
    // reviewer read the first as the second is the failure this line exists to
    // prevent.
    let reached = map.impacted().count();
    match map.graph {
        GraphStatus::Off => {
            line.push_str(" No code graph is attached, so no behavioural flow can be inferred.")
        }
        GraphStatus::Unavailable => line.push_str(
            " The code graph did not answer for this review. This is an outage, not an \
             empty flow.",
        ),
        GraphStatus::Cold => line.push_str(
            " The code graph does not know these behaviours yet — normal for newly added \
             code, and a cold index otherwise.",
        ),
        GraphStatus::Walked { nodes } if reached > 0 => {
            let _ = write!(
                line,
                " {reached} surrounding behaviour{} shown ({nodes} graph nodes walked).",
                if reached == 1 { " is" } else { "s are" }
            );
        }
        GraphStatus::Walked { nodes } => {
            let _ = write!(
                line,
                " No surrounding behaviour was found ({nodes} graph nodes walked)."
            );
        }
    }

    if map.folded > 0 {
        let _ = write!(
            line,
            " {} further behaviour{} left out to keep the diagram readable.",
            map.folded,
            if map.folded == 1 { "" } else { "s" }
        );
    }
    line
}

/// What the colours mean, since a colour that has to be guessed carries nothing.
const LEGEND: &str = "<sub>Green: changed behaviour. Grey: surrounding behaviour. \
     Arrows name the call, use, implementation, or test relationship. Orange: has findings. \
     Red: has a finding that blocks the merge.</sub>\n";

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
