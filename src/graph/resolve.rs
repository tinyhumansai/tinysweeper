//! Module-specifier resolution: turning `@/lib/math` into `src/lib/math.ts`.
//!
//! Always compiled.
//!
//! This is the module that decides whether the graph has edges at all. The
//! naive implementation — accept `./x` and `../x`, drop everything else —
//! produces a graph that is nearly edgeless on any codebase that configures
//! path aliases, which is most of them. So each language gets the resolution
//! rules its own toolchain uses:
//!
//! * TypeScript / JavaScript: relative paths, extension and `index` inference,
//!   `compilerOptions.paths` patterns, and `baseUrl`.
//! * Rust: `crate` / `self` / `super` / own-crate-name prefixes against the
//!   crate source root, `foo.rs` vs `foo/mod.rs`, and bodyless `mod foo;`.
//! * Python: dotted absolute modules and explicit relative imports, with
//!   `__init__.py` packages and src-layout roots.
//! * Go: package directories rooted at the `go.mod` module path.
//!
//! Whatever does not resolve is *reported*, never dropped. A resolver that
//! silently discards its failures scores the same whether it works or not.

use std::collections::{BTreeMap, BTreeSet};

use crate::graph::aliases::AliasConfig;
use crate::graph::path::{dir_of, join_relative, normalise, stem};
use crate::graph::types::{Language, SourceFile, UnresolvedReason};

/// Extensions tried when a TypeScript specifier omits one, in `tsc`'s order.
const TS_EXTENSIONS: &[&str] = &[
    ".ts", ".tsx", ".d.ts", ".mts", ".cts", ".js", ".jsx", ".mjs", ".cjs", ".json",
];

/// Files tried when a TypeScript specifier names a directory.
const TS_INDEXES: &[&str] = &[
    "/index.ts",
    "/index.tsx",
    "/index.d.ts",
    "/index.mts",
    "/index.js",
    "/index.jsx",
    "/index.mjs",
];

/// What a specifier turned into.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolution {
    /// One or more files in this repository. More than one only for Go, where
    /// an import names a package directory rather than a file.
    Resolved(Vec<String>),
    /// Not resolvable, with a reason. [`UnresolvedReason::External`] means the
    /// specifier correctly names something outside the repository.
    Unresolved(UnresolvedReason),
}

impl Resolution {
    /// The resolved targets, empty when unresolved.
    pub fn targets(&self) -> &[String] {
        match self {
            Self::Resolved(paths) => paths,
            Self::Unresolved(_) => &[],
        }
    }

    /// Whether this counts as an external dependency.
    pub fn is_external(&self) -> bool {
        matches!(self, Self::Unresolved(UnresolvedReason::External))
    }
}

/// Resolves specifiers against a known set of repository files.
#[derive(Debug, Clone, Default)]
pub struct Resolver {
    files: BTreeSet<String>,
    /// Files grouped by directory, so a Go package lookup is one map hit
    /// rather than a scan of the whole tree per import.
    by_dir: BTreeMap<String, Vec<String>>,
    aliases: AliasConfig,
}

impl Resolver {
    /// Build a resolver over the repository's files.
    ///
    /// Pass *every* file, not only the parseable ones: `tsconfig.json` and
    /// `go.mod` are where the aliases live, and a resolver that only sees
    /// source files cannot find them.
    pub fn new(files: &[SourceFile]) -> Self {
        let mut by_dir: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for file in files {
            by_dir
                .entry(dir_of(&file.path))
                .or_default()
                .push(file.path.clone());
        }
        Self {
            files: files.iter().map(|f| f.path.clone()).collect(),
            by_dir,
            aliases: AliasConfig::discover(files),
        }
    }

    /// The discovered alias configuration.
    pub fn aliases(&self) -> &AliasConfig {
        &self.aliases
    }

    /// Whether a repo-relative path is a known file.
    pub fn has(&self, path: &str) -> bool {
        self.files.contains(path)
    }

    /// Resolve `specifier`, as written in `from` in language `lang`.
    pub fn resolve(&self, from: &str, lang: Language, specifier: &str) -> Resolution {
        match lang {
            Language::TypeScript | Language::Tsx => self.resolve_ts(from, specifier),
            Language::Rust => self.resolve_rust(from, specifier),
            Language::Python => self.resolve_python(from, specifier),
            Language::Go => self.resolve_go(specifier),
        }
    }

    // --- TypeScript / JavaScript -------------------------------------------

