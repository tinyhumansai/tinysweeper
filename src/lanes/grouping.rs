//! Deterministic cross-file grouping for the per-file fan-out.
//!
//! **No model call.** `lanes::fanout` reviews one file per conversation, and
//! `harness::prompt::ISOLATION_CLAUSE` tells each conversation to ignore every
//! other file — otherwise N reviewers each notice the same cross-file problem
//! and report it N times. That isolation is also exactly what hides a bug that
//! spans two files: a caller changed in `a.rs`, its callee changed in `b.rs`,
//! or a function and the test that exercises it. Neither reviewer ever sees
//! both halves.
//!
//! This module decides, before any token is spent, which changed files belong
//! in the same conversation. Two changed files are related when:
//!
//! - the code graph has a `Calls`, `References`, `Tests`, `Imports` or
//!   `Extends` edge between a symbol in one and a symbol in the other (see
//!   `graph::impact`, which reads the same edge kinds off the same
//!   [`Neighbourhood`] to answer a related question); or
//! - their paths match a name heuristic that needs no graph at all: a test and
//!   the file it tests, a pair of locale files, or a component and its
//!   co-located stylesheet.
//!
//! Grouping never *grows* the fan-out's isolation clause into meaninglessness:
//! a group that would exceed `GroupBounds` — too many files, or too much
//! rendered diff — falls back to reviewing every one of its members alone,
//! never as a partial group. A group is a bet that one conversation reviews
//! its members better than N separate ones; a bet that would blow the budget
//! or bury the model in one file's diff is not worth making, so it is not
//! made at all. See open-code-review's `grouping.go`, which takes the same
//! position for the same reason.

use std::collections::BTreeMap;

use crate::evidence::diff::{self, FileDiff};
use crate::index::types::{EdgeKind, Neighbourhood};

/// One group of related changed files, reviewed together in one conversation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileGroup {
    /// The paths joined with " + ", or the bare path when there is only one.
    pub label: String,
    /// The paths in this group, in triage order.
    pub paths: Vec<String>,
}

/// The ceiling a component may not cross before it falls back to singletons.
#[derive(Debug, Clone, Copy)]
pub struct GroupBounds {
    /// How many files one conversation may hold.
    pub max_files: usize,
    /// How many characters of rendered hunks one conversation may hold,
    /// summed across every file in the group.
    pub max_hunk_chars: usize,
}

/// Group `paths` deterministically, preserving triage order.
///
/// `paths` is assumed already triaged and ordered — riskiest first — by the
/// caller; this function only decides which of them travel together, never
/// whether or in what priority they are reviewed. `diffs` supplies the hunks
/// to measure and must contain an entry for every path in `paths`.
/// `graph` is the neighbourhood already walked for this pull request's changed
/// files, when one is available; `None` degrades to the name heuristics alone,
/// which is what every offline test and every forge-only review runs with.
pub fn group(
    paths: &[String],
    diffs: &[FileDiff],
    graph: Option<&Neighbourhood>,
    bounds: &GroupBounds,
) -> Vec<FileGroup> {
    if paths.is_empty() {
        return Vec::new();
    }

    let index_of: BTreeMap<&str, usize> = paths
        .iter()
        .enumerate()
        .map(|(i, p)| (p.as_str(), i))
        .collect();

    let mut sets = UnionFind::new(paths.len());

    if let Some(graph) = graph {
        union_graph_neighbours(&mut sets, graph, &index_of);
    }

    for i in 0..paths.len() {
        for j in (i + 1)..paths.len() {
            if are_siblings(&paths[i], &paths[j]) {
                sets.union(i, j);
            }
        }
    }

    let mut components: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for i in 0..paths.len() {
        components.entry(sets.find(i)).or_default().push(i);
    }

    // Triage order is the order `paths` already arrived in, so a component's
    // place in the output is the lowest index any of its members held there —
    // the same rule `select` in `graph::impact` uses to keep a rendered block
    // stable rather than dependent on a map's iteration order.
    let mut ordered: Vec<Vec<usize>> = components.into_values().collect();
    ordered.sort_by_key(|members| members.iter().copied().min().unwrap_or(usize::MAX));

    let mut out = Vec::with_capacity(ordered.len());
    for members in ordered {
        let group_paths: Vec<String> = members.iter().map(|&i| paths[i].clone()).collect();
        if fits(&group_paths, diffs, bounds) {
            out.push(FileGroup {
                label: group_paths.join(" + "),
                paths: group_paths,
            });
        } else {
            // Never a partial group: every member reviewed alone, in the same
            // relative order it held in the oversized component.
            for path in group_paths {
                out.push(FileGroup {
                    label: path.clone(),
                    paths: vec![path],
                });
            }
        }
    }
    out
}

