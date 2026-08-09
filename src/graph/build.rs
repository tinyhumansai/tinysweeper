//! Assembling parsed files and resolved specifiers into nodes and edges.
//!
//! Always compiled.
//!
//! Two passes, and the second one is why. Pass one parses every file and
//! collects the symbols it defines; pass two resolves usages, because `foo()`
//! in file A can only be attributed to file B once we know B defines `foo`.
//! A single-pass builder would have to guess, and a wrong `calls` edge sends
//! retrieval confidently into the wrong file.

use std::collections::{BTreeMap, BTreeSet};

use crate::error::Result;
use crate::graph::extract::parse;
use crate::graph::resolve::{Resolution, Resolver};
use crate::graph::types::{
    Coverage, ParsedFile, RepoGraph, SourceFile, Unresolved, UnresolvedReason,
};
use crate::index::types::{EdgeKind, GraphEdge, GraphNode, NodeKind};
use crate::ports::graph::GraphStore;

/// Build the graph for a repository from its files.
///
/// `files` should be the whole tree, not only the parseable part: the
/// configuration files carrying path aliases are what make the internal edges
/// resolvable at all.
pub fn build(repo_id: &str, files: &[SourceFile]) -> Result<RepoGraph> {
    build_inner(repo_id, files, None, &[])
}

/// Build the graph for only `paths`, resolving against the rest of the tree.
///
/// The incremental counterpart to [`build`], and the two arguments it adds are
/// both there for the same reason: parsing a subset of a repository answers
/// repository-wide questions for the subset and nowhere else.
///
/// * `files` is still the **whole tree**. Text is only read for what will be
///   parsed and for the alias configuration; every other entry may carry an
///   empty body, because what resolution needs from an untouched file is that
///   its path exists.
/// * `known` is the stored symbol table — [`GraphStore::symbols`] — which is
///   what lets a call in a re-parsed file resolve into a file that was not
///   re-parsed. Entries for `paths` are ignored in favour of what was just
///   parsed, so a deleted symbol does not resurrect itself.
///
/// [`RepoGraph::coverage`] describes the parsed subset, not the repository. A
/// partial run's resolution rate is not the repository's, and reporting it as
/// though it were would make the one metric that says whether the resolver
/// works depend on which files a push happened to touch.
pub fn build_paths(
    repo_id: &str,
    files: &[SourceFile],
    paths: &[String],
    known: &[GraphNode],
) -> Result<RepoGraph> {
    let wanted: BTreeSet<&str> = paths.iter().map(String::as_str).collect();
    build_inner(repo_id, files, Some(&wanted), known)
}

