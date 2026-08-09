//! Ranking tests.
//!
//! The one that matters is [`a_node_reached_by_many_paths_outranks_an_equally_distant_one`]:
//! it is the whole reason this module exists, and it is the case the previous
//! distance-only cap decided alphabetically.

use super::*;

use crate::index::types::{GraphEdge, GraphNode};

const REPO: &str = "tinyhumansai/example";

fn nodes(names: &[&str]) -> Vec<GraphNode> {
    names
        .iter()
        .map(|n| GraphNode::file(REPO, format!("{n}.ts")))
        .collect()
}

fn calls(from: &str, to: &str) -> GraphEdge {
    GraphEdge::new(
        REPO,
        format!("{from}.ts"),
        format!("{to}.ts"),
        EdgeKind::Calls,
        format!("{from}.ts"),
    )
}

fn seed(name: &str) -> Vec<String> {
    vec![format!("{name}.ts")]
}

fn score_of(ranked: &[(String, f64)], name: &str) -> f64 {
    let id = format!("{name}.ts");
    ranked
        .iter()
        .find(|(node, _)| *node == id)
        .map(|(_, score)| *score)
        .unwrap_or_else(|| panic!("{id} was not ranked"))
}

#[test]
fn an_empty_neighbourhood_ranks_nothing() {
    assert!(rank(&Neighbourhood::default(), &seed("a")).is_empty());
}

#[test]
fn the_seed_outranks_everything_reached_from_it() {
    let hood = Neighbourhood {
        nodes: nodes(&["a", "b", "c"]),
        edges: vec![calls("a", "b"), calls("b", "c")],
    };
    let ranked = rank(&hood, &seed("a"));
    assert_eq!(ranked[0].0, "a.ts");
}

#[test]
fn score_decays_with_distance_from_the_seed() {
    let hood = Neighbourhood {
        nodes: nodes(&["a", "b", "c", "d"]),
        edges: vec![calls("a", "b"), calls("b", "c"), calls("c", "d")],
    };
    let ranked = rank(&hood, &seed("a"));
    let ids: Vec<&str> = ranked.iter().map(|(id, _)| id.as_str()).collect();
    assert_eq!(ids, ["a.ts", "b.ts", "c.ts", "d.ts"]);
}

/// The headline case. `hub` and `leaf` are both exactly two hops from the seed,
/// so the old distance-ordered cap ranked them by id — `hub` only survived
/// because `h` sorts before `l`. Here `hub` wins because three different
/// callers of the seed also reach it, which is a fact about the change rather
/// than about the alphabet.
#[test]
fn a_node_reached_by_many_paths_outranks_an_equally_distant_one() {
    let hood = Neighbourhood {
        nodes: nodes(&["seed", "one", "two", "three", "hub", "leaf"]),
        edges: vec![
            calls("one", "seed"),
            calls("two", "seed"),
            calls("three", "seed"),
            // Every caller of the seed also goes through `hub`.
            calls("one", "hub"),
            calls("two", "hub"),
            calls("three", "hub"),
            // `leaf` hangs off a single one of them, at the same distance.
            calls("one", "leaf"),
        ],
    };
    let ranked = rank(&hood, &seed("seed"));
    assert!(
        score_of(&ranked, "hub") > score_of(&ranked, "leaf"),
        "hub {:?} did not outrank leaf {:?}",
        score_of(&ranked, "hub"),
        score_of(&ranked, "leaf"),
    );
    // And the ordering is what a caller would truncate on.
    let ids: Vec<&str> = ranked.iter().map(|(id, _)| id.as_str()).collect();
    assert!(ids.iter().position(|id| *id == "hub.ts") < ids.iter().position(|id| *id == "leaf.ts"));
}

/// Distance is still the dominant term: a well-connected node far from the diff
/// must not outrank the diff's immediate neighbours, or the ranking has stopped
/// being about the change.
#[test]
fn connectivity_does_not_beat_being_next_to_the_seed() {
    let hood = Neighbourhood {
        nodes: nodes(&["seed", "near", "far", "x", "y", "z"]),
        edges: vec![
            calls("seed", "near"),
            calls("near", "far"),
            calls("far", "x"),
            calls("far", "y"),
            calls("far", "z"),
        ],
    };
    let ranked = rank(&hood, &seed("seed"));
    assert!(score_of(&ranked, "near") > score_of(&ranked, "far"));
}