/// Whether `paths` may travel together under `bounds`.
///
/// A component of one file is always within bounds — it is already what the
/// fallback would produce — so this only ever rejects an actual multi-file
/// group.
fn fits(paths: &[String], diffs: &[FileDiff], bounds: &GroupBounds) -> bool {
    if paths.len() > bounds.max_files {
        return false;
    }
    if paths.len() <= 1 {
        return true;
    }
    let total: usize = paths
        .iter()
        .filter_map(|path| diffs.iter().find(|d| &d.path == path))
        .map(|d| diff::render(std::slice::from_ref(d)).len())
        .sum();
    total <= bounds.max_hunk_chars
}

/// Union every pair of changed files a graph edge connects.
///
/// Only the edge kinds that mean "these files' code depends on each other" —
/// the same set `graph::impact::Relation::of` recognises, minus `Defines`,
/// which relates a file to its own symbol rather than to another file.
fn union_graph_neighbours(
    sets: &mut UnionFind,
    graph: &Neighbourhood,
    index_of: &BTreeMap<&str, usize>,
) {
    let path_of: BTreeMap<&str, &str> = graph
        .nodes
        .iter()
        .map(|node| (node.id.as_str(), node.path.as_str()))
        .collect();

    for edge in &graph.edges {
        if !matches!(
            edge.kind,
            EdgeKind::Calls
                | EdgeKind::References
                | EdgeKind::Tests
                | EdgeKind::Imports
                | EdgeKind::Extends
        ) {
            continue;
        }
        let Some(&from_path) = path_of.get(edge.from.as_str()) else {
            continue;
        };
        let Some(&to_path) = path_of.get(edge.to.as_str()) else {
            continue;
        };
        if from_path == to_path {
            continue;
        }
        let (Some(&a), Some(&b)) = (index_of.get(from_path), index_of.get(to_path)) else {
            continue;
        };
        sets.union(a, b);
    }
}

/// Whether `a` and `b` are related by name alone, with no graph.
///
/// Every rule here is deliberately narrow — a false grouping puts one file's
/// problem in front of the wrong file's reviewer just as surely as a missed
/// one hides a cross-file bug, and a name match is far cheaper to get wrong
/// than a graph edge is.
fn are_siblings(a: &str, b: &str) -> bool {
    test_siblings(a, b) || locale_siblings(a, b) || same_stem_different_extension(a, b)
}

/// `foo.rs` ↔ `foo_test.rs` / `foo_tests.rs`, `test_foo.py`, `foo.test.ts` /
/// `foo.spec.ts`, `FooTest.java`.
///
/// Deliberately does not try to infer a test module from a directory name —
/// `tests.rs` living in a `foo/` directory is exactly the ambiguous case that
/// stays out of scope, because a `foo/` directory routinely holds several
/// unrelated files and "the tests file in this directory" is a guess this
/// function is not in a position to make.
fn test_siblings(a: &str, b: &str) -> bool {
    let (dir_a, file_a) = split_dir_file(a);
    let (dir_b, file_b) = split_dir_file(b);
    if dir_a != dir_b {
        return false;
    }

    let (rust_a, rust_b) = (strip_suffix(file_a, ".rs"), strip_suffix(file_b, ".rs"));
    if let (Some(stem_a), Some(stem_b)) = (rust_a, rust_b)
        && (rust_test_stem(stem_a).is_some_and(|base| base == stem_b)
            || rust_test_stem(stem_b).is_some_and(|base| base == stem_a))
    {
        return true;
    }

    let (py_a, py_b) = (strip_suffix(file_a, ".py"), strip_suffix(file_b, ".py"));
    if let (Some(stem_a), Some(stem_b)) = (py_a, py_b)
        && (stem_a
            .strip_prefix("test_")
            .is_some_and(|base| base == stem_b)
            || stem_b
                .strip_prefix("test_")
                .is_some_and(|base| base == stem_a))
    {
        return true;
    }

    for ext in [".ts", ".tsx", ".js", ".jsx"] {
        let (ja, jb) = (strip_suffix(file_a, ext), strip_suffix(file_b, ext));
        if let (Some(stem_a), Some(stem_b)) = (ja, jb)
            && (js_test_stem(stem_a).is_some_and(|base| base == stem_b)
                || js_test_stem(stem_b).is_some_and(|base| base == stem_a))
        {
            return true;
        }
    }

    if let (Some(stem_a), Some(stem_b)) =
        (strip_suffix(file_a, ".java"), strip_suffix(file_b, ".java"))
        && (stem_a
            .strip_suffix("Test")
            .is_some_and(|base| base == stem_b)
            || stem_b
                .strip_suffix("Test")
                .is_some_and(|base| base == stem_a))
    {
        return true;
    }

    false
}

