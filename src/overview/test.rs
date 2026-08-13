//! Tests for the behavioural change flow.

use super::*;
use crate::config::types::{LaneId, Severity};
use crate::evidence::diff::parse_file_patch;
use crate::index::types::{GraphEdge, GraphNode, Neighbourhood};

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

fn diff(path: &str, symbol: &str) -> FileDiff {
    parse_file_patch(
        path,
        &format!("@@ -1,1 +1,2 @@ fn {symbol}() {{\n old\n+new\n"),
    )
}

fn symbol(path: &str, name: &str) -> GraphNode {
    GraphNode::symbol(REPO, path, name)
}

fn edge(from: &str, to: &str, kind: EdgeKind) -> GraphEdge {
    GraphEdge::new(REPO, from, to, kind, from.split('#').next().unwrap())
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
        applicable: None,
        late: false,
        identity: None,
    }
}

#[test]
fn golden_draws_the_behavioural_flow_around_the_change() {
    let diffs = [diff("src/order.rs", "settle")];
    let walk = Neighbourhood {
        nodes: vec![
            symbol("src/order.rs", "settle"),
            symbol("src/controller.rs", "handle_request"),
            symbol("src/output.rs", "emit_receipt"),
            symbol("tests/order.rs", "settles_an_order"),
        ],
        edges: vec![
            edge(
                "src/controller.rs#handle_request",
                "src/order.rs#settle",
                EdgeKind::Calls,
            ),
            edge(
                "src/order.rs#settle",
                "src/output.rs#emit_receipt",
                EdgeKind::Calls,
            ),
            edge(
                "tests/order.rs#settles_an_order",
                "src/order.rs#settle",
                EdgeKind::Tests,
            ),
        ],
    };

    let map = build(
        &diffs,
        &[finding("src/order.rs", Severity::High)],
        GraphView::Walked(&walk),
        &limits(),
    );
    let diagram = mermaid::flowchart(&map).expect("a flow worth drawing");

    assert_eq!(
        diagram,
        "```mermaid\n\
         flowchart LR\n  \
         n0[\"settle<br/>changed<br/>1 finding\"]:::blocking\n  \
         n1[\"handle_request\"]:::impacted\n  \
         n2[\"emit_receipt\"]:::impacted\n  \
         n3[\"settles_an_order\"]:::impacted\n  \
         n0 -->|calls| n2\n  \
         n1 -->|calls| n0\n  \
         n3 -->|tests| n0\n  \
         classDef changed fill:#0d4429,stroke:#238636,color:#e6edf3\n  \
         classDef impacted fill:#161b22,stroke:#6e7681,color:#c9d1d9\n  \
         classDef flagged fill:#5a1e02,stroke:#d93f0b,color:#ffffff\n  \
         classDef blocking fill:#67060c,stroke:#f85149,color:#ffffff\n\
         ```\n"
    );

    let body = comment(&map).expect("a flow worth publishing");
    assert!(body.contains("### How this change flows"), "{body}");
    assert!(body.contains("**1 changed behaviour**"), "{body}");
    assert!(!body.contains("Changed files"), "{body}");
    assert!(!body.contains("src/order.rs"), "{body}");
    assert!(!body.contains("+1 -"), "{body}");
}

#[test]
fn file_imports_do_not_masquerade_as_behavioural_flow() {
    let diffs = [diff("src/order.rs", "settle")];
    let walk = Neighbourhood {
        nodes: vec![
            symbol("src/order.rs", "settle"),
            GraphNode::file(REPO, "src/controller.rs"),
            GraphNode::file(REPO, "src/order.rs"),
        ],
        edges: vec![edge("src/controller.rs", "src/order.rs", EdgeKind::Imports)],
    };

    let map = build(&diffs, &[], GraphView::Walked(&walk), &limits());
    assert!(map.links.is_empty());
    assert!(comment(&map).is_none());
}

#[test]
fn a_diff_without_a_named_behaviour_does_not_fall_back_to_files() {
    let diff = parse_file_patch("config/app.toml", "@@ -1,1 +1,2 @@\n old\n+new\n");
    let map = build(&[diff], &[], GraphView::Absent, &limits());

    assert!(map.components.is_empty());
    assert!(comment(&map).is_none());
}

#[test]
fn disconnected_symbols_do_not_become_inventory_boxes() {
    let diffs = [
        diff("src/order.rs", "settle"),
        diff("src/audit.rs", "record"),
    ];
    let walk = Neighbourhood {
        nodes: vec![
            symbol("src/order.rs", "settle"),
            symbol("src/audit.rs", "record"),
            symbol("src/controller.rs", "handle"),
        ],
        edges: vec![edge(
            "src/controller.rs#handle",
            "src/order.rs#settle",
            EdgeKind::Calls,
        )],
    };
    let map = build(&diffs, &[], GraphView::Walked(&walk), &limits());

    assert_eq!(map.changed().count(), 1);
    assert!(
        map.components
            .iter()
            .all(|node| !node.name.ends_with("#record"))
    );
    assert_eq!(map.folded, 1);
}

