//! Tests for the change map.
//!
//! The golden test at the top is the one that matters: a fixture diff plus a
//! canned graph, asserting the exact diagram. Everything the map claims is
//! arithmetic, so a change to any of it should be visible as a diff in a
//! string, not inferred from a count.

use super::*;
use crate::config::types::{LaneId, Severity};
use crate::evidence::diff::parse_file_patch;
use crate::index::types::{GraphEdge, GraphNode, Neighbourhood, NodeKind};

const REPO: &str = "tinyhumansai/tinysweeper";

fn limits() -> Overview {
    Overview {
        enabled: true,
        max_components: 10,
        max_impacted: 6,
        max_links: 24,
        max_paths_per_component: 12,
    }
}

/// A diff that adds `added` lines to `path`.
fn diff(path: &str, added: usize) -> FileDiff {
    let mut patch = format!("@@ -1,1 +1,{} @@\n fn main() {{}}\n", added + 1);
    for line in 0..added {
        patch.push_str(&format!("+// line {line}\n"));
    }
    parse_file_patch(path, &patch)
}

fn finding(path: &str, severity: Severity) -> Finding {
    Finding {
        lane: LaneId::Critique,
        severity,
        confidence: 0.9,
        path: path.into(),
        line: Some(2),
        end_line: None,
        rule: "r".into(),
        title: "t".into(),
        body: "b".into(),
        suggestion: None,
        late: false,
        identity: None,
    }
}

fn file(path: &str) -> GraphNode {
    GraphNode::file(REPO, path)
}

fn edge(from: &str, to: &str, kind: EdgeKind) -> GraphEdge {
    GraphEdge::new(REPO, from, to, kind, from.split('#').next().unwrap())
}

// --- golden test -----------------------------------------------------------

#[test]
fn golden_a_change_across_two_modules_draws_what_it_reaches() {
    let diffs = [
        diff("src/lanes/critique.rs", 12),
        diff("src/lanes/security.rs", 4),
        diff("src/graph/build.rs", 3),
    ];
    let walk = Neighbourhood {
        nodes: vec![
            file("src/lanes/critique.rs"),
            file("src/lanes/security.rs"),
            file("src/graph/build.rs"),
            file("src/app/review.rs"),
            file("src/app/apply.rs"),
        ],
        edges: vec![
            edge("src/app/review.rs", "src/lanes/critique.rs", EdgeKind::Imports),
            edge("src/app/review.rs", "src/lanes/security.rs", EdgeKind::Imports),
            edge("src/app/apply.rs", "src/lanes/critique.rs", EdgeKind::Imports),
            edge("src/lanes/critique.rs", "src/graph/build.rs", EdgeKind::Imports),
        ],
    };

    let map = build(
        &diffs,
        &[finding("src/lanes/critique.rs", Severity::High)],
        GraphView::Walked(&walk),
        &limits(),
    );
    let diagram = mermaid::flowchart(&map).expect("a map worth drawing");

    assert_eq!(
        diagram,
        "```mermaid\n\
         flowchart LR\n  \
         n0[\"src/lanes<br/>2 files +16 -0<br/>1 finding\"]:::blocking\n  \
         n1[\"src/graph<br/>1 file +3 -0\"]:::changed\n  \
         n2[\"src/app<br/>2 files reached\"]:::impacted\n  \
         n2 -->|3 refs| n0\n  \
         n0 -->|1 ref| n1\n  \
         classDef changed fill:#0d4429,stroke:#238636,color:#e6edf3\n  \
         classDef impacted fill:#161b22,stroke:#6e7681,color:#c9d1d9\n  \
         classDef flagged fill:#5a1e02,stroke:#d93f0b,color:#ffffff\n  \
         classDef blocking fill:#67060c,stroke:#f85149,color:#ffffff\n\
         ```\n"
    );
}

// --- what the map counts ---------------------------------------------------

#[test]
fn components_are_directories_at_the_deepest_depth_that_fits() {
    let diffs = [diff("src/lanes/critique.rs", 1), diff("src/graph/build.rs", 1)];
    let map = build(&diffs, &[], GraphView::Absent, &limits());

    let names: Vec<&str> = map.components.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["src/graph", "src/lanes"]);
}

#[test]
fn a_tight_component_budget_collapses_the_grain_rather_than_dropping_files() {
    let diffs = [diff("src/lanes/critique.rs", 1), diff("src/graph/build.rs", 1)];
    let map = build(
        &diffs,
        &[],
        GraphView::Absent,
        &Overview {
            max_components: 1,
            ..limits()
        },
    );

    // One box called `src` holding both files — not one box called `src/lanes`
    // with the other file silently missing.
    assert_eq!(map.components.len(), 1);
    assert_eq!(map.components[0].name, "src");
    assert_eq!(map.components[0].files, 2);
    assert_eq!(map.folded, 0);
}

