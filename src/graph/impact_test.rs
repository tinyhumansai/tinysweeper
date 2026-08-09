//! Blast-radius tests.
//!
//! The two that matter are [`a_callee_is_context_but_not_blast_radius`] — the
//! direction rule this module exists for — and
//! [`a_symbol_the_graph_never_saw_is_not_reported_as_untested`], which is the
//! dishonest answer it would otherwise be easy to give.

use super::*;

use crate::index::types::{GraphEdge, GraphNode};

const REPO: &str = "tinyhumansai/example";

fn edge(from: &str, to: &str, kind: EdgeKind) -> GraphEdge {
    GraphEdge::new(REPO, from, to, kind, from.split('#').next().unwrap_or(from))
}

fn symbol(id: &str) -> GraphNode {
    let (path, name) = id.split_once('#').expect("a symbol id");
    GraphNode::symbol(REPO, path, name)
}

fn hood(nodes: &[GraphNode], edges: &[GraphEdge]) -> Neighbourhood {
    Neighbourhood {
        nodes: nodes.to_vec(),
        edges: edges.to_vec(),
    }
}

fn ids(impact: &Impact) -> Vec<&str> {
    impact.reached.iter().map(|e| e.id.as_str()).collect()
}

#[test]
fn a_callee_is_context_but_not_blast_radius() {
    // `settle` was changed. `render` calls it, so `render` may break.
    // `settle` calls `log`, and nothing about `log` changed.
    let seeds = vec!["src/ledger.rs#settle".to_string()];
    let neighbourhood = hood(
        &[symbol("src/ledger.rs#settle")],
        &[
            edge("src/app.rs#render", "src/ledger.rs#settle", EdgeKind::Calls),
            edge("src/ledger.rs#settle", "src/log.rs#log", EdgeKind::Calls),
        ],
    );

    let impact = Impact::of(&neighbourhood, &seeds, DEFAULT_MAX_REACHED);
    assert_eq!(ids(&impact), ["src/app.rs#render"]);
}

#[test]
fn a_test_that_covers_a_changed_symbol_outranks_the_call_it_made() {
    // The coverage edge accompanies a call edge by construction. Reporting the
    // same node twice would spend the cap saying one thing.
    let seeds = vec!["src/lib/math.ts#total".to_string()];
    let neighbourhood = hood(
        &[symbol("src/lib/math.ts#total")],
        &[
            edge(
                "src/lib/math.test.ts",
                "src/lib/math.ts#total",
                EdgeKind::Calls,
            ),
            edge(
                "src/lib/math.test.ts",
                "src/lib/math.ts#total",
                EdgeKind::Tests,
            ),
        ],
    );

    let impact = Impact::of(&neighbourhood, &seeds, DEFAULT_MAX_REACHED);
    assert_eq!(impact.reached.len(), 1);
    assert_eq!(impact.reached[0].relation, Relation::Test);
    assert!(impact.untested.is_empty(), "{:?}", impact.untested);
}

#[test]
fn a_changed_symbol_no_test_reaches_is_reported_as_untested() {
    let seeds = vec!["src/ledger.rs#settle".to_string()];
    let neighbourhood = hood(
        &[symbol("src/ledger.rs#settle")],
        &[edge(
            "src/app.rs#render",
            "src/ledger.rs#settle",
            EdgeKind::Calls,
        )],
    );

    let impact = Impact::of(&neighbourhood, &seeds, DEFAULT_MAX_REACHED);
    assert_eq!(impact.untested, ["src/ledger.rs#settle"]);
    assert!(impact.render().contains("no test in the index exercises"));
}

#[test]
fn a_symbol_the_graph_never_saw_is_not_reported_as_untested() {
    // A pull request that adds a file names symbols the index has never held,
    // and a heading-derived seed can name nothing at all. Calling either one
    // "untested" tells a reviewer we looked and found no coverage, when what
    // happened is that we never looked at it.
    let seeds = vec![
        "src/brand/new.rs#fresh".to_string(),
        "src/ledger.rs".to_string(),
    ];
    let impact = Impact::of(&hood(&[], &[]), &seeds, DEFAULT_MAX_REACHED);
    assert!(impact.untested.is_empty(), "{:?}", impact.untested);
    assert!(impact.is_empty());
    assert!(impact.render().is_empty());
}

#[test]
fn a_file_seed_is_never_untested_however_thoroughly_nothing_covers_it() {
    // Coverage is a claim about symbols. "No test exercises `src/ledger.rs`"
    // would be true of a file whose every function is tested individually.
    let seeds = vec!["src/ledger.rs".to_string()];
    let neighbourhood = hood(
        &[GraphNode::file(REPO, "src/ledger.rs")],
        &[edge("src/app.rs", "src/ledger.rs", EdgeKind::Imports)],
    );

    let impact = Impact::of(&neighbourhood, &seeds, DEFAULT_MAX_REACHED);
    assert!(impact.untested.is_empty());
    assert_eq!(impact.reached[0].relation, Relation::Importer);
}

#[test]
fn another_changed_file_is_not_its_own_blast_radius() {
    // Both files are in the diff, so both are already in front of the reviewer.
    let seeds = vec!["src/a.rs#one".to_string(), "src/b.rs#two".to_string()];
    let neighbourhood = hood(
        &[symbol("src/a.rs#one"), symbol("src/b.rs#two")],
        &[edge("src/b.rs#two", "src/a.rs#one", EdgeKind::Calls)],
    );

    assert!(
        Impact::of(&neighbourhood, &seeds, DEFAULT_MAX_REACHED)
            .reached
            .is_empty()
    );
}