fn build_inner(
    repo_id: &str,
    files: &[SourceFile],
    only: Option<&BTreeSet<&str>>,
    known: &[GraphNode],
) -> Result<RepoGraph> {
    let resolver = Resolver::new(files);

    let mut parsed: Vec<ParsedFile> = Vec::new();
    for file in files {
        if only.is_some_and(|only| !only.contains(file.path.as_str())) {
            continue;
        }
        if let Some(one) = parse(file)? {
            parsed.push(one);
        }
    }

    // Repo-wide symbol table, built before any usage is resolved.
    //
    // Stored symbols go in first and freshly parsed ones after, so a file that
    // was re-parsed contributes only what it defines *now*. Seeding from the
    // store in the other order would leave a renamed function defined under
    // both names until the next full rebuild.
    let mut defined_in: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let reparsed: BTreeSet<&str> = parsed.iter().map(|file| file.path.as_str()).collect();
    for node in known {
        let Some(symbol) = &node.symbol else { continue };
        if reparsed.contains(node.path.as_str()) {
            continue;
        }
        defined_in
            .entry(symbol.clone())
            .or_default()
            .insert(node.path.clone());
    }
    for file in &parsed {
        for definition in &file.defs {
            defined_in
                .entry(definition.name.clone())
                .or_default()
                .insert(file.path.clone());
        }
    }

    let mut nodes: BTreeMap<String, GraphNode> = BTreeMap::new();
    let mut edges: BTreeMap<String, GraphEdge> = BTreeMap::new();
    let mut unresolved: Vec<Unresolved> = Vec::new();
    let mut coverage = Coverage::default();

    for file in &parsed {
        let mut file_node = GraphNode::file(repo_id, &file.path);
        file_node.lang = Some(file.lang.tag().to_string());
        nodes.insert(file_node.id.clone(), file_node);

        let local: BTreeSet<&str> = file.defs.iter().map(|d| d.name.as_str()).collect();

        for definition in &file.defs {
            let mut node = GraphNode::symbol(repo_id, &file.path, &definition.name);
            node.lang = Some(file.lang.tag().to_string());
            let edge = GraphEdge::new(repo_id, &file.path, &node.id, EdgeKind::Defines, &file.path);
            edges.insert(edge.id(), edge);
            nodes.insert(node.id.clone(), node);
        }

        // Local name -> files it was imported from. This is what lets a bare
        // `computeTotal()` become an edge into the aliased file rather than a
        // repo-wide name guess.
        let mut bindings: BTreeMap<String, Vec<String>> = BTreeMap::new();

        for import in &file.imports {
            coverage.imports_total += 1;
            let resolution = resolver.resolve(&file.path, file.lang, &import.specifier);
            match &resolution {
                Resolution::Resolved(targets) => {
                    coverage.imports_resolved += 1;
                    for target in targets {
                        if *target == file.path {
                            continue;
                        }
                        let edge = GraphEdge::new(
                            repo_id,
                            &file.path,
                            target,
                            EdgeKind::Imports,
                            &file.path,
                        );
                        edges.insert(edge.id(), edge);
                        let bound = if import.names.is_empty() {
                            // A whole-module import binds nothing by name, but
                            // the file is still reachable, so record it under
                            // the module's own last segment for `pkg.Fn()`.
                            vec![last_segment(&import.specifier)]
                        } else {
                            import.names.clone()
                        };
                        for name in bound {
                            bindings.entry(name).or_default().push(target.clone());
                        }
                    }
                }
                Resolution::Unresolved(reason) => {
                    if *reason == UnresolvedReason::External {
                        coverage.imports_external += 1;
                    }
                    unresolved.push(Unresolved {
                        path: file.path.clone(),
                        specifier: import.specifier.clone(),
                        reason: *reason,
                    });
                }
            }
        }

        for usage in &file.usages {
            let imported = bindings.contains_key(&usage.name);
            let known = defined_in.contains_key(&usage.name);
            // Neither imported nor defined anywhere we can see: a local
            // variable, a builtin, a field. Not a graph fact, and counting it
            // would drown the coverage numbers in noise.
            if !imported && !known {
                continue;
            }
            coverage.usages_total += 1;

            let source = match file.enclosing(usage.byte) {
                Some(definition) => format!("{}#{}", file.path, definition.name),
                None => file.path.clone(),
            };
            let kind = if usage.call {
                EdgeKind::Calls
            } else {
                EdgeKind::References
            };

            // Ambiguous usages are counted, not listed. On any real repository
            // `new`, `fmt` and `len` are defined in dozens of files, so listing
            // each one would bury the handful of genuinely broken *imports*
            // that `unresolved` exists to make findable. The gap is still
            // measurable as `usages_total - usages_resolved`.
            let Some(target) = target_for(&file.path, &usage.name, &local, &bindings, &defined_in)
            else {
                continue;
            };
            coverage.usages_resolved += 1;
            if target == source {
                continue;
            }
            let edge = GraphEdge::new(repo_id, &source, &target, kind, &file.path);
            edges.insert(edge.id(), edge);

            // A call made from a test scope into code that is not itself test
            // code is coverage. Derived from the resolved call rather than
            // asserted from the file name, so `foo_test.rs` claims to cover
            // exactly what it actually reaches. References are excluded: a
            // test that names a type does not exercise it.
            if kind == EdgeKind::Calls
                && file.in_test_scope(usage.byte)
                && !crate::graph::path::is_test_path(path_of(&target))
            {
                let covers = GraphEdge::new(repo_id, &source, &target, EdgeKind::Tests, &file.path);
                edges.insert(covers.id(), covers);
            }
        }

        for relation in &file.heritage {
            // Both ends are resolved the same way a usage is: a Rust
            // `impl Display for Ledger` names two types and declares neither,
            // so assuming the child is local would attach the edge to a symbol
            // that file does not define.
            let (Some(child), Some(parent)) = (
                target_for(&file.path, &relation.child, &local, &bindings, &defined_in),
                target_for(&file.path, &relation.parent, &local, &bindings, &defined_in),
            ) else {
                continue;
            };
            if child == parent {
                continue;
            }
            // Child to parent, so walking *inbound* from a changed base class
            // lists what implements it — the direction a review asks in.
            let edge = GraphEdge::new(repo_id, &child, &parent, EdgeKind::Extends, &file.path);
            edges.insert(edge.id(), edge);
        }
    }

    // Every edge endpoint must exist as a node, or a traversal returns edges
    // pointing at nothing. Symbol ids are `path#name`, so the node can be
    // reconstructed from the id alone.
    let endpoints: Vec<String> = edges
        .values()
        .flat_map(|e| [e.from.clone(), e.to.clone()])
        .collect();
    for id in endpoints {
        if nodes.contains_key(&id) {
            continue;
        }
        let node = match id.split_once('#') {
            Some((path, symbol)) => GraphNode::symbol(repo_id, path, symbol),
            None => GraphNode::file(repo_id, &id),
        };
        nodes.insert(id, node);
    }

    Ok(RepoGraph {
        nodes: nodes.into_values().collect(),
        edges: edges.into_values().collect(),
        unresolved,
        coverage,
    })
}