/// The base name of a Rust test-module stem, e.g. `foo_test` → `foo`.
fn rust_test_stem(stem: &str) -> Option<&str> {
    stem.strip_suffix("_tests")
        .or_else(|| stem.strip_suffix("_test"))
}

/// The base name of a JS/TS test-file stem, e.g. `foo.test` → `foo`.
fn js_test_stem(stem: &str) -> Option<&str> {
    stem.strip_suffix(".test")
        .or_else(|| stem.strip_suffix(".spec"))
}

/// Same directory, one of `messages.en.json` / `messages.fr.json`, or two
/// files that are both locale bundles under an `i18n/` or `locales/`
/// directory (`i18n/en.json` ↔ `i18n/fr.json`, where the whole filename is the
/// locale and there is no shared stem to compare).
fn locale_siblings(a: &str, b: &str) -> bool {
    let (dir_a, file_a) = split_dir_file(a);
    let (dir_b, file_b) = split_dir_file(b);
    if dir_a != dir_b || file_a == file_b {
        return false;
    }

    if let (Some((base_a, locale_a, ext_a)), Some((base_b, locale_b, ext_b))) =
        (locale_segment(file_a), locale_segment(file_b))
        && base_a == base_b
        && ext_a == ext_b
        && locale_a != locale_b
    {
        return true;
    }

    let under_locale_dir = dir_a
        .split('/')
        .any(|segment| segment == "i18n" || segment == "locales");
    under_locale_dir
        && extension_of(file_a).is_some()
        && extension_of(file_a) == extension_of(file_b)
        && looks_like_locale(stem_of(file_a))
        && looks_like_locale(stem_of(file_b))
}

/// `file`'s name with its final extension removed, e.g. `en.json` → `en`.
fn stem_of(file: &str) -> &str {
    match file.rsplit_once('.') {
        Some((stem, _ext)) => stem,
        None => file,
    }
}

/// Split `base.locale.ext` into its three parts, when `locale` reads like a
/// locale code — `en`, `fr`, `pt-BR` — rather than an ordinary middle
/// extension such as `module` or `d`.
fn locale_segment(file: &str) -> Option<(&str, &str, &str)> {
    let mut parts: Vec<&str> = file.split('.').collect();
    if parts.len() < 3 {
        return None;
    }
    let ext = parts.pop()?;
    let locale = parts.pop()?;
    if !looks_like_locale(locale) {
        return None;
    }
    // Rejoining handles a base name that itself contains a dot.
    let base_end = file.len() - ext.len() - locale.len() - 2;
    Some((&file[..base_end], locale, ext))
}

/// Two- and three-letter qualifiers that are common build/environment
/// markers, not locale codes, even though they pass the shape check below.
/// `bundle.min.js` and `bundle.dev.js` are unrelated build variants of the
/// same base name, not translations of each other, and must not be grouped
/// as locale siblings.
const ORDINARY_QUALIFIERS: &[&str] = &[
    "min", "dev", "prod", "src", "lib", "bin", "raw", "tmp", "bak", "old", "new", "esm", "cjs",
    "umd", "amd", "doc", "api", "mod", "d",
];

