//! Path-alias configuration, discovered from the repository's own config files.
//!
//! Always compiled. This is the module that exists because the obvious
//! implementation of a repository graph does not have it.
//!
//! A regex that only accepts specifiers beginning with `.` or `..` cannot
//! follow `@/lib/math`, `~/components/Button`, `github.com/me/mod/pkg` or
//! `crate::graph::types` — and a modern TypeScript codebase writes almost
//! every internal import as an alias. The resulting graph is not merely
//! incomplete; it is edgeless in exactly the places a reviewer cares about.
//! So aliases are read from `tsconfig.json` / `jsconfig.json`, `go.mod` and
//! `Cargo.toml`, the same files the language's own toolchain reads.

use std::collections::BTreeMap;

use crate::graph::path::{join_relative, normalise};
use crate::graph::types::SourceFile;

/// One `compilerOptions.paths` entry, pre-joined to repo-relative form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AliasPattern {
    /// The pattern as written, e.g. `@/*` or `@app`.
    pub pattern: String,
    /// Repo-relative targets, e.g. `src/*`. Tried in order, first hit wins,
    /// matching how `tsc` treats the array.
    pub targets: Vec<String>,
}

impl AliasPattern {
    /// Expand `specifier` through this pattern, or `None` if it does not match.
    ///
    /// TypeScript allows at most one `*` per pattern and treats it as a
    /// non-greedy wildcard; a pattern without `*` matches exactly.
    pub fn expand(&self, specifier: &str) -> Option<Vec<String>> {
        match self.pattern.split_once('*') {
            None => (self.pattern == specifier).then(|| self.targets.clone()),
            Some((prefix, suffix)) => {
                let rest = specifier.strip_prefix(prefix)?;
                let star = rest.strip_suffix(suffix)?;
                Some(
                    self.targets
                        .iter()
                        .map(|t| normalise(&t.replacen('*', star, 1)))
                        .collect(),
                )
            }
        }
    }

    /// How specific this pattern is, for longest-prefix ordering.
    fn specificity(&self) -> usize {
        self.pattern.split('*').next().unwrap_or("").len()
    }
}

/// Everything the resolver needs to know about the repository's own layout.
///
/// Built from the file set rather than from the filesystem, so the resolver
/// stays a pure function of its inputs and every alias case is testable from
/// string literals.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AliasConfig {
    /// TypeScript path patterns, most specific first.
    pub ts_paths: Vec<AliasPattern>,
    /// Repo-relative `baseUrl` directories, in discovery order.
    ///
    /// A bare `lib/math` specifier resolves against these before being written
    /// off as a package — which is the second way a naive resolver loses
    /// internal edges.
    pub ts_base_urls: Vec<String>,
    /// The module path from `go.mod`, if present.
    pub go_module: Option<String>,
    /// The directory holding that `go.mod`, repo-relative (`""` at the root).
    pub go_root: String,
    /// Crate name from `Cargo.toml`, with `-` normalised to `_` as rustc does.
    pub rust_crate: Option<String>,
    /// The crate's source root, repo-relative and without a trailing slash.
    pub rust_src_root: String,
}

impl AliasConfig {
    /// Discover configuration from the repository's own config files.
    pub fn discover(files: &[SourceFile]) -> Self {
        let mut config = Self {
            rust_src_root: "src".to_string(),
            ..Self::default()
        };
        // Sorted so shallower config files win: a root `tsconfig.json` should
        // be consulted before a nested one, and discovery order is otherwise
        // whatever the caller happened to pass.
        let mut ordered: Vec<&SourceFile> = files.iter().collect();
        ordered.sort_by_key(|f| (f.path.matches('/').count(), f.path.clone()));

        for file in ordered {
            let name = file.path.rsplit('/').next().unwrap_or(&file.path);
            let dir = parent_dir(&file.path);
            match name {
                "tsconfig.json" | "jsconfig.json" => config.absorb_tsconfig(&file.text, &dir),
                "go.mod" => config.absorb_go_mod(&file.text, &dir),
                "Cargo.toml" => config.absorb_cargo_toml(&file.text, &dir),
                _ => {}
            }
        }

        config
            .ts_paths
            .sort_by(|a, b| b.specificity().cmp(&a.specificity()).then(a.pattern.cmp(&b.pattern)));
        config.ts_paths.dedup_by(|a, b| a.pattern == b.pattern);
        config.ts_base_urls.dedup();
        config
    }

    fn absorb_tsconfig(&mut self, text: &str, dir: &str) {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&strip_jsonc(text)) else {
            return;
        };
        let options = value.get("compilerOptions").unwrap_or(&value);