#[test]
fn a_test_changed_in_the_same_push_still_counts_as_coverage() {
    // The function and the test that exercises it changed together, so both are
    // seeds. The test earns no `reached` row — it is already in front of the
    // reviewer — but it still covers the function, and reporting that function
    // as untested because of a shared push would be wrong.
    let seeds = vec![
        "src/lib/math.test.ts".to_string(),
        "src/lib/math.ts#total".to_string(),
    ];
    let neighbourhood = hood(
        &[symbol("src/lib/math.ts#total")],
        &[edge(
            "src/lib/math.test.ts",
            "src/lib/math.ts#total",
            EdgeKind::Tests,
        )],
    );

    let impact = Impact::of(&neighbourhood, &seeds, DEFAULT_MAX_REACHED);
    assert!(
        impact.reached.is_empty(),
        "seeds are excluded from reached: {:?}",
        impact.reached
    );
    assert!(
        impact.untested.is_empty(),
        "a same-diff test covers it: {:?}",
        impact.untested
    );
}

#[test]
fn the_cap_drops_importers_before_it_drops_a_test() {
    let seeds = vec!["src/ledger.rs#settle".to_string()];
    let mut edges = vec![edge(
        "src/ledger_test.rs#settles",
        "src/ledger.rs#settle",
        EdgeKind::Tests,
    )];
    for n in 0..40 {
        edges.push(edge(
            &format!("src/consumer{n:02}.rs"),
            "src/ledger.rs#settle",
            EdgeKind::Imports,
        ));
    }
    let neighbourhood = hood(&[symbol("src/ledger.rs#settle")], &edges);

    let impact = Impact::of(&neighbourhood, &seeds, 5);
    assert_eq!(impact.reached.len(), 5);
    assert_eq!(impact.reached[0].relation, Relation::Test);
    assert_eq!(impact.truncated, 36);
    assert!(impact.render().contains("and 36 more not shown"));
}

#[test]
fn an_implementor_of_a_changed_trait_is_in_the_radius() {
    let seeds = vec!["src/ports/store.rs#Store".to_string()];
    let neighbourhood = hood(
        &[symbol("src/ports/store.rs#Store")],
        &[edge(
            "src/mongo.rs#Mongo",
            "src/ports/store.rs#Store",
            EdgeKind::Extends,
        )],
    );

    let impact = Impact::of(&neighbourhood, &seeds, DEFAULT_MAX_REACHED);
    assert_eq!(impact.reached[0].relation, Relation::Implementor);
    assert!(
        impact
            .render()
            .contains("implemented by src/mongo.rs#Mongo")
    );
}

#[test]
fn the_file_that_defines_a_changed_symbol_is_not_its_dependent() {
    let seeds = vec!["src/ledger.rs#settle".to_string()];
    let neighbourhood = hood(
        &[symbol("src/ledger.rs#settle")],
        &[edge(
            "src/ledger.rs",
            "src/ledger.rs#settle",
            EdgeKind::Defines,
        )],
    );

    assert!(
        Impact::of(&neighbourhood, &seeds, DEFAULT_MAX_REACHED)
            .reached
            .is_empty()
    );
}

#[test]
fn a_heavily_tested_symbol_does_not_crowd_out_the_code_that_would_break() {
    // The shape that made strict priority order wrong: twenty-six test callers
    // and three production ones. Ordering by relation alone spends the whole
    // block on tests and never names the code a mistake would break.
    let seeds = vec!["src/app/apply.rs#apply".to_string()];
    let mut edges = Vec::new();
    for n in 0..26 {
        edges.push(edge(
            &format!("src/app/apply.rs#covers_case_{n:02}"),
            "src/app/apply.rs#apply",
            EdgeKind::Tests,
        ));
    }
    for n in 0..3 {
        edges.push(edge(
            &format!("src/server/routes{n}.rs#handle"),
            "src/app/apply.rs#apply",
            EdgeKind::Calls,
        ));
    }
    let neighbourhood = hood(&[symbol("src/app/apply.rs#apply")], &edges);

    let impact = Impact::of(&neighbourhood, &seeds, 8);
    let callers = impact
        .reached
        .iter()
        .filter(|e| e.relation == Relation::Caller)
        .count();
    assert_eq!(callers, 3, "{:?}", impact.reached);
    assert_eq!(impact.reached.len(), 8);
    assert_eq!(impact.truncated, 21);
}

#[test]
fn a_relation_with_nothing_left_gives_its_share_to_the_others() {
    // Round-robin must not reserve room for a relation that has run out, or a
    // change with one test and forty callers would list one of each.
    let seeds = vec!["src/ledger.rs#settle".to_string()];
    let mut edges = vec![edge(
        "src/ledger_test.rs#settles",
        "src/ledger.rs#settle",
        EdgeKind::Tests,
    )];
    for n in 0..10 {
        edges.push(edge(
            &format!("src/caller{n:02}.rs#use_it"),
            "src/ledger.rs#settle",
            EdgeKind::Calls,
        ));
    }
    let neighbourhood = hood(&[symbol("src/ledger.rs#settle")], &edges);

    let impact = Impact::of(&neighbourhood, &seeds, 6);
    assert_eq!(impact.reached.len(), 6);
    assert_eq!(
        impact
            .reached
            .iter()
            .filter(|e| e.relation == Relation::Caller)
            .count(),
        5
    );
}