/// Whether `segment` reads as a locale code rather than an ordinary extension
/// segment (`module`, `d`, `min`).
fn looks_like_locale(segment: &str) -> bool {
    let core = segment.split(['-', '_']).next().unwrap_or(segment);
    (2..=3).contains(&core.len())
        && core.chars().all(|c| c.is_ascii_alphabetic())
        && segment
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        && !ORDINARY_QUALIFIERS
            .iter()
            .any(|q| q.eq_ignore_ascii_case(core))
}

/// Same directory, same component name, one a script and the other its
/// stylesheet — `Button.tsx` ↔ `Button.module.css`.
///
/// Deliberately narrower than "same first-dot stem, different extension":
/// that rule also matched `docker-compose.yml` ↔
/// `docker-compose.kernel-bypass.yml`, two independent Compose overlays that
/// happen to share a prefix, and grouped every `.tinysweeper.toml`-adjacent
/// override file with its base by the same accident. A component and its
/// stylesheet is the one shape narrow enough to name outright: a script
/// extension paired with a style extension, nothing else.
fn same_stem_different_extension(a: &str, b: &str) -> bool {
    let (dir_a, file_a) = split_dir_file(a);
    let (dir_b, file_b) = split_dir_file(b);
    if dir_a != dir_b || file_a == file_b {
        return false;
    }
    let root_a = file_a.split('.').next().unwrap_or(file_a);
    let root_b = file_b.split('.').next().unwrap_or(file_b);
    if root_a.is_empty() || root_a != root_b || file_a == root_a || file_b == root_b {
        return false;
    }
    let rest_a = &file_a[root_a.len() + 1..];
    let rest_b = &file_b[root_b.len() + 1..];
    (is_script_extension(rest_a) && is_style_extension(rest_b))
        || (is_script_extension(rest_b) && is_style_extension(rest_a))
}

/// Whether `rest` — the filename after its component root — names a
/// script: `tsx`, or `module.ts` if anyone ever writes one.
///
/// Exact matches only, plus the one qualifier CSS Modules uses: a bare
/// `.ends_with(".ts")` also matched `user.model.ts`, whose middle segment
/// names a different concern entirely rather than opting into a scoped
/// module — `user.model.ts` and `user.profile.css` share a root but are not
/// a component and its stylesheet.
fn is_script_extension(rest: &str) -> bool {
    matches!(
        rest,
        "ts" | "tsx" | "js" | "jsx" | "module.ts" | "module.tsx"
    )
}

/// Whether `rest` names a stylesheet, plain or CSS-Modules-scoped.
///
/// Exact matches only — see [`is_script_extension`] for why a suffix check
/// is not narrow enough.
fn is_style_extension(rest: &str) -> bool {
    matches!(
        rest,
        "css" | "scss" | "less" | "module.css" | "module.scss" | "module.less"
    )
}

/// A path's directory (empty for a bare filename) and its filename.
fn split_dir_file(path: &str) -> (&str, &str) {
    match path.rsplit_once('/') {
        Some((dir, file)) => (dir, file),
        None => ("", path),
    }
}

/// `file` with `suffix` removed from before its own extension, e.g.
/// `strip_suffix("foo_test.rs", ".rs")` → `Some("foo_test")`.
fn strip_suffix<'a>(file: &'a str, ext: &str) -> Option<&'a str> {
    file.strip_suffix(ext)
}

/// The extension of a filename, if it has one.
fn extension_of(file: &str) -> Option<&str> {
    file.rsplit_once('.').map(|(_, ext)| ext)
}

/// A minimal union-find over `0..n`, path-compressed on find.
///
/// No union by rank: these components hold single-digit file counts, so the
/// tree depth a rank heuristic would save is not worth the extra field.
struct UnionFind {
    parent: Vec<usize>,
}

impl UnionFind {
    fn new(n: usize) -> Self {
        Self {
            parent: (0..n).collect(),
        }
    }

    fn find(&mut self, x: usize) -> usize {
        if self.parent[x] != x {
            self.parent[x] = self.find(self.parent[x]);
        }
        self.parent[x]
    }

