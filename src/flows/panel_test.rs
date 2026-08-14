//! What the council graph must be shaped like.

use super::*;

fn call(id: &str) -> Call {
    Call {
        id: id.into(),
        model: "vendor/flash".into(),
        system: format!("system for {id}"),
        prompt: "the evidence".into(),
        schema_name: "tinysweeper_critique".into(),
    }
}

fn graph_of(ids: &[&str]) -> WorkflowGraph {
    let calls: Vec<Call> = ids.iter().map(|id| call(id)).collect();
    council_graph(LaneId::Critique, &calls, &json!({ "type": "object" }))
}

#[test]
fn every_reviewer_is_a_concurrent_successor_of_the_trigger() {
    // The whole point of the graph: N reviewers on one file is N round trips
    // wide, not N deep.
    let graph = graph_of(&["a", "b", "c"]);
    let fan_out = graph
        .edges
        .iter()
        .filter(|e| e.from_node == "trigger")
        .count();

    assert_eq!(fan_out, 3);
}

#[test]
fn every_reviewer_feeds_the_barrier_so_none_is_read_early() {
    let graph = graph_of(&["a", "b", "c"]);
    let joined = graph
        .edges
        .iter()
        .filter(|e| e.to_node == "council")
        .count();

    assert_eq!(joined, 3);
}

#[test]
fn a_solo_reviewer_produces_the_same_shape_as_a_council() {
    // `council::reviewers` always returns at least one reviewer rather than
    // branching, so the graph must not branch either — a solo path and a
    // council path that differ are two paths that drift.
    let graph = graph_of(&["reviewer"]);

    assert!(graph.nodes.iter().any(|n| n.id == "council"));
    assert!(graph.nodes.iter().any(|n| n.id == "trigger"));
    assert_eq!(graph.nodes.len(), 3);
}

#[test]
fn a_reviewer_failing_does_not_stop_the_others() {
    // The engine's default is `stop`, which would lose every other reviewer's
    // work to one provider timeout.
    for node in graph_of(&["a", "b"])
        .nodes
        .iter()
        .filter(|n| n.id.starts_with("reviewer_"))
    {
        assert_eq!(node.config["on_error"], json!("continue"), "{}", node.id);
    }
}

#[test]
fn each_reviewer_carries_its_own_model_and_prompt() {
    // A council whose agents share a model is legal; one whose nodes share a
    // *config* would be a council of one wearing three hats.
    let mut calls = vec![call("a"), call("b")];
    calls[1].model = "vendor/deep".into();

    let graph = council_graph(LaneId::Critique, &calls, &json!({}));
    let node = |id: &str| {
        graph
            .nodes
            .iter()
            .find(|n| n.id == node_id(id))
            .expect("node")
    };

    assert_eq!(node("a").config["model"], json!("vendor/flash"));
    assert_eq!(node("b").config["model"], json!("vendor/deep"));
    assert_eq!(node("a").config["system"], json!("system for a"));
}

#[test]
fn every_node_carries_a_schema_because_prose_is_never_parsed() {
    for node in graph_of(&["a"])
        .nodes
        .iter()
        .filter(|n| n.id.starts_with("reviewer_"))
    {
        assert!(!node.config["schema"].is_null());
        assert!(node.config["schema_name"].is_string());
    }
}

#[test]
fn an_operator_supplied_id_cannot_produce_an_illegal_node_id() {
    // Agent ids come from configuration, so they are not assumed to be legal
    // node ids. `config::validate` already rejects duplicates, so the only job
    // here is producing something the graph can carry.
    assert_eq!(node_id("security-focused"), "reviewer_security_focused");
    assert_eq!(node_id("A Reviewer!"), "reviewer_a_reviewer_");
    assert_eq!(node_id("plain"), "reviewer_plain");
}

#[test]
fn every_lanes_graph_compiles() {
    // Structural validation catches a dangling edge or an unreachable node,
    // and it runs before a token is spent.
    for lane in [
        LaneId::Critique,
        LaneId::Security,
        LaneId::Tests,
        LaneId::Description,
    ] {
        let calls = vec![call("a"), call("b")];
        let graph = council_graph(lane, &calls, &json!({ "type": "object" }));
        tinyflows::compiler::compile(&graph)
            .unwrap_or_else(|e| panic!("{lane:?} council graph: {e}"));
    }
}

#[test]
fn no_calls_still_produces_a_compilable_graph() {
    let graph = council_graph(LaneId::Critique, &[], &json!({}));
    tinyflows::compiler::compile(&graph).expect("an empty council still compiles");
}
