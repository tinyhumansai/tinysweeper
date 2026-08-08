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

#[cfg(test)]
mod tests {
    use super::*;

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
