//! The end-to-end harness a repository has, read off its tree at head.
//!
//! Everything here is a pure function over strings: the tree listing, the
//! workflow files, the changed paths. It decides three things before any
//! token is spent, and hands the model the answers rather than the question:
//!
//! - which files in the tree *are* end-to-end tests;
//! - which workflows (and which of their jobs) *are* end-to-end jobs;
//! - whether each of those workflows would even trigger for this pull request.
//!
//! The third is the one that bites. The commonest end-to-end gap is not a red
//! suite — branch protection catches that — but a workflow whose `paths:`
//! filter predates the suite's growth, or one that only runs on
//! `workflow_dispatch`, so a green pull request has an e2e suite with an
//! opinion about nothing. A model asked to read the YAML and guess would get
//! that wrong often enough to be worse than not asking.
//!
//! No YAML parser. Workflows are read as an indentation outline, which is all
//! `on:`, `paths:`, `jobs:` and `steps:` need, and it keeps the crate's
//! dependency set where it is. The scanner in `crate::scan::workflows` reads
//! the same files line by line for the same reason.

use std::fmt::Write as _;

use globset::{Glob, GlobMatcher};

/// Directory names that mark a path as an end-to-end test.
///
/// Looser than the unit-test table in `lanes::tests` on purpose: there is no
/// language convention for e2e placement, only project habit, and the table
/// is overridable per repository with `lanes.e2e.paths`.
const E2E_DIRS: &[&str] = &[
    "e2e",
    "end-to-end",
    "end_to_end",
    "integration",
    "integration-tests",
    "integration_tests",
    "acceptance",
    "smoke",
    "cypress",
    "playwright",
];

/// File-name fragments that mark a path as an end-to-end test on their own.
const E2E_NAME_MARKS: &[&str] = &[".e2e.", "_e2e.", "-e2e.", ".integration.", ".acceptance."];

/// Directories that are never part of the harness, whatever they contain.
const NEVER: &[&str] = &["node_modules", "vendor", "target", "dist", "build", ".git"];

/// Words in a workflow, job or file name that say "end to end".
const E2E_WORDS: &[&str] = &[
    "e2e",
    "end-to-end",
    "end_to_end",
    "endtoend",
    "integration",
    "acceptance",
    "smoke",
];

/// Step fragments that say a job drives a running system, whatever it is
/// called. Matched case-insensitively against `uses:` and `run:` lines.
const E2E_STEP_MARKS: &[&str] = &[
    "playwright",
    "cypress",
    "puppeteer",
    "selenium",
    "webdriver",
    "wdio",
    "testcafe",
    "docker compose up",
    "docker-compose up",
    "testcontainers",
    "k6 run",
    "test:e2e",
    "run e2e",
    "--e2e",
    "e2e-",
    "e2e_",
];

/// Whether `path` is an end-to-end test, by the default table.
pub fn is_e2e_test_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    let mut parts = lower.split('/').peekable();
    let mut in_e2e_dir = false;
    let mut name = "";
    while let Some(part) = parts.next() {
        if parts.peek().is_none() {
            name = part;
            break;
        }
        if NEVER.contains(&part) {
            return false;
        }
        if E2E_DIRS.contains(&part) {
            in_e2e_dir = true;
        }
    }
    if name.is_empty() {
        return false;
    }
    if name.ends_with(".feature") {
        return true;
    }
    if E2E_NAME_MARKS.iter().any(|mark| name.contains(mark)) {
        return true;
    }
    // Inside an e2e directory, only code and data count; a README in `e2e/`
    // is documentation, not a test.
    in_e2e_dir && !is_prose(name)
}

fn is_prose(name: &str) -> bool {
    const PROSE: &[&str] = &[
        ".md",
        ".markdown",
        ".txt",
        ".rst",
        ".png",
        ".jpg",
        ".gif",
        ".svg",
    ];
    PROSE.iter().any(|suffix| name.ends_with(suffix))
}

