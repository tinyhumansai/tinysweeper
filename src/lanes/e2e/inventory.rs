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

use std::collections::BTreeSet;
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
        /// Whether this is `pull_request_target` rather than plain
        /// `pull_request`.
        ///
        /// The distinction matters upstream of this type: GitHub resolves a
        /// `pull_request_target` workflow's *definition* — its jobs, its own
        /// trigger — from the base branch, never the head, precisely so a
        /// fork cannot rewrite the workflow that runs with base-branch
        /// secrets. Classifying one from the head-branch file (what
        /// `evidence::gather` reads by default) can therefore describe a
        /// workflow GitHub will never run in that shape. See
        /// `evidence::gather`'s `pull_request_target` re-read.
        target: bool,
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

/// Which of `changed` a GitHub `paths:`/`paths-ignore:` pattern list matches.
///
/// GitHub evaluates the list in order rather than as a plain union: a
/// pattern prefixed `!` *removes* its matches from the running set instead of
/// adding to it, so a later pattern can carve an exception out of an earlier
/// one (`["**", "!docs/**"]` matches everything except `docs/`). Patterns
/// also use a literal path separator — GitHub's `*` does not cross `/`, only
/// `**` does — which globset only enforces when asked to; its default
/// compilation lets `*` span directories, which would let `src/*` match
/// `src/server/routes.rs`.
fn github_path_matches(patterns: &[String], changed: &[String]) -> BTreeSet<String> {
    let mut matched: BTreeSet<String> = BTreeSet::new();
    for pattern in patterns {
        let (negate, glob) = match pattern.strip_prefix('!') {
            Some(rest) => (true, rest),
            None => (false, pattern.as_str()),
        };
        let Ok(built) = globset::GlobBuilder::new(glob)
            .literal_separator(true)
            .build()
        else {
            continue;
        };
        let matcher = built.compile_matcher();
        for path in changed {
            if matcher.is_match(path) {
                if negate {
                    matched.remove(path);
                } else {
                    matched.insert(path.clone());
                }
            }
        }
    }
    matched
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
                if !paths.is_empty() && github_path_matches(paths, changed).is_empty() {
                    return Applies::PathsExcluded;
                }
                if !paths_ignore.is_empty() {
                    let ignored = github_path_matches(paths_ignore, changed);
                    if !changed.is_empty() && changed.iter().all(|path| ignored.contains(path)) {
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
            // A service container is common in ordinary unit/integration CI
            // (a job running unit tests against Postgres or Redis) and is
            // not, on its own, evidence of an end-to-end suite. It only
            // counts alongside something that actually says "end to end":
            // the workflow's own name, or a step that drives a real e2e
            // runner. Otherwise it would misclassify plain service-backed
            // unit-test jobs and manufacture false `e2e-not-triggered`
            // findings against them.
            names_e2e(&job.key)
                || names_e2e(&display)
                || job
                    .steps
                    .iter()
                    .any(|step| E2E_STEP_MARKS.iter().any(|mark| step.contains(mark)))
                || (job.has_services && workflow_is_e2e)
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

/// One step's fields, accumulated while `jobs()` walks the flattened list
/// under `steps:`, so a step's own `if:` is only ever attributed to that
/// step.
///
/// Only a step that itself runs something e2e-shaped (`E2E_STEP_MARKS`) may
/// set the job's label gate — an unrelated auxiliary step (a label-gated
/// artifact upload ahead of an unconditional `playwright test`) must not
/// make the whole job read as gated on a label it does not require. This
/// deliberately leaves a job-wide `if:` (read directly off the job, not a
/// step) alone; that one already gates every step including the e2e one.
#[derive(Default)]
struct StepBeingRead {
    gate: Option<String>,
    is_e2e_step: bool,
}

impl StepBeingRead {
    fn note_e2e_mark(&mut self, lower_value: &str) {
        if E2E_STEP_MARKS.iter().any(|mark| lower_value.contains(mark)) {
            self.is_e2e_step = true;
        }
    }

    /// Apply this step's gate to `label_gate`, if this step earned the right
    /// to (it ran something e2e-shaped) and no earlier e2e step already
    /// decided one.
    ///
    /// `decided` is tracked separately from `label_gate.is_none()`: an
    /// earlier, *unconditional* e2e step deciding "no gate" also leaves
    /// `label_gate` at `None`, which is indistinguishable from "no e2e step
    /// has spoken yet" if that were the only signal. Without `decided`, a
    /// later e2e step that happens to be label-gated would overwrite the
    /// earlier unconditional step's `None` with its own gate — reporting
    /// the whole job as gated on a label when it already runs an
    /// unconditional e2e step regardless.
    fn commit(&self, label_gate: &mut Option<String>, decided: &mut bool) {
        if self.is_e2e_step && !*decided {
            *label_gate = self.gate.clone();
            *decided = true;
        }
    }
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
        let events: Vec<(String, Option<usize>)> = if node.value.starts_with('{') {
            // `on: {pull_request: {paths: [...]}}` — a flow mapping. Its
            // nested filters are not walked (that would need real YAML), so
            // the event is recognised but treated as unfiltered — a
            // conservative "would trigger" rather than the `Trigger::Never`
            // that reading the whole mapping as one literal event name used
            // to produce, which reported an active e2e workflow as one that
            // never runs on pull requests at all.
            flow_mapping_keys(&node.value)
                .into_iter()
                .map(|event| (event, None))
                .collect()
        } else if !node.value.is_empty() {
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
                    target: event == "pull_request_target",
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
                // Whether a label gate has already been decided by an e2e
                // step — tracked separately from `job.label_gate.is_none()`
                // because "decided, and the decision was no gate" and "not
                // decided yet" both leave `label_gate` at `None`.
                let mut label_gate_decided = false;
                // The job-level `if:`, applied after every field is read
                // rather than in document order: nothing here requires
                // `if:` to appear before `steps:` in the file, and a
                // job-level condition always outranks a step-level guess
                // however the two are ordered.
                let mut job_level_gate: Option<Option<String>> = None;
                for (j, field) in self.direct(i) {
                    match field.key.as_str() {
                        "name" => job.name = Some(unquote(&field.value)),
                        "if" => {
                            // Only when the job-level condition actually
                            // mentions a label at all — positively
                            // (`Some(label)`) or negated (`None`, "a
                            // negated gate is not a gate") — does it decide
                            // anything here. An unrelated condition
                            // (`github.repository_owner == 'acme'`) says
                            // nothing about a label gate one way or the
                            // other, and must not erase what a step already
                            // inferred.
                            if mentions_label(&field.value) {
                                job_level_gate = Some(label_in(&field.value));
                            }
                        }
                        "services" => job.has_services = true,
                        "steps" => {
                            // `self.children(j)` is every field of every step,
                            // flattened — a step boundary is only visible as
                            // `item: true` on the field that opened it (`- name:
                            // ...`, `- run: ...`). Grouped by hand here so an
                            // `if:` is only ever attributed to the step it
                            // actually sits on, not to whichever step happens to
                            // come next.
                            let mut current = StepBeingRead::default();
                            for step in self.children(j) {
                                if step.item {
                                    current.commit(&mut job.label_gate, &mut label_gate_decided);
                                    current = StepBeingRead::default();
                                }
                                if step.key == "uses" || step.key == "run" {
                                    let text = step.value.to_ascii_lowercase();
                                    current.note_e2e_mark(&text);
                                    job.steps.push(text);
                                }
                                if step.key == "if" {
                                    current.gate = label_in(&step.value);
                                }
                            }
                            current.commit(&mut job.label_gate, &mut label_gate_decided);
                        }
                        _ => {}
                    }
                }
                if let Some(gate) = job_level_gate {
                    job.label_gate = gate;
                }
                job
            })
            .collect()
    }
}

/// Split `key: value`, refusing lines that are not a mapping entry.
fn split_key(body: &str) -> Option<(String, String)> {
    let (raw_key, value) = body.split_once(':')?;
    let key = unquote_key(raw_key.trim())?;
    // `http://…` inside a value is not a key, and neither is `${{ a:b }}`.
    if key.is_empty() || key.contains(' ') || key.contains('{') {
        return None;
    }
    if !(value.is_empty() || value.starts_with(' ')) {
        return None;
    }
    Some((key, value.trim().to_string()))
}

/// A mapping key, plain or quoted — `on:`, `'on':` and `"on":` are all the
/// same key.
///
/// GitHub accepts `'on':`/`"on":` (quoting is how a YAML 1.1 author avoids
/// `on` being read as the boolean `true`), and rejecting every quoted key
/// outright — the previous rule — misread `'on': pull_request` as a workflow
/// with no `on:` block at all, which `Outline::trigger` reads as
/// `Trigger::Never`. Only a *fully* quoted key is accepted; anything that
/// merely starts with a quote (a plain scalar value that happens to contain
/// a colon, `- "http://example.com: see docs"`) still falls through to
/// `None`, exactly as it did before.
fn unquote_key(raw: &str) -> Option<String> {
    for quote in ['\'', '"'] {
        if let Some(inner) = raw
            .strip_prefix(quote)
            .and_then(|rest| rest.strip_suffix(quote))
        {
            return (!inner.is_empty() && !inner.contains(quote)).then(|| inner.to_string());
        }
    }
    if raw.starts_with('"') || raw.starts_with('\'') {
        return None;
    }
    Some(raw.to_string())
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

/// The top-level keys of a YAML flow mapping like `{pull_request: {paths:
/// [...]}}`, without parsing it as YAML.
///
/// Split on commas at bracket depth zero so a nested `{...}` or `[...]`
/// value is not itself split, then take the text before each entry's first
/// `:` as its key. Good enough to recognise which events a compact `on:`
/// names; nested filters (`paths:` inside the mapping) are not read, on the
/// same "no YAML parser" reasoning as the rest of this module — they are
/// left for `applies_to` to treat as unfiltered.
fn flow_mapping_keys(value: &str) -> Vec<String> {
    let inner = value
        .trim()
        .strip_prefix('{')
        .and_then(|v| v.strip_suffix('}'))
        .unwrap_or(value);
    let mut keys = Vec::new();
    let mut depth = 0i32;
    let mut start = 0usize;
    let push_entry = |entry: &str, keys: &mut Vec<String>| {
        let entry = entry.trim();
        if entry.is_empty() {
            return;
        }
        let key = entry.split_once(':').map_or(entry, |(key, _)| key);
        let key = unquote(key.trim());
        if !key.is_empty() {
            keys.push(key);
        }
    };
    for (i, ch) in inner.char_indices() {
        match ch {
            '{' | '[' => depth += 1,
            '}' | ']' => depth -= 1,
            ',' if depth == 0 => {
                push_entry(&inner[start..i], &mut keys);
                start = i + 1;
            }
            _ => {}
        }
    }
    push_entry(&inner[start..], &mut keys);
    keys
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
/// Whether `expr` mentions a label condition at all, whichever way it comes
/// out — `label_in` returns `None` both for a recognized-but-negated label
/// condition and for a condition that has nothing to do with a label, and
/// those two cases must be told apart by a caller deciding whether to
/// override an existing gate: an unrelated condition says nothing about a
/// label, so it must not erase a gate a step already earned.
fn mentions_label(expr: &str) -> bool {
    expr.to_ascii_lowercase().contains("labels.*.name")
}

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
                target: false,
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
                filter_line: None,
                target: false,
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
    fn a_quoted_on_key_is_still_read_as_a_trigger() {
        // `'on':` and `"on":` are how a YAML 1.1 author avoids `on` being
        // read as the boolean `true`; GitHub accepts both. Previously any
        // quoted key was rejected outright, so this workflow read as having
        // no `on:` block at all.
        for quoted in ["'on': pull_request", "\"on\": pull_request"] {
            let text =
                format!("name: e2e\n{quoted}\njobs:\n  run:\n    steps:\n      - run: make e2e\n");
            let workflow = classify_workflow(".github/workflows/e2e.yml", &text, &[])
                .unwrap_or_else(|| panic!("{quoted} should classify as e2e"));
            assert!(
                matches!(workflow.trigger, Trigger::PullRequest { .. }),
                "{quoted}: {:?}",
                workflow.trigger
            );
        }
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
        // `**/*.md`, not `**.md`: a bare `**` only spans directories as a
        // whole path segment, on GitHub and in `globset` alike (see
        // `github_path_matches`). Glued directly to a literal — `**.md` — it
        // is just `*.md`, which stays inside one directory and would not
        // reach `docs/a.md` at all.
        let text = "name: e2e\non:\n  pull_request:\n    paths-ignore:\n      - '**/*.md'\njobs:\n  run:\n    steps:\n      - run: make e2e\n";
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
    fn a_star_does_not_cross_a_directory_separator() {
        // The bug `github_path_matches` exists to fix: GitHub's `*` stays
        // inside one path segment, so `src/*` must not match a file nested
        // two levels deep under `src/`.
        let text = "name: e2e\non:\n  pull_request:\n    paths:\n      - 'src/*'\njobs:\n  run:\n    steps:\n      - run: make e2e\n";
        let workflow = classify_workflow(".github/workflows/e2e.yml", text, &[]).unwrap();
        assert_eq!(
            workflow.applies_to(&strings(&["src/server/routes.rs"])),
            Applies::PathsExcluded
        );
        assert_eq!(workflow.applies_to(&strings(&["src/lib.rs"])), Applies::Yes);
    }

    #[test]
    fn a_negated_pattern_carves_an_exception_out_of_an_earlier_one() {
        // GitHub evaluates `paths:` in order: `!docs/**` after `**` removes
        // `docs/` from the match rather than the first pattern's match
        // standing regardless of what comes after it.
        let text = "name: e2e\non:\n  pull_request:\n    paths:\n      - '**'\n      - '!docs/**'\njobs:\n  run:\n    steps:\n      - run: make e2e\n";
        let workflow = classify_workflow(".github/workflows/e2e.yml", text, &[]).unwrap();
        assert_eq!(
            workflow.applies_to(&strings(&["docs/readme.md"])),
            Applies::PathsExcluded
        );
        assert_eq!(workflow.applies_to(&strings(&["src/lib.rs"])), Applies::Yes);
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
    fn an_unrelated_conditional_step_does_not_gate_the_whole_job() {
        // A label-gated artifact upload ahead of an unconditional Playwright
        // step must not make the job read as gated on that label — the
        // actual e2e step runs unconditionally.
        let text = "name: e2e\non: pull_request\njobs:\n  run:\n    steps:\n      - name: Upload debug artifact\n        if: contains(github.event.pull_request.labels.*.name, 'debug')\n        uses: actions/upload-artifact@v4\n      - name: Run e2e\n        run: npx playwright test\n";
        let workflow = classify_workflow(".github/workflows/e2e.yml", text, &[]).unwrap();
        assert_eq!(
            workflow.jobs[0].label_gate, None,
            "the unconditional playwright step should not have inherited the upload step's gate"
        );
    }

    #[test]
    fn a_gate_on_the_e2e_step_itself_is_still_read() {
        let text = "name: e2e\non: pull_request\njobs:\n  run:\n    steps:\n      - name: Run e2e\n        if: contains(github.event.pull_request.labels.*.name, 'run-e2e')\n        run: npx playwright test\n";
        let workflow = classify_workflow(".github/workflows/e2e.yml", text, &[]).unwrap();
        assert_eq!(workflow.jobs[0].label_gate.as_deref(), Some("run-e2e"));
    }

    #[test]
    fn an_unconditional_e2e_step_is_not_overridden_by_a_later_gated_one() {
        // The first e2e step here is unconditional; a later, unrelated
        // label-gated e2e step must not make the job read as gated — the
        // job already runs the unconditional one regardless.
        let text = "name: e2e\non: pull_request\njobs:\n  run:\n    steps:\n      - name: Run e2e\n        run: npx playwright test\n      - name: Run flaky e2e\n        if: contains(github.event.pull_request.labels.*.name, 'run-flaky')\n        run: npx cypress run\n";
        let workflow = classify_workflow(".github/workflows/e2e.yml", text, &[]).unwrap();
        assert_eq!(
            workflow.jobs[0].label_gate, None,
            "the unconditional playwright step already decided this job runs"
        );
    }

    #[test]
    fn a_job_level_gate_outranks_a_step_level_guess_however_they_are_ordered() {
        // A job-level `if:` is the more explicit signal and must win even
        // when it happens to be written after `steps:` in the file — nothing
        // requires `if:` to come first, and an unconditional e2e step
        // ordered before it must not be read as clearing the job-level gate.
        let text = "name: e2e\non: pull_request\njobs:\n  run:\n    steps:\n      - name: Run e2e\n        run: npx playwright test\n    if: contains(github.event.pull_request.labels.*.name, 'run-e2e')\n";
        let workflow = classify_workflow(".github/workflows/e2e.yml", text, &[]).unwrap();
        assert_eq!(workflow.jobs[0].label_gate.as_deref(), Some("run-e2e"));
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