/// Pick the node a usage points at, or `None` when it is genuinely ambiguous.
///
/// Order is deliberate: a definition in the same file shadows an import, an
/// import beats a repo-wide name, and a repo-wide name is only used when it is
/// unique. Anything else is left unresolved rather than guessed.
fn target_for(
    path: &str,
    name: &str,
    local: &BTreeSet<&str>,
    bindings: &BTreeMap<String, Vec<String>>,
    defined_in: &BTreeMap<String, BTreeSet<String>>,
) -> Option<String> {
    if local.contains(name) {
        return Some(format!("{path}#{name}"));
    }
    if let Some(targets) = bindings.get(name) {
        let defining: Vec<&String> = targets
            .iter()
            .filter(|t| defined_in.get(name).is_some_and(|files| files.contains(*t)))
            .collect();
        if let Some(target) = defining.first() {
            return Some(format!("{target}#{name}"));
        }
        // Imported but the target does not define the name itself: a
        // re-export, or a name we cannot see. The file edge is still true.
        if let Some(target) = targets.first() {
            return Some((*target).clone());
        }
    }
    let files = defined_in.get(name)?;
    if files.len() == 1 {
        let target = files.iter().next()?;
        return Some(format!("{target}#{name}"));
    }
    None
}

/// The file half of a node id, which is the whole id for a file node.
fn path_of(id: &str) -> &str {
    match id.split_once('#') {
        Some((path, _)) => path,
        None => id,
    }
}

fn last_segment(specifier: &str) -> String {
    specifier
        .trim_start_matches("mod ")
        .rsplit(['/', ':', '.'])
        .find(|s| !s.is_empty())
        .unwrap_or(specifier)
        .to_string()
}

