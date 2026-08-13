//! What the panel graph must be shaped like.

use super::*;

fn schema() -> Value {
    json!({ "type": "object" })
}

fn propose(lane: LaneId) -> WorkflowGraph {
    propose_graph(lane, schema(), "evidence", |lens| {
        format!("system for {}", lens.id)
    })
}

#[test]
fn every_lens_becomes_a_concurrent_successor_of_the_trigger() {
    let graph = propose(LaneId::Critique);
    let fan_out = graph
        .edges
        .iter()
        .filter(|e| e.from_node == "trigger")
        .count();

    assert_eq!(fan_out, lenses(LaneId::Critique).len());
}

#[test]
fn every_lens_feeds_the_barrier_so_none_is_read_early() {
    // The merge node is what makes the round a barrier. A lens wired straight
    // to the output would have its findings read while others were still
    // running, which is a partial review that reads like a complete one.
    let graph = propose(LaneId::Critique);
    let into_panel = graph
        .edges
        .iter()
        .filter(|e| e.to_node == "panel")
        .count();

    assert_eq!(into_panel, lenses(LaneId::Critique).len());
}

#[test]
fn a_lane_with_one_lens_still_has_the_barrier() {
    // So the node a run reads results from has one name across every lane.
    let graph = propose(LaneId::Description);
    assert!(graph.nodes.iter().any(|n| n.id == "panel"));
}

#[test]
fn every_panellist_runs_on_the_flash_tier() {
    // The panel is the quality mechanism. Paying deep-tier prices per member
    // would make it strictly more expensive than the single call it replaced,
    // which is the whole economic argument gone.
    let graph = propose(LaneId::Critique);

    for node in graph.nodes.iter().filter(|n| n.id.starts_with("lens_")) {
        assert_eq!(node.config["tier"], json!("flash"), "{}", node.id);
    }
}

#[test]
fn each_lens_carries_its_own_system_prompt() {
    let graph = propose(LaneId::Critique);
    let systems: Vec<&str> = graph
        .nodes
        .iter()
        .filter(|n| n.id.starts_with("lens_"))
        .map(|n| n.config["system"].as_str().unwrap())
        .collect();

    let unique: std::collections::BTreeSet<&&str> = systems.iter().collect();
    assert_eq!(unique.len(), systems.len(), "lenses shared a prompt");
}

#[test]
fn every_agent_node_carries_a_schema_because_prose_is_never_parsed() {
    let graph = propose(LaneId::Security);

    for node in graph.nodes.iter().filter(|n| n.id.starts_with("lens_")) {
        assert!(!node.config["schema"].is_null(), "{}", node.id);
        assert!(node.config["schema_name"].is_string(), "{}", node.id);
    }
}

#[test]
fn the_verify_round_runs_an_odd_number_of_judges() {
    // `consensus::settle` drops a tie, so an even panel wastes a call: two
    // verifiers can only ever be as decisive as one.
    assert_eq!(VERIFIERS % 2, 1);

    let graph = verify_graph(LaneId::Critique, schema(), "system", "prompt");
    let judges = graph
        .nodes
        .iter()
        .filter(|n| n.id.starts_with("verifier_"))
        .count();

    assert_eq!(judges, VERIFIERS);
}

#[test]
fn the_commits_lane_has_no_panel() {
    // It makes no model call — its verdict is a regular expression's. A
    // default lens here would quietly start spending money on it.
    assert!(lenses(LaneId::Commits).is_empty());
}

#[test]
fn no_lane_repeats_a_lens_id() {
    // Two lenses with one id collide as node ids, and the second silently
    // replaces the first — a panellist that vanishes without any error.
    for lane in [
        LaneId::Critique,
        LaneId::Security,
        LaneId::Tests,
        LaneId::Description,
    ] {
        let ids: Vec<&str> = lenses(lane).iter().map(|l| l.id).collect();
        let unique: std::collections::BTreeSet<&&str> = ids.iter().collect();
        assert_eq!(unique.len(), ids.len(), "{lane:?} repeats a lens id");
    }
}

#[test]
fn every_graph_compiles() {
    // Structural validation is what catches a dangling edge or an unreachable
    // node, and it runs before any token is spent.
    for lane in [
        LaneId::Critique,
        LaneId::Security,
        LaneId::Tests,
        LaneId::Description,
    ] {
        tinyflows::compiler::compile(&propose(lane))
            .unwrap_or_else(|e| panic!("{lane:?} propose graph: {e}"));
    }

    tinyflows::compiler::compile(&verify_graph(LaneId::Critique, schema(), "s", "p"))
        .expect("verify graph");
}