#[test]
fn components_that_do_not_fit_are_counted_not_dropped_silently() {
    // Four top-level directories against a budget of two: the depth rule has
    // nowhere left to collapse to, so two components really are left out and
    // the map has to say so.
    let diffs = [
        diff("a/one.rs", 9),
        diff("b/two.rs", 7),
        diff("c/three.rs", 5),
        diff("d/four.rs", 3),
    ];
    let map = build(
        &diffs,
        &[],
        GraphView::Absent,
        &Overview {
            max_components: 2,
            ..limits()
        },
    );

    assert_eq!(map.components.len(), 2);
    assert_eq!(map.folded, 2);
    // Ranked by churn, so what survives is the largest part of the change.
    assert_eq!(map.components[0].name, "a");
    assert_eq!(map.components[1].name, "b");
    assert_eq!(map.files, 4, "the totals still describe the whole change");
}

#[test]
fn a_findings_severity_is_the_worst_in_the_component() {
    let diffs = [diff("src/a.rs", 1), diff("src/b.rs", 1)];
    let map = build(
        &diffs,
        &[
            finding("src/a.rs", Severity::Low),
            finding("src/b.rs", Severity::Critical),
        ],
        GraphView::Absent,
        &limits(),
    );

    assert_eq!(map.components[0].findings, 2);
    assert_eq!(map.components[0].worst, Some(Severity::Critical));
}

#[test]
fn an_unanchored_finding_belongs_to_no_component() {
    // The description lane reports on `(pull request description)`, which is
    // not a file. Inventing a box for it would put something on the diagram
    // that no code is in.
    let diffs = [diff("src/a.rs", 1)];
    let map = build(
        &diffs,
        &[finding("(pull request description)", Severity::High)],
        GraphView::Absent,
        &limits(),
    );

    assert_eq!(map.components[0].findings, 0);
    assert_eq!(map.components[0].worst, None);
}

#[test]
fn a_changed_file_is_never_also_an_impacted_one() {
    let diffs = [diff("src/a.rs", 1)];
    let walk = Neighbourhood {
        nodes: vec![file("src/a.rs"), file("other/b.rs")],
        edges: vec![edge("other/b.rs", "src/a.rs", EdgeKind::Imports)],
    };
    let map = build(&diffs, &[], GraphView::Walked(&walk), &limits());

    assert_eq!(map.changed().count(), 1);
    let impacted: Vec<&str> = map.impacted().map(|c| c.name.as_str()).collect();
    assert_eq!(impacted, ["other"]);
}

#[test]
fn a_files_own_symbols_do_not_become_arrows() {
    // `Defines` runs from a file to a symbol inside it. Drawn, it would be an
    // arrow from a component to itself on every single pull request.
    let diffs = [diff("src/a.rs", 1)];
    let walk = Neighbourhood {
        nodes: vec![
            file("src/a.rs"),
            GraphNode {
                kind: NodeKind::Symbol,
                ..GraphNode::symbol(REPO, "src/a.rs", "run")
            },
        ],
        edges: vec![edge("src/a.rs", "src/a.rs#run", EdgeKind::Defines)],
    };
    let map = build(&diffs, &[], GraphView::Walked(&walk), &limits());

    assert!(map.links.is_empty());
}

#[test]
fn a_symbol_node_is_attributed_to_its_file() {
    let diffs = [diff("src/lanes/a.rs", 1)];
    let walk = Neighbourhood {
        nodes: vec![file("src/lanes/a.rs"), file("src/app/review.rs")],
        edges: vec![edge(
            "src/app/review.rs#run",
            "src/lanes/a.rs#review",
            EdgeKind::Calls,
        )],
    };
    let map = build(&diffs, &[], GraphView::Walked(&walk), &limits());

    assert_eq!(map.links.len(), 1);
    assert_eq!(map.components[map.links[0].from].name, "src/app");
    assert_eq!(map.components[map.links[0].to].name, "src/lanes");
}

#[test]
fn arrows_are_capped_heaviest_first() {
    let diffs = [diff("src/a/one.rs", 1)];
    let walk = Neighbourhood {
        nodes: vec![file("src/a/one.rs"), file("src/b/two.rs"), file("src/c/three.rs")],
        edges: vec![
            edge("src/b/two.rs", "src/a/one.rs", EdgeKind::Imports),
            edge("src/b/two.rs", "src/a/one.rs#run", EdgeKind::Calls),
            edge("src/c/three.rs", "src/a/one.rs", EdgeKind::Imports),
        ],
    };
    let map = build(
        &diffs,
        &[],
        GraphView::Walked(&walk),
        &Overview {
            max_links: 1,
            ..limits()
        },
    );

    assert_eq!(map.links.len(), 1);
    assert_eq!(map.links[0].weight, 2);
    assert_eq!(map.components[map.links[0].from].name, "src/b");
}