/// Write a whole repository's graph, replacing whatever was there.
///
/// Used for a first index. Incremental re-indexing goes through
/// [`sync_paths`], which is the path a push actually takes.
pub async fn sync_all(store: &dyn GraphStore, repo_id: &str, graph: &RepoGraph) -> Result<u64> {
    store.delete_repo(repo_id).await?;
    let written =
        store.upsert_nodes(&graph.nodes).await? + store.upsert_edges(&graph.edges).await?;
    Ok(written)
}

/// Write only the parts of `graph` belonging to `paths`, deleting those paths
/// first.
///
/// Delete-then-upsert, in that order and by path, mirroring how the chunk
/// index re-indexes. Skipping the delete would leave the previous revision's
/// symbols behind forever: a renamed function stays in the graph under both
/// names, and traversal keeps handing reviewers a definition that no longer
/// exists.
pub async fn sync_paths(
    store: &dyn GraphStore,
    repo_id: &str,
    graph: &RepoGraph,
    paths: &[String],
) -> Result<u64> {
    if paths.is_empty() {
        return Ok(0);
    }
    store.delete_paths(repo_id, paths).await?;
    let wanted: BTreeSet<&String> = paths.iter().collect();
    let nodes: Vec<GraphNode> = graph
        .nodes
        .iter()
        .filter(|n| wanted.contains(&n.path))
        .cloned()
        .collect();
    let edges: Vec<GraphEdge> = graph
        .edges
        .iter()
        .filter(|e| {
            wanted.contains(&e.path)
                || paths.iter().any(|path| {
                    endpoint_belongs_to(&e.from, path) || endpoint_belongs_to(&e.to, path)
                })
        })
        .cloned()
        .collect();
    Ok(store.upsert_nodes(&nodes).await? + store.upsert_edges(&edges).await?)
}

/// The files an incremental rebuild has to re-parse, given what changed.
///
/// Not just the changed ones, and the reason is
/// [`GraphStore::delete_paths`]: it removes every edge *touching* a path,
/// including the inbound ones written by files that did not change — which are
/// exactly the edges a blast radius is made of. Re-parsing the changed files'
/// existing graph neighbours puts them back. It is still a few dozen files
/// where a full rebuild is thousands.
///
/// The residual gap, stated because it is real and not fixable at this price:
/// a file that did not change and had *no* edge to the changed file gets no new
/// edge either, so a call that only now resolves — because this push added the
/// symbol it names — is missed until either file is touched again. Catching it
/// would mean parsing every file on every push, which is the cost this exists
/// to avoid.
///
/// Removed paths seed the walk, so their dependents are re-parsed and stop
/// pointing at a file that is gone, but are never returned for parsing
/// themselves.
pub async fn rebuild_set(
    store: &dyn GraphStore,
    repo_id: &str,
    changed: &[String],
    removed: &[String],
) -> Result<BTreeSet<String>> {
    let mut seeds: Vec<String> = changed.to_vec();
    seeds.extend(removed.iter().cloned());
    seeds.sort();
    seeds.dedup();

    let mut set: BTreeSet<String> = changed.iter().cloned().collect();
    if seeds.is_empty() {
        return Ok(set);
    }
    let neighbourhood = crate::graph::traverse::walk(
        store,
        repo_id,
        &crate::graph::traverse::NeighbourQuery::new(seeds).hops(1),
    )
    .await?;
    set.extend(neighbourhood.nodes.into_iter().map(|node| node.path));
    for gone in removed {
        set.remove(gone);
    }
    Ok(set)
}

fn endpoint_belongs_to(endpoint: &str, path: &str) -> bool {
    endpoint == path
        || endpoint
            .strip_prefix(path)
            .is_some_and(|suffix| suffix.starts_with('#'))
}

/// The file nodes in a graph, for callers that only want the file layer.
pub fn file_nodes(graph: &RepoGraph) -> Vec<&GraphNode> {
    graph
        .nodes
        .iter()
        .filter(|n| n.kind == NodeKind::File)
        .collect()
}

#[cfg(test)]
#[path = "build_test.rs"]
mod tests;