        // `paths` targets are relative to `baseUrl` when one is set, and to the
        // tsconfig's own directory otherwise. Getting this wrong shifts every
        // alias by one directory, which resolves to nothing and looks exactly
        // like "the repository has no internal imports".
        let base_url = options.get("baseUrl").and_then(|v| v.as_str());
        let base_dir = match base_url {
            Some(base) => join_relative(dir, base),
            None => dir.to_string(),
        };
        if base_url.is_some() {
            self.ts_base_urls.push(base_dir.clone());
        }

        if let Some(paths) = options.get("paths").and_then(|v| v.as_object()) {
            for (pattern, targets) in paths {
                let targets: Vec<String> = targets
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|t| t.as_str())
                            .map(|t| join_relative(&base_dir, t))
                            .collect()
                    })
                    .unwrap_or_default();
                if !targets.is_empty() {
                    self.ts_paths.push(AliasPattern {
                        pattern: pattern.clone(),
                        targets,
                    });
                }
            }
        }
    }

    fn absorb_go_mod(&mut self, text: &str, dir: &str) {
        if self.go_module.is_some() {
            return;
        }
        for line in text.lines() {
            let line = line.trim();
            if let Some(rest) = line.strip_prefix("module ") {
                self.go_module = Some(rest.trim().trim_matches('"').to_string());
                self.go_root = dir.to_string();
                return;
            }
        }
    }

    fn absorb_cargo_toml(&mut self, text: &str, dir: &str) {
        if self.rust_crate.is_some() {
            return;
        }
        let Ok(value) = toml::from_str::<toml::Value>(text) else {
            return;
        };
        let Some(name) = value
            .get("package")
            .and_then(|p| p.get("name"))
            .and_then(|n| n.as_str())
        else {
            return;
        };
        // rustc replaces `-` with `_` in the crate name a `use` statement sees,
        // so `use my_crate::x` in a package called `my-crate` must match.
        self.rust_crate = Some(name.replace('-', "_"));
        self.rust_src_root = if dir.is_empty() {
            "src".to_string()
        } else {
            format!("{dir}/src")
        };
    }

    /// Expand a TypeScript specifier through the alias patterns.
    pub fn expand_ts(&self, specifier: &str) -> Vec<String> {
        for pattern in &self.ts_paths {
            if let Some(expanded) = pattern.expand(specifier) {
                return expanded;
            }
        }
        Vec::new()
    }
}

/// The directory part of a repo-relative path, `""` at the root.
pub(crate) fn parent_dir(path: &str) -> String {
    match path.rfind('/') {
        Some(index) => path[..index].to_string(),
        None => String::new(),
    }
}

/// Strip `//` and `/* */` comments and trailing commas from JSONC.
///
/// `tsconfig.json` is JSON with comments by convention and by tooling support;
/// `serde_json` rejects both. Bailing on a commented tsconfig would mean
/// losing every alias in most real repositories, so the cheap preprocessor is
/// the difference between the feature working and not.
fn strip_jsonc(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    let mut in_string = false;
    let mut escaped = false;

    while let Some(c) = chars.next() {
        if in_string {
            out.push(c);
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        match c {
            '"' => {
                in_string = true;
                out.push(c);
            }
            '/' if chars.peek() == Some(&'/') => {
                for c in chars.by_ref() {
                    if c == '\n' {
                        out.push('\n');
                        break;
                    }
                }
            }
            '/' if chars.peek() == Some(&'*') => {
                chars.next();
                let mut previous = '\0';
                for c in chars.by_ref() {
                    if previous == '*' && c == '/' {
                        break;
                    }
                    previous = c;
                }
                out.push(' ');
            }
            _ => out.push(c),
        }
    }

    // Trailing commas, in a second pass: doing it inline would need lookahead
    // past whitespace, and the string state is already tracked above.
    let mut cleaned = String::with_capacity(out.len());
    let mut in_string = false;
    let mut escaped = false;
    let bytes: Vec<char> = out.chars().collect();
    for (index, &c) in bytes.iter().enumerate() {
        if in_string {
            cleaned.push(c);
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        if c == '"' {
            in_string = true;
            cleaned.push(c);
            continue;
        }
        if c == ','
            && bytes[index + 1..]
                .iter()
                .find(|n| !n.is_whitespace())
                .is_some_and(|n| *n == '}' || *n == ']')
        {
            continue;
        }
        cleaned.push(c);
    }
    cleaned
}

/// A map of pattern to targets, for tests and diagnostics.
pub fn alias_map(config: &AliasConfig) -> BTreeMap<String, Vec<String>> {
    config
        .ts_paths
        .iter()
        .map(|p| (p.pattern.clone(), p.targets.clone()))
        .collect()
}

#[cfg(test)]
#[path = "aliases_test.rs"]
mod tests;
