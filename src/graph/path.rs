//! Repo-relative path arithmetic.
//!
//! Always compiled. Split out from resolution because every language's
//! resolver needs the same two operations — join a possibly-`..` fragment onto
//! a directory, and normalise the result — and having each one open-code them
//! is how a graph ends up with `src/a/../b/c.ts` and `src/b/c.ts` as two
//! different nodes.

/// Collapse `.` and `..` segments and duplicate slashes.
///
/// A leading `..` that would escape the repository root is dropped rather than
/// kept: nothing above the root can be a node, and keeping the segment would
/// only produce an id no file ever matches.
pub fn normalise(path: &str) -> String {
    let mut segments: Vec<&str> = Vec::new();
    for segment in path.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                segments.pop();
            }
            other => segments.push(other),
        }
    }
    segments.join("/")
}

/// Join `fragment` onto the directory `dir`, then normalise.
///
/// An absolute-looking `fragment` (leading `/`) is treated as repo-root
/// relative, because that is what a bundler alias like `/src/x` means.
pub fn join_relative(dir: &str, fragment: &str) -> String {
    if let Some(rooted) = fragment.strip_prefix('/') {
        return normalise(rooted);
    }
    if dir.is_empty() {
        return normalise(fragment);
    }
    normalise(&format!("{dir}/{fragment}"))
}

/// The directory containing `path`, `""` at the repository root.
pub fn dir_of(path: &str) -> String {
    match path.rfind('/') {
        Some(index) => path[..index].to_string(),
        None => String::new(),
    }
}

/// The final segment of `path`, with any extension removed.
pub fn stem(path: &str) -> &str {
    let name = match path.rfind('/') {
        Some(index) => &path[index + 1..],
        None => path,
    };
    match name.rfind('.') {
        Some(index) if index > 0 => &name[..index],
        _ => name,
    }
}

/// Whether a path is a test file by the conventions its ecosystem enforces.
///
/// Only conventions a *tool* acts on are listed, and that restraint is the
/// point. `go test` will not run a file that is not `_test.go`; pytest collects
/// `test_*.py` and `*_test.py`; Jest and Vitest match `.test.` and `.spec.`.
/// Guessing beyond that — treating `helpers.rs` next to a test as a test —
/// would attach coverage edges to code nothing exercises, and a review that
/// reports a change as covered when it is not is worse than one that says
/// nothing.
///
/// A directory named `test` or `tests` counts because all four ecosystems use
/// it and none of them requires the files inside to be named anything.
pub fn is_test_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    if lower
        .split('/')
        .any(|segment| segment == "test" || segment == "tests" || segment == "__tests__")
    {
        return true;
    }
    let name = match lower.rfind('/') {
        Some(index) => &lower[index + 1..],
        None => &lower,
    };
    // `.test.` / `.spec.` before the extension covers the whole JS family in
    // one check, including `.test.tsx` and `.spec.mjs`.
    name.contains(".test.")
        || name.contains(".spec.")
        || name.ends_with("_test.go")
        || name.ends_with("_test.rs")
        || name.ends_with("_test.py")
        || name.starts_with("test_")
        || name.starts_with("conftest.")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_paths_follow_the_conventions_the_toolchains_enforce() {
        for path in [
            "src/graph/build_test.rs",
            "pkg/store/store_test.go",
            "app/tests/test_ledger.py",
            "src/lib/math.test.ts",
            "web/__tests__/button.spec.tsx",
            "tests/e2e/checkout.ts",
        ] {
            assert!(is_test_path(path), "{path}");
        }
    }

    #[test]
    fn ordinary_source_is_not_a_test_however_much_it_says_test() {
        // The trap this guards: a name *containing* "test" is not a test file,
        // and treating it as one attaches coverage edges to production code.
        for path in [
            "src/graph/build.rs",
            "src/testing/harness.rs",
            "src/latest.go",
            "src/contest.py",
        ] {
            assert!(!is_test_path(path), "{path}");
        }
    }

    #[test]
    fn normalise_collapses_dot_segments() {
        assert_eq!(normalise("src/a/../b/c.ts"), "src/b/c.ts");
        assert_eq!(normalise("./src//a.ts"), "src/a.ts");
        assert_eq!(normalise("a/b/../../c"), "c");
    }

    #[test]
    fn normalise_drops_escapes_above_the_root() {
        assert_eq!(normalise("../../outside.ts"), "outside.ts");
    }

    #[test]
    fn join_relative_handles_parents_and_roots() {
        assert_eq!(join_relative("src/app", "../lib/math"), "src/lib/math");
        assert_eq!(join_relative("src/app", "./util"), "src/app/util");
        assert_eq!(join_relative("src/app", "/src/lib/x"), "src/lib/x");
        assert_eq!(join_relative("", "lib/x"), "lib/x");
    }

    #[test]
    fn dir_and_stem_split_a_path() {
        assert_eq!(dir_of("src/lib/math.ts"), "src/lib");
        assert_eq!(dir_of("main.go"), "");
        assert_eq!(stem("src/lib/math.ts"), "math");
        assert_eq!(stem("src/lib/mod.rs"), "mod");
        assert_eq!(stem("Makefile"), "Makefile");
        assert_eq!(stem(".gitignore"), ".gitignore");
    }
}
