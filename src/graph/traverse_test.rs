//! Traversal tests: the hop bound, the edge-kind filter, and the hard node cap
//! that keeps a neighbourhood small enough to put in a prompt.

use super::*;

use crate::index::MockGraphStore;

const REPO: &str = "tinyhumansai/example";

/// A chain `a -> b -> c -> d`, one `calls` edge per link, plus a `references`
/// edge from `a` to `z` so the kind filter has something to exclude.
async fn chain() -> MockGraphStore {
    let store = MockGraphStore::new();
    let nodes: Vec<GraphNode> = ["a", "b", "c", "d", "z"]
        .iter()
        .map(|name| GraphNode::file(REPO, format!("{name}.ts")))
        .collect();
    let edges = vec![
        GraphEdge::new(REPO, "a.ts", "b.ts", EdgeKind::Calls, "a.ts"),
        GraphEdge::new(REPO, "b.ts", "c.ts", EdgeKind::Calls, "b.ts"),
        GraphEdge::new(REPO, "c.ts", "d.ts", EdgeKind::Calls, "c.ts"),
        GraphEdge::new(REPO, "a.ts", "z.ts", EdgeKind::References, "a.ts"),
    ];
    store.upsert_nodes(&nodes).await.expect("nodes");
    store.upsert_edges(&edges).await.expect("edges");
    store
}

async fn ids(store: &MockGraphStore, query: &NeighbourQuery) -> Vec<String> {
    let mut out: Vec<String> = neighbours(store, REPO, query)
        .await
        .expect("traverses")
        .nodes
        .into_iter()
        .map(|n| n.id)
        .collect();
    out.sort();
    out
}

#[tokio::test]
async fn one_hop_returns_the_immediate_neighbourhood() {
    let store = chain().await;
    let query = NeighbourQuery::new(["b.ts".to_string()]);
    assert_eq!(ids(&store, &query).await, ["a.ts", "b.ts", "c.ts"]);
}

#[tokio::test]
async fn hops_bound_how_far_the_walk_goes() {
    let store = chain().await;
    let query = NeighbourQuery::new(["a.ts".to_string()]).hops(2);
    assert_eq!(ids(&store, &query).await, ["a.ts", "b.ts", "c.ts", "z.ts"]);
}

#[tokio::test]
async fn traversal_follows_edges_backwards_as_well_as_forwards() {
    let store = chain().await;
    // Nothing points out of `d`; the whole value of the walk here is finding
    // what points *in*, because that is what a change to `d` breaks.
    let query = NeighbourQuery::new(["d.ts".to_string()]);
    assert_eq!(ids(&store, &query).await, ["c.ts", "d.ts"]);
}

#[tokio::test]
async fn the_kind_filter_excludes_edges_it_is_not_given() {
    let store = chain().await;
    let query = NeighbourQuery::new(["a.ts".to_string()]).kinds([EdgeKind::Calls]);
    assert_eq!(ids(&store, &query).await, ["a.ts", "b.ts"]);
}

#[tokio::test]
async fn seeds_that_do_not_exist_are_skipped_rather_than_fatal() {
    let store = chain().await;
    // A pull request that adds a file names a path the graph has never seen.
    let query = NeighbourQuery::new(["brand-new.ts".to_string(), "b.ts".to_string()]);
    assert_eq!(ids(&store, &query).await, ["a.ts", "b.ts", "c.ts"]);
}

#[tokio::test]
async fn an_empty_seed_set_returns_nothing_without_asking_the_store() {
    let store = MockGraphStore::new();
    let query = NeighbourQuery::new([]);
    assert_eq!(
        neighbours(&store, REPO, &query).await.expect("traverses"),
        Neighbourhood::default()
    );
}

#[tokio::test]
async fn the_node_cap_is_hard() {
    let store = chain().await;
    let query = NeighbourQuery::new(["a.ts".to_string()])
        .hops(4)
        .max_nodes(2);
    let hood = neighbours(&store, REPO, &query).await.expect("traverses");
    assert_eq!(hood.nodes.len(), 2);
}

#[tokio::test]
async fn a_zero_cap_returns_nothing() {
    let store = chain().await;
    let query = NeighbourQuery::new(["a.ts".to_string()]).max_nodes(0);
    assert_eq!(
        neighbours(&store, REPO, &query).await.expect("traverses"),
        Neighbourhood::default()
    );
}

#[test]
fn capping_keeps_the_nodes_closest_to_the_seeds() {
    let nodes: Vec<GraphNode> = ["a", "b", "c", "d"]
        .iter()
        .map(|n| GraphNode::file(REPO, format!("{n}.ts")))
        .collect();
    let edges = vec![
        GraphEdge::new(REPO, "a.ts", "b.ts", EdgeKind::Calls, "a.ts"),
        GraphEdge::new(REPO, "b.ts", "c.ts", EdgeKind::Calls, "b.ts"),
        GraphEdge::new(REPO, "c.ts", "d.ts", EdgeKind::Calls, "c.ts"),
    ];
    let hood = Neighbourhood { nodes, edges };

    let capped = cap(hood, &["a.ts".to_string()], 2);
    let ids: Vec<&str> = capped.nodes.iter().map(|n| n.id.as_str()).collect();
    // Breadth-first from the seed: the far end of the chain is what goes.
    assert_eq!(ids, ["a.ts", "b.ts"]);
}

#[test]
fn capping_drops_edges_whose_endpoints_it_dropped() {
    let nodes: Vec<GraphNode> = ["a", "b", "c"]
        .iter()
        .map(|n| GraphNode::file(REPO, format!("{n}.ts")))
        .collect();
    let edges = vec![
        GraphEdge::new(REPO, "a.ts", "b.ts", EdgeKind::Calls, "a.ts"),
        GraphEdge::new(REPO, "b.ts", "c.ts", EdgeKind::Calls, "b.ts"),
    ];
    let capped = cap(Neighbourhood { nodes, edges }, &["a.ts".to_string()], 2);
    // An edge pointing at a node that is not in the result is a dangling
    // reference in whatever prompt renders it.
    assert_eq!(capped.edges.len(), 1);
    assert_eq!(capped.edges[0].to, "b.ts");
}

#[test]
fn capping_below_the_limit_changes_nothing() {
    let nodes = vec![GraphNode::file(REPO, "a.ts")];
    let hood = Neighbourhood {
        nodes,
        edges: Vec::new(),
    };
    assert_eq!(cap(hood.clone(), &["a.ts".to_string()], 10), hood);
}

#[test]
fn isolated_nodes_still_fill_the_remaining_room() {
    let nodes: Vec<GraphNode> = ["a", "lonely"]
        .iter()
        .map(|n| GraphNode::file(REPO, format!("{n}.ts")))
        .collect();
    let capped = cap(
        Neighbourhood {
            nodes,
            edges: Vec::new(),
        },
        &["a.ts".to_string()],
        5,
    );
    assert_eq!(capped.nodes.len(), 2);
}

#[test]
fn the_defaults_are_deliberately_small() {
    let query = NeighbourQuery::new(["a.ts".to_string()]);
    assert_eq!(query.hops, DEFAULT_HOPS);
    assert_eq!(query.max_nodes, DEFAULT_MAX_NODES);
    assert_eq!(query.kinds, EdgeKind::ALL.to_vec());
}