/// The path table in effect: the default, or the repository's own globs.
///
/// Explicit globs *replace* the default rather than extend it. A repository
/// that says where its suite lives has said the default table is wrong for
/// it, and extending would keep flagging the false positives it just
/// corrected.
pub struct PathTable {
    globs: Vec<GlobMatcher>,
    /// Whether `lanes.e2e.paths` was configured at all, as opposed to left
    /// empty. Kept separately from `globs` so an explicit table that failed
    /// to compile *anything* is distinguishable from no table at all — see
    /// `matches`.
    explicit: bool,
    /// How many configured globs failed to compile and were dropped.
    invalid: usize,
}

impl PathTable {
    /// Build from `lanes.e2e.paths`; empty means the default table.
    pub fn new(globs: &[String]) -> Self {
        let compiled: Vec<GlobMatcher> = globs
            .iter()
            .filter_map(|glob| Glob::new(glob).ok())
            .map(|glob| glob.compile_matcher())
            .collect();
        Self {
            invalid: globs.len() - compiled.len(),
            explicit: !globs.is_empty(),
            globs: compiled,
        }
    }

    /// Whether `path` is an end-to-end test under this table.
    ///
    /// An explicit `lanes.e2e.paths` *replaces* the default table, even when
    /// every entry in it failed to compile: silently falling back to the
    /// default here would re-enable paths the operator's malformed
    /// configuration never meant to re-enable, and report harness findings
    /// against them. `invalid_globs` is how the caller learns to surface
    /// that the configuration itself is broken.
    pub fn matches(&self, path: &str) -> bool {
        if self.explicit {
            return self.globs.iter().any(|glob| glob.is_match(path));
        }
        is_e2e_test_path(path)
    }

    /// How many configured globs failed to compile and were dropped.
    pub fn invalid_globs(&self) -> usize {
        self.invalid
    }
}

/// Whether a workflow, job or file name names end-to-end testing.
pub fn names_e2e(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    E2E_WORDS.iter().any(|word| lower.contains(word))
}

/// A workflow's pull-request trigger, as far as this pull request is concerned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Trigger {
    /// Runs on pull requests, subject to the path filters (GitHub's own
    /// semantics: `paths` must match at least one changed file, `paths-ignore`
    /// must not match every changed file).
    PullRequest {
        /// The `paths:` filter, empty when unfiltered.
        paths: Vec<String>,
        /// The `paths-ignore:` filter.
        paths_ignore: Vec<String>,
        /// The line the filter sits on, for anchoring a finding.
        filter_line: Option<u64>,
    },
    /// Never runs on a pull request; the string names what it runs on.
    Never(String),
}

/// One job in an e2e workflow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Job {
    /// The key under `jobs:`.
    pub key: String,
    /// The display name GitHub gives the check run: `name:` if set, else the
    /// key. A matrix job's check runs are named `<name> (<matrix values>)`,
    /// which `runs::matches_check` handles as a prefix.
    pub name: String,
    /// A label this job's `if:` requires the pull request to carry.
    pub label_gate: Option<String>,
}

/// An end-to-end workflow in the tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Workflow {
    /// `.github/workflows/<file>`.
    pub path: String,
    /// `name:` if set, else the file stem.
    pub name: String,
    /// When it runs.
    pub trigger: Trigger,
    /// The jobs that count as end to end. Every job when the workflow itself
    /// is named for it; otherwise only the ones that say so themselves.
    pub jobs: Vec<Job>,
}

/// Why a workflow would not run for this pull request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Applies {
    /// It would.
    Yes,
    /// It would not: `paths:` matched nothing this pull request changed.
    PathsExcluded,
    /// It would not: `paths-ignore:` matched everything this pull request changed.
    AllIgnored,
    /// It never runs on pull requests.
    NotOnPullRequests(String),
}