    fn resolve_ts(&self, from: &str, specifier: &str) -> Resolution {
        if specifier.starts_with("./") || specifier.starts_with("../") || specifier == "." {
            let candidate = join_relative(&dir_of(from), specifier);
            return match self.try_ts(&candidate) {
                Some(path) => Resolution::Resolved(vec![path]),
                None => Resolution::Unresolved(UnresolvedReason::NoSuchFile),
            };
        }
        if specifier.starts_with('/') {
            return match self.try_ts(&normalise(specifier)) {
                Some(path) => Resolution::Resolved(vec![path]),
                None => Resolution::Unresolved(UnresolvedReason::NoSuchFile),
            };
        }

        // The alias arm. `@/lib/math` is not a package and never was; treating
        // it as one is precisely the bug this workstream exists to not have.
        let expanded = self.aliases.expand_ts(from, specifier);
        let aliased = !expanded.is_empty();
        for candidate in expanded {
            if let Some(path) = self.try_ts(&candidate) {
                return Resolution::Resolved(vec![path]);
            }
        }

        for base in self.aliases.ts_base_urls_for(from) {
            if let Some(path) = self.try_ts(&join_relative(base, specifier)) {
                return Resolution::Resolved(vec![path]);
            }
        }

        if aliased {
            // It matched a configured alias and still found nothing. That is a
            // broken import or a gap in our extension list — either way it is
            // internal, and calling it "external" would hide it.
            return Resolution::Unresolved(UnresolvedReason::NoSuchFile);
        }
        Resolution::Unresolved(UnresolvedReason::External)
    }

    fn try_ts(&self, candidate: &str) -> Option<String> {
        if candidate.is_empty() {
            return None;
        }
        if self.files.contains(candidate) && Language::from_path(candidate).is_some() {
            return Some(candidate.to_string());
        }
        for extension in TS_EXTENSIONS {
            let with_extension = format!("{candidate}{extension}");
            if self.files.contains(&with_extension) {
                return Some(with_extension);
            }
        }
        for index in TS_INDEXES {
            let with_index = format!("{candidate}{index}");
            if self.files.contains(&with_index) {
                return Some(with_index);
            }
        }
        None
    }

    // --- Rust ---------------------------------------------------------------

    fn resolve_rust(&self, from: &str, specifier: &str) -> Resolution {
        // `mod foo;` is emitted by the extractor with this marker because it
        // resolves against the *current* module directory, while a `use` path
        // resolves against the crate root. Same syntax family, opposite base.
        // `#[path = "x.rs"] mod y;` is a literal, directory-relative filename.
        // It is how this repository points a module at a sibling test file, so
        // ignoring it would make the graph blind to its own tests.
        if let Some(relative) = specifier.strip_prefix("path ") {
            let candidate = join_relative(&dir_of(from), relative);
            return match self.files.contains(&candidate) {
                true => Resolution::Resolved(vec![candidate]),
                false => Resolution::Unresolved(UnresolvedReason::NoSuchFile),
            };
        }
        if let Some(name) = specifier.strip_prefix("mod ") {
            let base = self.rust_module_dir(from);
            return match self.rust_module_file(&base, name) {
                Some(path) => Resolution::Resolved(vec![path]),
                None => Resolution::Unresolved(UnresolvedReason::NoSuchFile),
            };
        }

        // A trailing `*` is a glob over a module's items, not a module of its
        // own; keeping it would send the search one level too deep.
        let segments: Vec<&str> = specifier
            .split("::")
            .filter(|s| !s.is_empty() && *s != "*")
            .collect();
        let Some(first) = segments.first() else {
            return Resolution::Unresolved(UnresolvedReason::NoSuchFile);
        };

        let own_crate = self.aliases.rust_crate.as_deref();
        let (base, rest): (String, &[&str]) =
            if *first == "crate" || own_crate.is_some_and(|c| c == *first) {
                (self.aliases.rust_src_root.clone(), &segments[1..])
            } else if *first == "self" {
                (self.rust_module_dir(from), &segments[1..])
            } else if *first == "super" {
                let ups = segments.iter().take_while(|s| **s == "super").count();
                let mut dir = self.rust_module_dir(from);
                for _ in 0..ups {
                    dir = dir_of(&dir);
                }
                (dir, &segments[ups..])
            } else {
                // `std`, `serde`, another workspace crate: correctly outside.
                return Resolution::Unresolved(UnresolvedReason::External);
            };

        // Longest prefix first: `use crate::graph::types::Definition` names a
        // *type* in a module, so the module path is one segment shorter than
        // the use path — but `use crate::graph::types` names the module
        // itself. Trying long to short gets both right without knowing which.
        for take in (0..=rest.len()).rev() {
            let candidate_dir = if take == 0 {
                base.clone()
            } else {
                format!("{base}/{}", rest[..take].join("/"))
            };
            if take == 0 {
                for root in ["lib.rs", "main.rs", "mod.rs"] {
                    let path = join_relative(&candidate_dir, root);
                    if self.files.contains(&path) {
                        return Resolution::Resolved(vec![path]);
                    }
                }
                // The module may be a flat `foo.rs` beside the `foo/`
                // directory rather than a `foo/mod.rs` inside it. Both layouts
                // are legal and both appear in the same codebases.
                if !candidate_dir.is_empty()
                    && let Some(path) =
                        self.rust_module_file(&dir_of(&candidate_dir), stem(&candidate_dir))
                {
                    return Resolution::Resolved(vec![path]);
                }
                continue;
            }
            let parent = dir_of(&candidate_dir);
            let name = &rest[take - 1];
            if let Some(path) = self.rust_module_file(&parent, name) {
                return Resolution::Resolved(vec![path]);
            }
        }
        Resolution::Unresolved(UnresolvedReason::NoSuchFile)
    }