    fn union(&mut self, a: usize, b: usize) {
        let (ra, rb) = (self.find(a), self.find(b));
        if ra != rb {
            self.parent[ra] = rb;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::evidence::diff::parse_file_patch;
    use crate::index::types::{GraphEdge, GraphNode};

    fn small_diff(path: &str) -> FileDiff {
        parse_file_patch(path, "@@ -1,1 +1,2 @@\n a\n+b\n")
    }

    fn diffs_for(paths: &[&str]) -> Vec<FileDiff> {
        paths.iter().map(|p| small_diff(p)).collect()
    }

    fn strings(paths: &[&str]) -> Vec<String> {
        paths.iter().map(|p| p.to_string()).collect()
    }

    fn default_bounds() -> GroupBounds {
        GroupBounds {
            max_files: 4,
            max_hunk_chars: 20_000,
        }
    }

    #[test]
    fn files_connected_by_a_calls_edge_are_grouped() {
        let paths = strings(&["src/a.rs", "src/b.rs"]);
        let diffs = diffs_for(&["src/a.rs", "src/b.rs"]);

        let mut graph = Neighbourhood::default();
        graph
            .nodes
            .push(GraphNode::symbol("r", "src/a.rs", "caller"));
        graph
            .nodes
            .push(GraphNode::symbol("r", "src/b.rs", "callee"));
        graph.edges.push(GraphEdge::new(
            "r",
            "src/a.rs#caller",
            "src/b.rs#callee",
            EdgeKind::Calls,
            "src/a.rs",
        ));

        let groups = group(&paths, &diffs, Some(&graph), &default_bounds());

        assert_eq!(groups.len(), 1, "{groups:?}");
        assert_eq!(groups[0].paths, vec!["src/a.rs", "src/b.rs"]);
        assert_eq!(groups[0].label, "src/a.rs + src/b.rs");
    }

    #[test]
    fn a_file_and_its_underscore_test_sibling_are_grouped_with_no_graph() {
        let paths = strings(&["src/widget.rs", "src/widget_test.rs"]);
        let diffs = diffs_for(&["src/widget.rs", "src/widget_test.rs"]);

        let groups = group(&paths, &diffs, None, &default_bounds());

        assert_eq!(groups.len(), 1, "{groups:?}");
        assert_eq!(groups[0].paths.len(), 2);
    }

    #[test]
    fn sibling_locale_files_are_grouped() {
        let paths = strings(&["locales/messages.en.json", "locales/messages.fr.json"]);
        let diffs = diffs_for(&["locales/messages.en.json", "locales/messages.fr.json"]);

        let groups = group(&paths, &diffs, None, &default_bounds());

        assert_eq!(groups.len(), 1, "{groups:?}");
        assert_eq!(groups[0].paths.len(), 2);
    }

    #[test]
    fn locale_bundles_named_only_by_their_locale_are_grouped() {
        let paths = strings(&["i18n/en.json", "i18n/fr.json"]);
        let diffs = diffs_for(&["i18n/en.json", "i18n/fr.json"]);

        let groups = group(&paths, &diffs, None, &default_bounds());

        assert_eq!(groups.len(), 1, "{groups:?}");
    }

    #[test]
    fn build_variant_qualifiers_are_not_grouped_as_locale_siblings() {
        let paths = strings(&["dist/bundle.min.js", "dist/bundle.dev.js"]);
        let diffs = diffs_for(&["dist/bundle.min.js", "dist/bundle.dev.js"]);

        let groups = group(&paths, &diffs, None, &default_bounds());

        assert_eq!(groups.len(), 2, "{groups:?}");
        assert!(groups.iter().all(|g| g.paths.len() == 1));
    }

    #[test]
    fn non_locale_shaped_files_under_a_locale_dir_are_not_grouped() {
        let paths = strings(&["locales/schema.json", "locales/config.json"]);
        let diffs = diffs_for(&["locales/schema.json", "locales/config.json"]);

        let groups = group(&paths, &diffs, None, &default_bounds());

        assert_eq!(groups.len(), 2, "{groups:?}");
        assert!(groups.iter().all(|g| g.paths.len() == 1));
    }

    #[test]
    fn a_component_and_its_stylesheet_are_grouped() {
        let paths = strings(&["ui/Button.tsx", "ui/Button.module.css"]);
        let diffs = diffs_for(&["ui/Button.tsx", "ui/Button.module.css"]);

        let groups = group(&paths, &diffs, None, &default_bounds());

        assert_eq!(groups.len(), 1, "{groups:?}");
    }

    #[test]
    fn files_sharing_a_root_but_naming_different_middle_segments_stay_singletons() {
        // Both start with `user.` and end in a script/style extension, but
        // `.model.` and `.profile.` name different concerns, not a component
        // and its CSS-Modules-scoped stylesheet.
        let paths = strings(&["ui/user.model.ts", "ui/user.profile.css"]);
        let diffs = diffs_for(&["ui/user.model.ts", "ui/user.profile.css"]);

        let groups = group(&paths, &diffs, None, &default_bounds());

        assert_eq!(groups.len(), 2, "{groups:?}");
        assert!(groups.iter().all(|g| g.paths.len() == 1));
    }

    #[test]
    fn an_oversized_component_is_split_back_to_singletons() {
        // Six files chained by calls edges: one component of six, which is
        // over `max_files = 4`. The fallback is six singletons, never 4 + 2.
        let names = ["a", "b", "c", "d", "e", "f"];
        let file_paths: Vec<String> = names.iter().map(|n| format!("src/{n}.rs")).collect();
        let diffs = diffs_for(&file_paths.iter().map(String::as_str).collect::<Vec<_>>());

        let mut graph = Neighbourhood::default();
        for path in &file_paths {
            graph.nodes.push(GraphNode::symbol("r", path, "sym"));
        }
        for pair in file_paths.windows(2) {
            graph.edges.push(GraphEdge::new(
                "r",
                format!("{}#sym", pair[0]),
                format!("{}#sym", pair[1]),
                EdgeKind::Calls,
                pair[0].clone(),
            ));
        }

        let groups = group(&file_paths, &diffs, Some(&graph), &default_bounds());

        assert_eq!(groups.len(), 6, "{groups:?}");
        assert!(groups.iter().all(|g| g.paths.len() == 1));
    }

    #[test]
    fn a_component_over_the_char_budget_is_split_even_when_file_count_is_fine() {
        let paths = strings(&["src/big_test.rs", "src/big.rs"]);
        let huge_hunk = format!("@@ -1,1 +1,2 @@\n a\n+{}\n", "x".repeat(30_000));
        let diffs = vec![
            parse_file_patch("src/big_test.rs", &huge_hunk),
            parse_file_patch("src/big.rs", &huge_hunk),
        ];
        let bounds = GroupBounds {
            max_files: 4,
            max_hunk_chars: 20_000,
        };

        let groups = group(&paths, &diffs, None, &bounds);

        assert_eq!(groups.len(), 2, "{groups:?}");
        assert!(groups.iter().all(|g| g.paths.len() == 1));
    }

    #[test]
    fn unrelated_files_stay_singletons() {
        let paths = strings(&["src/a.rs", "src/b.rs", "docs/readme.md"]);
        let diffs = diffs_for(&["src/a.rs", "src/b.rs", "docs/readme.md"]);

        let groups = group(&paths, &diffs, None, &default_bounds());

        assert_eq!(groups.len(), 3);
        assert!(groups.iter().all(|g| g.paths.len() == 1));
    }

    #[test]
    fn group_order_follows_triage_order() {
        // `c.rs` is riskiest and comes first in triage order; `a.rs` and
        // `b.rs` are test siblings that triaged after it. The singleton must
        // still lead the output, exactly as triage ordered it.
        let paths = strings(&["src/c.rs", "src/a.rs", "src/a_test.rs"]);
        let diffs = diffs_for(&["src/c.rs", "src/a.rs", "src/a_test.rs"]);

        let groups = group(&paths, &diffs, None, &default_bounds());

        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].paths, vec!["src/c.rs"]);
        assert_eq!(groups[1].paths, vec!["src/a.rs", "src/a_test.rs"]);
    }

    #[test]
    fn a_single_file_is_its_own_group_labelled_by_its_own_path() {
        let paths = strings(&["src/only.rs"]);
        let diffs = diffs_for(&["src/only.rs"]);

        let groups = group(&paths, &diffs, None, &default_bounds());

        assert_eq!(
            groups,
            vec![FileGroup {
                label: "src/only.rs".to_string(),
                paths: vec!["src/only.rs".to_string()],
            }]
        );
    }
}
