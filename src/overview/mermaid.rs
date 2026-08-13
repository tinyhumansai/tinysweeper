//! Rendering a [`ChangeMap`] as a Mermaid flowchart.
//!
//! Always compiled. Mermaid rather than an image: GitHub renders a fenced
//! `mermaid` block natively, so the diagram needs no asset host, no signed URL
//! and no service that can be down — and it stays readable as text in the API,
//! in an email notification, and in a terminal.
//!
//! # Labels are untrusted
//!
//! Every label comes from a repository symbol, and contributor code controls
//! those names. A symbol is therefore treated exactly like a diff: data, never
//! syntax. Two rules enforce it, and both are needed.
//!
//! - **Node ids are generated here** (`n0`, `n1`, …) and never derived from a
//!   path. An id is unquoted Mermaid syntax, so a path could otherwise close
//!   the statement it sits in.
//! - **Label text is filtered to a safe alphabet** by [`label`], not escaped.
//!   Escaping is a guess about a renderer's parser; an allow-list is a
//!   statement about what can appear. A hostile symbol loses its punctuation
//!   and draws an ordinary box rather than becoming Mermaid syntax.

use std::fmt::Write as _;

use crate::config::types::Severity;
use crate::overview::types::{ChangeMap, Component, Role};

/// Characters allowed through into a diagram label.
///
/// An allow-list, deliberately. Everything a real path needs is here; anything
/// else is dropped rather than escaped, because a label is decoration and no
/// decoration is worth a parser argument.
fn allowed(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '/' | '-' | '+' | ' ')
}

/// How long a label may get before it is cut.
///
/// Long enough for `packages/web/src/app/api`, short enough that a deeply
/// nested path does not stretch one box across the whole diagram.
const MAX_LABEL: usize = 44;

/// Make an arbitrary string safe to put inside a quoted Mermaid label.
pub fn label(text: &str) -> String {
    let filtered: String = text.chars().filter(|&c| allowed(c)).collect();
    let trimmed = filtered.trim();
    if trimmed.is_empty() {
        // A path made entirely of characters we dropped still has to draw a
        // box; an empty label would render as a nameless node.
        return "...".to_string();
    }
    if trimmed.chars().count() <= MAX_LABEL {
        return trimmed.to_string();
    }
    // Cut from the left: qualified symbol names keep their distinguishing tail.
    let tail: String = trimmed
        .chars()
        .skip(trimmed.chars().count() - (MAX_LABEL - 3))
        .collect();
    format!("...{tail}")
}

/// Render the map as a fenced Mermaid block, or `None` when there is nothing
/// worth drawing.
pub fn flowchart(map: &ChangeMap) -> Option<String> {
    if !map.worth_drawing() {
        return None;
    }

    let mut out = String::with_capacity(512);
    // Left-to-right follows call/data flow. Inbound callers and tests naturally
    // sit before the changed behaviour; its callees sit after it.
    out.push_str("```mermaid\nflowchart LR\n");

    for (index, component) in map.components.iter().enumerate() {
        let _ = writeln!(
            out,
            "  n{index}[\"{}\"]:::{}",
            node_label(component),
            class_of(component)
        );
    }

    for link in &map.links {
        let relation = link.relation.as_str();
        let edge_label = if link.weight == 1 {
            relation.to_string()
        } else {
            format!("{relation} x{}", link.weight)
        };
        let _ = writeln!(out, "  n{} -->|{}| n{}", link.from, edge_label, link.to);
    }

    out.push_str(CLASS_DEFS);
    out.push_str("```\n");
    Some(out)
}

/// The style for each role, and for a behaviour the review has findings in.
///
/// Colours are stated as fills with explicit stroke and text colours rather
/// than left to Mermaid's theme: a diagram whose text vanishes in dark mode is
/// a diagram half the readers cannot use. These four are legible on both of
/// GitHub's.
const CLASS_DEFS: &str = "  classDef changed fill:#0d4429,stroke:#238636,color:#e6edf3\n  \
     classDef impacted fill:#161b22,stroke:#6e7681,color:#c9d1d9\n  \
     classDef flagged fill:#5a1e02,stroke:#d93f0b,color:#ffffff\n  \
     classDef blocking fill:#67060c,stroke:#f85149,color:#ffffff\n";

/// Which class a behaviour draws in.
fn class_of(component: &Component) -> &'static str {
    match (component.role, component.worst) {
        // Blocking severity gets its own colour rather than sharing the
        // findings one: the whole point of the picture is that a reviewer can
        // see where to look first, and "there is a finding here" and "this
        // finding stops the merge" are not the same instruction.
        (_, Some(Severity::High | Severity::Critical)) => "blocking",
        (_, Some(_)) => "flagged",
        (Role::Changed, None) => "changed",
        (Role::Impacted, None) => "impacted",
    }
}

/// A behaviour's label, without its file-qualified graph id.
fn node_label(component: &Component) -> String {
    let semantic_name = component
        .name
        .split_once('#')
        .map_or(component.name.as_str(), |(_, symbol)| symbol);
    let mut out = label(semantic_name);
    match component.role {
        Role::Changed => {
            out.push_str("<br/>changed");
            if component.findings > 0 {
                let _ = write!(
                    out,
                    "<br/>{} finding{}",
                    component.findings,
                    if component.findings == 1 { "" } else { "s" }
                );
            }
        }
        Role::Impacted => {}
    }
    out
}