// --- degrading honestly ----------------------------------------------------

#[test]
fn the_three_graph_outcomes_are_distinguishable() {
    let diffs = [diff("src/a.rs", 1), diff("docs/b.md", 1)];
    let empty = Neighbourhood::default();

    for (view, expected) in [
        (GraphView::Absent, GraphStatus::Off),
        (GraphView::Unavailable, GraphStatus::Unavailable),
        (GraphView::Walked(&empty), GraphStatus::Cold),
    ] {
        assert_eq!(build(&diffs, &[], view, &limits()).graph, expected);
    }
}

#[test]
fn an_outage_says_so_in_the_comment() {
    let diffs = [diff("src/a.rs", 1), diff("docs/b.md", 1)];
    let body = comment(&build(&diffs, &[], GraphView::Unavailable, &limits()))
        .expect("two components are worth drawing");

    assert!(body.contains("outage"), "{body}");
}

#[test]
fn no_graph_never_reads_as_nothing_reached() {
    let diffs = [diff("src/a.rs", 1), diff("docs/b.md", 1)];
    let body = comment(&build(&diffs, &[], GraphView::Absent, &limits())).expect("worth drawing");

    assert!(body.contains("No code graph is attached"), "{body}");
}

// --- the comment -----------------------------------------------------------

#[test]
fn a_single_component_change_gets_no_comment_at_all() {
    // A box on its own restates the files tab. The whole point of the noise
    // rules is that a comment has to earn its place.
    let diffs = [diff("src/a.rs", 1), diff("src/b.rs", 1)];
    let map = build(&diffs, &[], GraphView::Absent, &limits());

    assert!(!map.worth_drawing());
    assert!(comment(&map).is_none());
}

#[test]
fn the_comment_carries_the_marker_that_finds_it_again() {
    let diffs = [diff("src/a.rs", 1), diff("docs/b.md", 1)];
    let body = comment(&build(&diffs, &[], GraphView::Absent, &limits())).expect("worth drawing");

    assert!(body.starts_with(MARKER));
}

#[test]
fn the_file_list_says_how_many_it_did_not_list() {
    let diffs: Vec<FileDiff> = (0..5)
        .map(|n| diff(&format!("src/f{n}.rs"), 1))
        .chain(std::iter::once(diff("docs/b.md", 1)))
        .collect();
    let body = comment(&build(
        &diffs,
        &[],
        GraphView::Absent,
        &Overview {
            max_paths_per_component: 2,
            ..limits()
        },
    ))
    .expect("worth drawing");

    assert!(body.contains("…and 3 more files"), "{body}");
}

// --- untrusted paths -------------------------------------------------------

#[test]
fn a_hostile_filename_cannot_escape_a_diagram_label() {
    // A contributor picks their own filenames. This one tries to close the
    // node statement and add a `click` directive of its own.
    let hostile = "src/x\"] click n0 \"javascript:alert(1)/evil.rs";
    let diffs = [diff(hostile, 1), diff("docs/b.md", 1)];
    let diagram = mermaid::flowchart(&build(&diffs, &[], GraphView::Absent, &limits()))
        .expect("worth drawing");

    assert!(!diagram.contains("click"), "{diagram}");
    assert!(!diagram.contains("javascript"), "{diagram}");
    // Exactly two opening quotes per node line, and none of them the file's.
    for line in diagram.lines().filter(|l| l.contains("n0[")) {
        assert_eq!(line.matches('"').count(), 2, "{line}");
    }
}

#[test]
fn a_label_made_entirely_of_dropped_characters_still_draws_a_box() {
    assert_eq!(mermaid::label("<<>>"), "...");
}

#[test]
fn a_very_long_component_keeps_its_distinguishing_tail() {
    let long = "packages/very-long-workspace-name/services/api/src/routes/internal";
    let label = mermaid::label(long);

    assert!(label.starts_with("..."));
    assert!(label.ends_with("routes/internal"), "{label}");
    assert!(label.chars().count() <= 44);
}

#[test]
fn a_hostile_filename_is_escaped_in_the_table_too() {
    // The diagram filters; the markdown escapes. Both are needed — a pipe ends
    // a table cell, and GitHub renders inline HTML inside one.
    let diffs = [diff("src/a|b<img src=x>.rs", 1), diff("docs/b.md", 1)];
    let body = comment(&build(&diffs, &[], GraphView::Absent, &limits())).expect("worth drawing");

    assert!(!body.contains("<img"), "{body}");
}