#[test]
fn calls_uses_implementations_and_tests_are_named_on_arrows() {
    let diffs = [diff("src/api.rs", "serve")];
    let mut nodes = vec![symbol("src/api.rs", "serve")];
    let mut edges = Vec::new();
    for (path, name, kind) in [
        ("src/caller.rs", "call", EdgeKind::Calls),
        ("src/user.rs", "read", EdgeKind::References),
        ("src/impl.rs", "handler", EdgeKind::Extends),
        ("tests/api.rs", "works", EdgeKind::Tests),
    ] {
        nodes.push(symbol(path, name));
        edges.push(edge(&format!("{path}#{name}"), "src/api.rs#serve", kind));
    }
    let walk = Neighbourhood { nodes, edges };
    let diagram = mermaid::flowchart(&build(&diffs, &[], GraphView::Walked(&walk), &limits()))
        .expect("typed flow");

    for label in ["|calls|", "|uses|", "|implements|", "|tests|"] {
        assert!(diagram.contains(label), "missing {label} in {diagram}");
    }
}

#[test]
fn findings_mark_the_changed_behaviour_not_a_directory() {
    let diffs = [diff("src/a.rs", "alpha"), diff("src/b.rs", "beta")];
    let walk = Neighbourhood {
        nodes: vec![symbol("src/a.rs", "alpha"), symbol("src/b.rs", "beta")],
        edges: vec![edge("src/a.rs#alpha", "src/b.rs#beta", EdgeKind::Calls)],
    };
    let map = build(
        &diffs,
        &[finding("src/b.rs", Severity::Critical)],
        GraphView::Walked(&walk),
        &limits(),
    );

    let alpha = map
        .components
        .iter()
        .find(|node| node.name.ends_with("#alpha"))
        .unwrap();
    let beta = map
        .components
        .iter()
        .find(|node| node.name.ends_with("#beta"))
        .unwrap();
    assert_eq!(alpha.findings, 0);
    assert_eq!(beta.findings, 1);
    assert_eq!(beta.worst, Some(Severity::Critical));
}

#[test]
fn arrows_are_capped_heaviest_first() {
    let diffs = [diff("src/a.rs", "alpha")];
    let walk = Neighbourhood {
        nodes: vec![
            symbol("src/a.rs", "alpha"),
            symbol("src/b.rs", "beta"),
            symbol("src/c.rs", "gamma"),
        ],
        edges: vec![
            edge("src/b.rs#beta", "src/a.rs#alpha", EdgeKind::Calls),
            edge("src/b.rs#beta", "src/a.rs#alpha", EdgeKind::Calls),
            edge("src/c.rs#gamma", "src/a.rs#alpha", EdgeKind::Calls),
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
    assert_eq!(map.links[0].relation, FlowRelation::Calls);
}

#[test]
fn graph_outcomes_remain_distinguishable() {
    let diffs = [diff("src/a.rs", "alpha")];
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
fn a_hostile_symbol_cannot_escape_the_diagram() {
    let hostile = "settle\"] click n0 \"javascript:alert(1)";
    let diffs = [diff("src/order.rs", "settle")];
    let hostile_id = format!("src/caller.rs#{hostile}");
    let walk = Neighbourhood {
        nodes: vec![
            symbol("src/order.rs", "settle"),
            symbol("src/caller.rs", hostile),
        ],
        edges: vec![edge(&hostile_id, "src/order.rs#settle", EdgeKind::Calls)],
    };
    let diagram =
        mermaid::flowchart(&build(&diffs, &[], GraphView::Walked(&walk), &limits())).expect("flow");

    for line in diagram.lines().filter(|line| line.contains("[\"")) {
        assert_eq!(line.matches('"').count(), 2, "{line}");
        assert_eq!(line.matches(']').count(), 1, "{line}");
        assert_eq!(line.matches('[').count(), 1, "{line}");
    }
}

#[test]
fn label_filter_handles_empty_and_long_names() {
    assert_eq!(mermaid::label("<<>>"), "...");
    let long = "Service::ANameThatIsMuchLongerThanAnyBoxShouldEverNeed::handle_request";
    let label = mermaid::label(long);
    assert!(label.starts_with("..."));
    assert!(label.ends_with("handle_request"));
    assert!(label.chars().count() <= 44);
}