impl Workflow {
    /// Whether this workflow triggers for a pull request changing `changed`.
    pub fn applies_to(&self, changed: &[String]) -> Applies {
        match &self.trigger {
            Trigger::Never(on) => Applies::NotOnPullRequests(on.clone()),
            Trigger::PullRequest {
                paths,
                paths_ignore,
                ..
            } => {
                let matchers = |globs: &[String]| -> Vec<GlobMatcher> {
                    globs
                        .iter()
                        .filter_map(|glob| {
                            // GitHub's `**` spans directories and `*` does
                            // not; globset's defaults say the same with a
                            // literal separator.
                            Glob::new(glob).ok().map(|g| g.compile_matcher())
                        })
                        .collect()
                };
                if !paths.is_empty() {
                    let include = matchers(paths);
                    if !changed
                        .iter()
                        .any(|path| include.iter().any(|glob| glob.is_match(path)))
                    {
                        return Applies::PathsExcluded;
                    }
                }
                if !paths_ignore.is_empty() {
                    let ignore = matchers(paths_ignore);
                    if !changed.is_empty()
                        && changed
                            .iter()
                            .all(|path| ignore.iter().any(|glob| glob.is_match(path)))
                    {
                        return Applies::AllIgnored;
                    }
                }
                Applies::Yes
            }
        }
    }
}

/// The harness: what the tree holds and what the workflows say.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Harness {
    /// End-to-end test files in the tree at head.
    pub tests: Vec<String>,
    /// End-to-end workflows in the tree at head.
    pub workflows: Vec<Workflow>,
    /// Whether the tree listing was cut short by the forge, in which case
    /// `tests` may be incomplete and the lane must say so.
    pub truncated: bool,
}

impl Harness {
    /// Whether the repository has any end-to-end harness at all.
    pub fn is_empty(&self) -> bool {
        self.tests.is_empty() && self.workflows.is_empty()
    }

    /// Every e2e job across every e2e workflow.
    pub fn jobs(&self) -> impl Iterator<Item = (&Workflow, &Job)> {
        self.workflows
            .iter()
            .flat_map(|workflow| workflow.jobs.iter().map(move |job| (workflow, job)))
    }
}

/// Select the e2e test files from a tree listing.
pub fn e2e_tests(paths: &[String], table: &PathTable) -> Vec<String> {
    paths
        .iter()
        .filter(|path| table.matches(path))
        .cloned()
        .collect()
}

/// Every workflow file in a tree listing.
pub fn workflow_paths(paths: &[String]) -> Vec<String> {
    paths
        .iter()
        .filter(|path| crate::scan::workflows::is_workflow(path))
        .cloned()
        .collect()
}

/// Classify one workflow file, returning it only if it is end to end.
///
/// `named` is `lanes.e2e.workflows`: explicit workflow or job names that
/// count. When it is non-empty it is the whole answer, on the same reasoning
/// as [`PathTable`] — an operator who named the jobs has corrected the guess.
pub fn classify_workflow(path: &str, text: &str, named: &[String]) -> Option<Workflow> {
    let outline = Outline::parse(text);
    let stem = path
        .rsplit('/')
        .next()
        .unwrap_or(path)
        .trim_end_matches(".yml")
        .trim_end_matches(".yaml");
    let name = outline
        .top_value("name")
        .map(|value| unquote(&value))
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| stem.to_string());

    let explicit = !named.is_empty();
    let counts = |candidate: &str| {
        named
            .iter()
            .any(|n| n.eq_ignore_ascii_case(candidate.trim()))
    };
    let workflow_is_e2e = if explicit {
        counts(&name) || counts(stem)
    } else {
        names_e2e(&name) || names_e2e(stem)
    };

    let mut jobs = Vec::new();
    for job in outline.jobs() {
        let display = job.name.clone().unwrap_or_else(|| job.key.clone());
        let job_is_e2e = if explicit {
            counts(&job.key) || counts(&display)
        } else {
            names_e2e(&job.key)
                || names_e2e(&display)
                || job.has_services
                || job
                    .steps
                    .iter()
                    .any(|step| E2E_STEP_MARKS.iter().any(|mark| step.contains(mark)))
        };
        if workflow_is_e2e || job_is_e2e {
            jobs.push(Job {
                key: job.key,
                name: display,
                label_gate: job.label_gate,
            });
        }
    }
    if jobs.is_empty() {
        return None;
    }

    Some(Workflow {
        path: path.to_string(),
        name,
        trigger: outline.trigger(),
        jobs,
    })
}