#[test]
fn a_calls_edge_carries_more_than_a_reference() {
    let hood = Neighbourhood {
        nodes: nodes(&["seed", "called", "mentioned"]),
        edges: vec![
            calls("seed", "called"),
            GraphEdge::new(
                REPO,
                "seed.ts",
                "mentioned.ts",
                EdgeKind::References,
                "seed.ts",
            ),
        ],
    };
    let ranked = rank(&hood, &seed("seed"));
    assert!(score_of(&ranked, "called") > score_of(&ranked, "mentioned"));
}

/// A file's `defines` edges must not let it outrank the change's real
/// neighbours simply by being large.
#[test]
fn containment_does_not_let_a_big_file_dominate() {
    let mut nodes = nodes(&["seed", "caller"]);
    let mut edges = vec![calls("caller", "seed")];
    for i in 0..20 {
        let id = format!("big.ts#sym{i}");
        nodes.push(GraphNode::symbol(REPO, "big.ts", format!("sym{i}")));
        edges.push(GraphEdge::new(
            REPO,
            "big.ts",
            &id,
            EdgeKind::Defines,
            "big.ts",
        ));
    }
    nodes.push(GraphNode::file(REPO, "big.ts"));
    edges.push(calls("seed", "big"));

    let hood = Neighbourhood { nodes, edges };
    let ranked = rank(&hood, &seed("seed"));
    // The 20 symbols inside `big.ts` must not crowd out the actual caller.
    assert!(
        score_of(&ranked, "caller")
            > ranked
                .iter()
                .filter(|(id, _)| id.starts_with("big.ts#"))
                .map(|(_, s)| *s)
                .fold(0.0_f64, f64::max),
        "a symbol inside big.ts outranked the seed's caller"
    );
}

#[test]
fn the_distribution_is_normalised() {
    let hood = Neighbourhood {
        nodes: nodes(&["a", "b", "c", "lonely"]),
        edges: vec![calls("a", "b"), calls("b", "c")],
    };
    let total: f64 = rank(&hood, &seed("a")).iter().map(|(_, s)| s).sum();
    assert!(
        (total - 1.0).abs() < 1e-6,
        "scores summed to {total}, not 1 — mass is leaking"
    );
}

#[test]
fn an_isolated_node_is_ranked_rather_than_dropped() {
    let hood = Neighbourhood {
        nodes: nodes(&["a", "lonely"]),
        edges: Vec::new(),
    };
    let ranked = rank(&hood, &seed("a"));
    assert_eq!(ranked.len(), 2);
    assert!(score_of(&ranked, "a") > score_of(&ranked, "lonely"));
}

#[test]
fn a_seed_the_neighbourhood_does_not_contain_falls_back_to_uniform() {
    // A pull request that adds a file names a path the graph has never seen.
    let hood = Neighbourhood {
        nodes: nodes(&["a", "b"]),
        edges: vec![calls("a", "b")],
    };
    let ranked = rank(&hood, &seed("brand-new"));
    assert_eq!(ranked.len(), 2);
    let total: f64 = ranked.iter().map(|(_, s)| s).sum();
    assert!((total - 1.0).abs() < 1e-6);
}

#[test]
fn ties_break_on_id_so_the_order_is_reproducible() {
    // A perfectly symmetric star: every leaf has the identical score.
    let hood = Neighbourhood {
        nodes: nodes(&["seed", "c", "a", "b"]),
        edges: vec![calls("seed", "c"), calls("seed", "a"), calls("seed", "b")],
    };
    let ranked = rank(&hood, &seed("seed"));
    let ids: Vec<&str> = ranked.iter().map(|(id, _)| id.as_str()).collect();
    assert_eq!(ids, ["seed.ts", "a.ts", "b.ts", "c.ts"]);
}

#[test]
fn an_edge_pointing_outside_the_node_set_is_skipped() {
    let hood = Neighbourhood {
        nodes: nodes(&["a", "b"]),
        edges: vec![calls("a", "b"), calls("b", "dropped-by-the-cap")],
    };
    let ranked = rank(&hood, &seed("a"));
    assert_eq!(ranked.len(), 2);
    let total: f64 = ranked.iter().map(|(_, s)| s).sum();
    assert!((total - 1.0).abs() < 1e-6);
}