    /// The directory that `self::` refers to inside `from`.
    ///
    /// `src/graph/mod.rs` *is* the `graph` module, so its directory is
    /// `src/graph`; `src/graph/types.rs` is a module one level deeper, so its
    /// children would live in `src/graph/types/`.
    fn rust_module_dir(&self, from: &str) -> String {
        let dir = dir_of(from);
        let stem = stem(from);
        if matches!(stem, "mod" | "lib" | "main") {
            dir
        } else if dir.is_empty() {
            stem.to_string()
        } else {
            format!("{dir}/{stem}")
        }
    }

    fn rust_module_file(&self, dir: &str, name: &str) -> Option<String> {
        for candidate in [format!("{name}.rs"), format!("{name}/mod.rs")] {
            let path = join_relative(dir, &candidate);
            if self.files.contains(&path) {
                return Some(path);
            }
        }
        None
    }

    // --- Python -------------------------------------------------------------

    fn resolve_python(&self, from: &str, specifier: &str) -> Resolution {
        if specifier.starts_with('.') {
            let dots = specifier.chars().take_while(|c| *c == '.').count();
            let tail = &specifier[dots..];
            // One dot is the current package; each extra dot walks up one.
            let mut dir = dir_of(from);
            for _ in 1..dots {
                dir = dir_of(&dir);
            }
            let suffix = tail.replace('.', "/");
            let candidate = if suffix.is_empty() {
                dir.clone()
            } else {
                join_relative(&dir, &suffix)
            };
            return match self.try_python(&candidate) {
                Some(path) => Resolution::Resolved(vec![path]),
                None => Resolution::Unresolved(UnresolvedReason::NoSuchFile),
            };
        }

        // Absolute dotted modules are matched by *suffix* against the file set
        // rather than joined onto a guessed root. That is what makes a
        // `src/`-layout package (`src/pkg/mod.py` imported as `pkg.mod`)
        // resolve without reading packaging metadata we may not have.
        let module = specifier.replace('.', "/");
        match self.python_hits(&module).as_slice() {
            [] if self
                .python_hits(specifier.split('.').next().unwrap_or_default())
                .is_empty() =>
            {
                Resolution::Unresolved(UnresolvedReason::External)
            }
            [] => Resolution::Unresolved(UnresolvedReason::NoSuchFile),
            [path] => Resolution::Resolved(vec![path.clone()]),
            hits => {
                // Shallowest wins only if it is strictly shallower; otherwise
                // two source roots genuinely both provide the module.
                if hits[1].matches('/').count() > hits[0].matches('/').count() {
                    Resolution::Resolved(vec![hits[0].clone()])
                } else {
                    Resolution::Unresolved(UnresolvedReason::Ambiguous)
                }
            }
        }
    }

    fn python_hits(&self, module: &str) -> Vec<String> {
        let mut hits: Vec<String> = [format!("{module}.py"), format!("{module}/__init__.py")]
            .into_iter()
            .flat_map(|wanted| {
                self.files
                    .iter()
                    .filter(move |path| **path == wanted || path.ends_with(&format!("/{wanted}")))
                    .cloned()
            })
            .collect();
        hits.sort_by_key(|path| (path.matches('/').count(), path.clone()));
        hits.dedup();
        hits
    }

    fn try_python(&self, candidate: &str) -> Option<String> {
        for wanted in [
            format!("{candidate}.py"),
            format!("{candidate}/__init__.py"),
            format!("{candidate}.pyi"),
        ] {
            if self.files.contains(&wanted) {
                return Some(wanted);
            }
        }
        if self.files.contains(candidate) {
            return Some(candidate.to_string());
        }
        None
    }

    // --- Go -----------------------------------------------------------------

    fn resolve_go(&self, specifier: &str) -> Resolution {
        let Some(module) = self.aliases.go_module.as_deref() else {
            return Resolution::Unresolved(UnresolvedReason::External);
        };
        let dir = if specifier == module {
            self.aliases.go_root.clone()
        } else if let Some(rest) = specifier.strip_prefix(&format!("{module}/")) {
            join_relative(&self.aliases.go_root, rest)
        } else {
            return Resolution::Unresolved(UnresolvedReason::External);
        };

        // A Go import names a package, and a package is every `.go` file in
        // one directory. Edges to all of them is the honest answer; picking
        // one would make the graph depend on file naming.
        let mut targets: Vec<String> = self
            .by_dir
            .get(&dir)
            .map(|paths| {
                paths
                    .iter()
                    .filter(|p| p.ends_with(".go") && !p.ends_with("_test.go"))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        targets.sort();
        if targets.is_empty() {
            return Resolution::Unresolved(UnresolvedReason::NoSuchFile);
        }
        Resolution::Resolved(targets)
    }
}

#[cfg(test)]
#[path = "resolve_test.rs"]
mod tests;