/// Render the harness for the prompt, with the trigger verdicts already
/// decided against `changed`.
pub fn render(harness: &Harness, changed: &[String]) -> String {
    let mut out = String::from("End-to-end harness in this repository:\n");
    if harness.is_empty() {
        out.push_str("- none found: no e2e test files and no e2e workflow\n");
        return out;
    }
    if harness.tests.is_empty() {
        out.push_str("- tests: none found\n");
    } else {
        let shown: Vec<&str> = harness.tests.iter().take(30).map(String::as_str).collect();
        let _ = writeln!(
            out,
            "- tests ({} files): {}{}",
            harness.tests.len(),
            shown.join(", "),
            if harness.tests.len() > shown.len() {
                ", …"
            } else {
                ""
            }
        );
    }
    if harness.truncated {
        out.push_str(
            "- the tree listing was truncated by the forge; the test list above may be incomplete\n",
        );
    }
    for workflow in &harness.workflows {
        let _ = writeln!(out, "- workflow: {} ({})", workflow.path, workflow.name);
        let verdict = match workflow.applies_to(changed) {
            Applies::Yes => "triggers for this pull request".to_string(),
            Applies::PathsExcluded => {
                "does NOT trigger: its `paths:` filter matches nothing this pull request changed"
                    .to_string()
            }
            Applies::AllIgnored => {
                "does NOT trigger: its `paths-ignore:` filter matches everything this pull request changed"
                    .to_string()
            }
            Applies::NotOnPullRequests(on) => {
                format!("does NOT trigger on pull requests (runs on: {on})")
            }
        };
        let _ = writeln!(out, "    {verdict}");
        for job in &workflow.jobs {
            match &job.label_gate {
                Some(label) => {
                    let _ = writeln!(out, "    job `{}`: gated on label `{label}`", job.name);
                }
                None => {
                    let _ = writeln!(out, "    job `{}`", job.name);
                }
            }
        }
    }
    out
}

// --- the outline reader ----------------------------------------------------

/// One meaningful line of YAML: its indentation, key and inline value.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Node {
    indent: usize,
    /// The key, or the item text for a `- item` line (with `key` empty and
    /// `item` set).
    key: String,
    value: String,
    item: bool,
    line: u64,
}

/// A workflow file read as an indentation outline.
struct Outline {
    nodes: Vec<Node>,
}

/// One job as the outline sees it.
struct OutlineJob {
    key: String,
    name: Option<String>,
    label_gate: Option<String>,
    has_services: bool,
    /// Lower-cased `uses:` and `run:` values.
    steps: Vec<String>,
}

impl Outline {
    fn parse(text: &str) -> Self {
        let mut nodes = Vec::new();
        for (index, raw) in text.lines().enumerate() {
            let line = index as u64 + 1;
            let without_comment = strip_comment(raw);
            let trimmed = without_comment.trim_end();
            if trimmed.trim().is_empty() {
                continue;
            }
            let indent = trimmed.len() - trimmed.trim_start().len();
            let body = trimmed.trim_start();
            if let Some(item) = body.strip_prefix("- ") {
                // `- key: value` inside a list opens a mapping; `- value` is a
                // plain item. Both matter: steps are the former, `paths:` the
                // latter.
                if let Some((key, value)) = split_key(item) {
                    nodes.push(Node {
                        indent: indent + 2,
                        key,
                        value,
                        item: true,
                        line,
                    });
                } else {
                    nodes.push(Node {
                        indent,
                        key: String::new(),
                        value: item.trim().to_string(),
                        item: true,
                        line,
                    });
                }
            } else if body == "-" {
                continue;
            } else if let Some((key, value)) = split_key(body) {
                nodes.push(Node {
                    indent,
                    key,
                    value,
                    item: false,
                    line,
                });
            }
        }
        Self { nodes }
    }

