//! Folding paths into components.
//!
//! Always compiled; pure functions over strings, so the whole grouping rule is
//! testable without a diff, a graph or a forge.
//!
//! The rule is one sentence: **a component is a directory prefix, at the
//! deepest level that still fits in the diagram.** Depth is chosen from the
//! paths rather than fixed, because `src/lanes` and `packages/web/src/app/api`
//! are the same kind of thing in two repositories with very different layouts,
//! and a hard-coded depth is right for exactly one of them.

use std::collections::BTreeSet;

/// The component name for a file with no directory at all.
///
/// Spelled with parentheses so it cannot collide with a real directory: a
/// repository may well have a folder called `root`, and it must not merge with
/// its top-level files.
pub const ROOT: &str = "(root)";

/// The component a path belongs to at `depth` directory segments.
pub fn component_of(path: &str, depth: usize) -> String {
    let normalised = path.trim_start_matches("./");
    let Some((dir, _file)) = normalised.rsplit_once('/') else {
        return ROOT.to_string();
    };
    let segments: Vec<&str> = dir.split('/').filter(|s| !s.is_empty()).collect();
    if segments.is_empty() {
        return ROOT.to_string();
    }
    segments[..depth.min(segments.len()).max(1)].join("/")
}

/// The deepest grouping of `paths` that yields at most `max` components.
///
/// Deepest rather than shallowest: depth is detail, and `src` as a single box
/// says nothing about a change that spans four subsystems inside it. Falls back
/// to 1 when even top-level directories overflow — the caller folds the tail
/// away and says how many it folded.
pub fn choose_depth(paths: impl IntoIterator<Item = impl AsRef<str>>, max: usize) -> usize {
    let paths: Vec<String> = paths.into_iter().map(|p| p.as_ref().to_string()).collect();
    let deepest = paths
        .iter()
        .map(|p| p.matches('/').count())
        .max()
        .unwrap_or(0)
        .max(1);

    for depth in (1..=deepest).rev() {
        let distinct: BTreeSet<String> = paths
            .iter()
            .map(|path| component_of(path, depth))
            .collect();
        if distinct.len() <= max.max(1) {
            return depth;
        }
    }
    1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_top_level_file_is_its_own_component() {
        assert_eq!(component_of("README.md", 2), ROOT);
        assert_eq!(component_of("./Cargo.toml", 1), ROOT);
    }

    #[test]
    fn a_path_shallower_than_the_depth_keeps_what_it_has() {
        assert_eq!(component_of("src/lib.rs", 3), "src");
    }

    #[test]
    fn depth_truncates_from_the_left() {
        assert_eq!(component_of("packages/web/src/app/page.tsx", 2), "packages/web");
        assert_eq!(component_of("packages/web/src/app/page.tsx", 4), "packages/web/src/app");
    }

    #[test]
    fn depth_zero_still_names_a_directory() {
        // A caller that computes a depth of zero gets the top-level directory,
        // not an empty string that would render as a nameless box.
        assert_eq!(component_of("src/lanes/mod.rs", 0), "src");
    }

    #[test]
    fn the_chosen_depth_is_the_deepest_that_fits() {
        let paths = [
            "src/lanes/critique.rs",
            "src/lanes/security.rs",
            "src/graph/build.rs",
        ];
        // Depth 2 gives {src/lanes, src/graph}; depth 1 would give {src} and
        // lose the distinction the reviewer needs.
        assert_eq!(choose_depth(paths, 8), 2);
    }

    #[test]
    fn a_tight_budget_collapses_to_the_top_level() {
        let paths = [
            "src/lanes/critique.rs",
            "src/graph/build.rs",
            "src/forge/github.rs",
        ];
        assert_eq!(choose_depth(paths, 2), 1);
    }

    #[test]
    fn an_overflowing_top_level_still_returns_a_usable_depth() {
        let paths = ["a/x.rs", "b/x.rs", "c/x.rs", "d/x.rs"];
        assert_eq!(choose_depth(paths, 1), 1, "never returns zero");
    }

    #[test]
    fn no_paths_is_not_a_panic() {
        assert_eq!(choose_depth(Vec::<String>::new(), 8), 1);
    }
}