    /// The inline value of a top-level key.
    fn top_value(&self, key: &str) -> Option<String> {
        self.nodes
            .iter()
            .find(|n| n.indent == 0 && !n.item && n.key == key)
            .map(|n| n.value.clone())
    }

    /// The index of a top-level key.
    fn top_index(&self, key: &str) -> Option<usize> {
        self.nodes
            .iter()
            .position(|n| n.indent == 0 && !n.item && n.key == key)
    }

    /// The nodes nested under `index`: everything until indentation returns
    /// to the parent's level.
    fn children(&self, index: usize) -> &[Node] {
        let parent = self.nodes[index].indent;
        let end = self.nodes[index + 1..]
            .iter()
            .position(|n| n.indent <= parent)
            .map(|offset| index + 1 + offset)
            .unwrap_or(self.nodes.len());
        &self.nodes[index + 1..end]
    }

    /// The direct children of `index`: the nested nodes at the shallowest
    /// nested indentation.
    fn direct(&self, index: usize) -> Vec<(usize, &Node)> {
        let children = self.children(index);
        let Some(level) = children.iter().map(|n| n.indent).min() else {
            return Vec::new();
        };
        children
            .iter()
            .enumerate()
            .filter(|(_, n)| n.indent == level)
            .map(|(offset, n)| (index + 1 + offset, n))
            .collect()
    }

    /// The plain list items directly under `index`, or the inline `[a, b]`.
    fn list(&self, index: usize) -> Vec<String> {
        let inline = &self.nodes[index].value;
        if let Some(inner) = inline.strip_prefix('[').and_then(|v| v.strip_suffix(']')) {
            return inner
                .split(',')
                .map(|s| unquote(s.trim()))
                .filter(|s| !s.is_empty())
                .collect();
        }
        self.children(index)
            .iter()
            .filter(|n| n.item && n.key.is_empty())
            .map(|n| unquote(&n.value))
            .collect()
    }

    fn trigger(&self) -> Trigger {
        // `on:` is also spelled `true:` by YAML 1.1 parsers that round-trip;
        // GitHub accepts both, so both are read.
        let Some(index) = self.top_index("on").or_else(|| self.top_index("true")) else {
            return Trigger::Never("nothing: no `on:` block".into());
        };
        let node = &self.nodes[index];
        let events: Vec<(String, Option<usize>)> = if !node.value.is_empty() {
            // `on: push` or `on: [push, pull_request]`.
            self.list(index)
                .into_iter()
                .chain((!node.value.starts_with('[')).then(|| unquote(&node.value)))
                .map(|event| (event, None))
                .collect()
        } else {
            self.direct(index)
                .into_iter()
                .map(|(i, n)| {
                    if n.item && n.key.is_empty() {
                        (unquote(&n.value), None)
                    } else {
                        (n.key.clone(), Some(i))
                    }
                })
                .collect()
        };

        let mut on_names = Vec::new();
        for (event, index) in &events {
            if event == "pull_request" || event == "pull_request_target" {
                let mut paths = Vec::new();
                let mut paths_ignore = Vec::new();
                let mut filter_line = None;
                if let Some(index) = index {
                    for (i, child) in self.direct(*index) {
                        match child.key.as_str() {
                            "paths" => {
                                paths = self.list(i);
                                filter_line.get_or_insert(child.line);
                            }
                            "paths-ignore" => {
                                paths_ignore = self.list(i);
                                filter_line.get_or_insert(child.line);
                            }
                            _ => {}
                        }
                    }
                }
                return Trigger::PullRequest {
                    paths,
                    paths_ignore,
                    filter_line,
                };
            }
            on_names.push(event.clone());
        }
        if on_names.is_empty() {
            on_names.push("nothing".into());
        }
        Trigger::Never(on_names.join(", "))
    }

    fn jobs(&self) -> Vec<OutlineJob> {
        let Some(index) = self.top_index("jobs") else {
            return Vec::new();
        };
        self.direct(index)
            .into_iter()
            .filter(|(_, n)| !n.item)
            .map(|(i, n)| {
                let mut job = OutlineJob {
                    key: n.key.clone(),
                    name: None,
                    label_gate: None,
                    has_services: false,
                    steps: Vec::new(),
                };
                for (j, field) in self.direct(i) {
                    match field.key.as_str() {
                        "name" => job.name = Some(unquote(&field.value)),
                        "if" => job.label_gate = label_in(&field.value),
                        "services" => job.has_services = true,
                        "steps" => {
                            for step in self.children(j) {
                                if step.key == "uses" || step.key == "run" {
                                    job.steps.push(step.value.to_ascii_lowercase());
                                }
                                // A step's `if:` gating on a label gates the
                                // whole job's usefulness just as well.
                                if step.key == "if" && job.label_gate.is_none() {
                                    job.label_gate = label_in(&step.value);
                                }
                            }
                        }
                        _ => {}
                    }
                }
                job
            })
            .collect()
    }
}

/// Split `key: value`, refusing lines that are not a mapping entry.
fn split_key(body: &str) -> Option<(String, String)> {
    let (key, value) = body.split_once(':')?;
    let key = key.trim();
    // `http://…` inside a value is not a key, and neither is `${{ a:b }}`.
    if key.is_empty()
        || key.contains(' ')
        || key.contains('{')
        || key.starts_with('"')
        || key.starts_with('\'')
    {
        return None;
    }
    if !(value.is_empty() || value.starts_with(' ')) {
        return None;
    }
    Some((key.to_string(), value.trim().to_string()))
}

fn strip_comment(line: &str) -> &str {
    // A `#` that starts a line or follows whitespace opens a comment; one
    // inside a quoted value does not, and a glob like `**/#foo` is nobody's
    // real file.
    let bytes = line.as_bytes();
    let mut quote: Option<u8> = None;
    for (i, &b) in bytes.iter().enumerate() {
        match (quote, b) {
            (None, b'"' | b'\'') => quote = Some(b),
            (Some(q), _) if q == b => quote = None,
            (None, b'#') if i == 0 || bytes[i - 1].is_ascii_whitespace() => {
                return &line[..i];
            }
            _ => {}
        }
    }
    line
}

fn unquote(value: &str) -> String {
    let trimmed = value.trim();
    let inner = trimmed
        .strip_prefix('"')
        .and_then(|v| v.strip_suffix('"'))
        .or_else(|| {
            trimmed
                .strip_prefix('\'')
                .and_then(|v| v.strip_suffix('\''))
        })
        .unwrap_or(trimmed);
    inner.to_string()
}

/// The label an `if:` expression requires, if it is a label expression.
///
/// Only the idiom is recognised —
/// `contains(github.event.pull_request.labels.*.name, 'run-e2e')` — and only
/// when it is not negated. Anything else is not a label gate as far as this
/// lane can tell, and saying nothing is better than a wrong gate.
fn label_in(expr: &str) -> Option<String> {
    let lower = expr.to_ascii_lowercase();
    let at = lower.find("labels.*.name")?;
    if lower[..at].contains('!') {
        return None;
    }
    let rest = &expr[at..];
    let (_, after) = rest.split_once(',')?;
    let after = after.trim_start();
    let quote = after.chars().next().filter(|c| *c == '\'' || *c == '"')?;
    let label: String = after[1..].chars().take_while(|c| *c != quote).collect();
    (!label.is_empty()).then_some(label)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn e2e_paths_are_recognised_across_the_common_layouts() {
        for path in [
            "e2e/login.spec.ts",
            "tests/e2e/checkout.py",
            "test/integration/api_test.go",
            "cypress/e2e/home.cy.js",
            "features/signup.feature",
            "src/app/login.e2e.ts",
            "playwright/smoke.spec.ts",
        ] {
            assert!(is_e2e_test_path(path), "{path} was not recognised");
        }
        for path in [
            "src/latest.rs",
            "tests/unit/api_test.go",
            "e2e/README.md",
            "node_modules/e2e/thing.js",
            "src/integration.rs",
        ] {
            assert!(!is_e2e_test_path(path), "{path} was wrongly recognised");
        }
    }

    #[test]
    fn explicit_globs_replace_the_default_table() {
        let table = PathTable::new(&strings(&["qa/**/*.ts"]));
        assert!(table.matches("qa/flows/login.ts"));
        assert!(
            !table.matches("e2e/login.spec.ts"),
            "the default table must not leak past an explicit one"
        );
    }

    const WORKFLOW: &str = r#"
name: E2E
on:
  pull_request:
    paths:
      - "src/server/**"
      - 'e2e/**'
  workflow_dispatch:
jobs:
  e2e-playwright:
    name: Playwright
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - run: npx playwright test
  lint:
    runs-on: ubuntu-latest
    steps:
      - run: npm run lint
"#;

    #[test]
    fn a_workflow_named_e2e_counts_every_job_and_reads_its_path_filter() {
        let workflow = classify_workflow(".github/workflows/e2e.yml", WORKFLOW, &[])
            .expect("classified as e2e");
        assert_eq!(workflow.name, "E2E");
        assert_eq!(
            workflow
                .jobs
                .iter()
                .map(|j| j.name.as_str())
                .collect::<Vec<_>>(),
            ["Playwright", "lint"]
        );
        assert_eq!(
            workflow.trigger,
            Trigger::PullRequest {
                paths: strings(&["src/server/**", "e2e/**"]),
                paths_ignore: vec![],
                filter_line: Some(5),
            }
        );
        assert_eq!(
            workflow.applies_to(&strings(&["src/preview/apply.rs"])),
            Applies::PathsExcluded
        );
        assert_eq!(
            workflow.applies_to(&strings(&["src/server/routes.rs"])),
            Applies::Yes
        );
    }

    #[test]
    fn a_job_is_e2e_by_its_steps_when_the_workflow_is_not_named_for_it() {
        let text = "name: CI\non: [push, pull_request]\njobs:\n  unit:\n    steps:\n      - run: cargo test\n  browser:\n    steps:\n      - run: npx cypress run\n";
        let workflow =
            classify_workflow(".github/workflows/ci.yml", text, &[]).expect("has an e2e job");
        assert_eq!(workflow.jobs.len(), 1);
        assert_eq!(workflow.jobs[0].key, "browser");
        assert_eq!(
            workflow.trigger,
            Trigger::PullRequest {
                paths: vec![],
                paths_ignore: vec![],
                filter_line: None
            }
        );
    }

    #[test]
    fn a_workflow_with_no_e2e_job_is_not_a_harness() {
        let text =
            "name: CI\non: pull_request\njobs:\n  unit:\n    steps:\n      - run: cargo test\n";
        assert!(classify_workflow(".github/workflows/ci.yml", text, &[]).is_none());
    }

    #[test]
    fn a_dispatch_only_workflow_never_triggers() {
        let text = "name: Nightly e2e\non:\n  schedule:\n    - cron: '0 3 * * *'\n  workflow_dispatch:\njobs:\n  run:\n    steps:\n      - run: make e2e\n";
        let workflow = classify_workflow(".github/workflows/nightly.yml", text, &[]).unwrap();
        assert_eq!(
            workflow.applies_to(&strings(&["src/main.rs"])),
            Applies::NotOnPullRequests("schedule, workflow_dispatch".into())
        );
    }

    #[test]
    fn paths_ignore_excludes_only_when_everything_is_ignored() {
        let text = "name: e2e\non:\n  pull_request:\n    paths-ignore:\n      - '**.md'\njobs:\n  run:\n    steps:\n      - run: make e2e\n";
        let workflow = classify_workflow(".github/workflows/e2e.yml", text, &[]).unwrap();
        assert_eq!(
            workflow.applies_to(&strings(&["README.md", "docs/a.md"])),
            Applies::AllIgnored
        );
        assert_eq!(
            workflow.applies_to(&strings(&["README.md", "src/a.rs"])),
            Applies::Yes
        );
    }

    #[test]
    fn a_label_gate_is_read_off_the_job_condition() {
        let text = "name: e2e\non: pull_request\njobs:\n  run:\n    if: contains(github.event.pull_request.labels.*.name, 'run-e2e')\n    steps:\n      - run: make e2e\n";
        let workflow = classify_workflow(".github/workflows/e2e.yml", text, &[]).unwrap();
        assert_eq!(workflow.jobs[0].label_gate.as_deref(), Some("run-e2e"));

        let negated = "name: e2e\non: pull_request\njobs:\n  run:\n    if: \"!contains(github.event.pull_request.labels.*.name, 'skip-e2e')\"\n    steps:\n      - run: make e2e\n";
        let workflow = classify_workflow(".github/workflows/e2e.yml", negated, &[]).unwrap();
        assert_eq!(
            workflow.jobs[0].label_gate, None,
            "a negated gate is not a gate"
        );
    }

    #[test]
    fn explicit_workflow_names_are_the_whole_answer() {
        let text = "name: CI\non: pull_request\njobs:\n  unit:\n    steps:\n      - run: cargo test\n  browser:\n    steps:\n      - run: npx cypress run\n";
        let workflow =
            classify_workflow(".github/workflows/ci.yml", text, &strings(&["unit"])).unwrap();
        assert_eq!(
            workflow
                .jobs
                .iter()
                .map(|j| j.key.as_str())
                .collect::<Vec<_>>(),
            ["unit"],
            "naming `unit` counts it and stops guessing about `browser`"
        );
    }

    #[test]
    fn comments_and_urls_do_not_confuse_the_outline() {
        let text = "name: e2e # the suite\non:\n  pull_request: # every PR\njobs:\n  run:\n    steps:\n      - run: curl http://localhost:3000/health\n      - run: npx playwright test\n";
        let workflow = classify_workflow(".github/workflows/e2e.yml", text, &[]).unwrap();
        assert_eq!(workflow.name, "e2e");
        assert!(matches!(workflow.trigger, Trigger::PullRequest { .. }));
    }

    #[test]
    fn the_render_states_each_verdict() {
        let workflow = classify_workflow(".github/workflows/e2e.yml", WORKFLOW, &[]).unwrap();
        let harness = Harness {
            tests: strings(&["e2e/login.spec.ts"]),
            workflows: vec![workflow],
            truncated: true,
        };
        let text = render(&harness, &strings(&["src/preview/apply.rs"]));
        assert!(
            text.contains("tests (1 files): e2e/login.spec.ts"),
            "{text}"
        );
        assert!(
            text.contains("does NOT trigger: its `paths:` filter"),
            "{text}"
        );
        assert!(text.contains("truncated"), "{text}");
        assert!(text.contains("job `Playwright`"), "{text}");
    }
}

#[cfg(test)]
mod scratch_probe {
    #[test]
    fn probe_globset_star_semantics() {
        let g = globset::Glob::new("src/*").unwrap().compile_matcher();
        println!("src/server/routes.rs matches src/*: {}", g.is_match("src/server/routes.rs"));
        assert!(!g.is_match("src/server/routes.rs"), "star crossed a separator by default");
    }
}
