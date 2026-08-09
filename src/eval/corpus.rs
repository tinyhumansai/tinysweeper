//! Loading and validating `evals/`.
//!
//! The corpus is **data**, the way `presets/` is data, and it is loaded by a CLI
//! subcommand rather than by `cargo test`. A labelled pull request is not a unit
//! test: it costs money to run, its answer moves, and it is edited by whoever
//! disagrees with a score. Putting it under `cargo test` would make `cargo test`
//! need a key.
//!
//! # Every problem at once
//!
//! [`load`] collects every invalid case before returning, the way
//! `config::validate` does. Fixing a corpus one error per run is how people
//! stop fixing it.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::error::{Error, Result};
use crate::eval::types::{Case, Fixture, SCHEMA};
use crate::forge::mock::{MockForge, MockState};

/// A case, its fixture, and where both came from.
#[derive(Debug, Clone)]
pub struct LoadedCase {
    /// The labelled case.
    pub case: Case,
    /// The frozen forge state.
    pub fixture: Fixture,
    /// The case file, for error messages.
    pub path: PathBuf,
}

impl LoadedCase {
    /// Build the forge this case is reviewed against.
    pub fn forge(&self) -> MockForge {
        let mut state = MockState::default();
        for (path, content) in &self.fixture.blobs {
            state.set_file(&self.fixture.pull_request.head_sha, path, content);
        }
        MockForge::with_state(state)
            .with_pull_request(
                self.fixture.pull_request.clone(),
                self.fixture.files.clone(),
                self.fixture.commits.clone(),
            )
            .with_comments(
                self.fixture.pull_request.number,
                self.fixture.comments.clone(),
            )
            // Read-only, belt and braces. A lane already cannot write — it is
            // handed a `ForgeRead` — but the eval runner constructs the forge
            // itself, and a corpus run must never be the thing that discovers
            // a write path exists.
            .read_only()
    }

    /// Where this case's cassette lives, under `root`.
    pub fn cassette_dir(&self, root: &Path) -> PathBuf {
        root.join("cassettes").join(&self.case.id)
    }
}

/// The whole corpus, in id order.
#[derive(Debug, Clone)]
pub struct Corpus {
    /// The `evals/` directory.
    pub root: PathBuf,
    /// Every case, sorted by id so two runs enumerate identically.
    pub cases: Vec<LoadedCase>,
    /// A hash over every case file, so a report can refuse to diff against a
    /// baseline scored on different labels.
    pub digest: String,
}

impl Corpus {
    /// Keep only the named cases, erroring on a name that is not in the corpus.
    pub fn select(mut self, ids: &[String]) -> Result<Self> {
        if ids.is_empty() {
            return Ok(self);
        }
        let known: BTreeSet<&str> = self.cases.iter().map(|c| c.case.id.as_str()).collect();
        let unknown: Vec<&String> = ids
            .iter()
            .filter(|id| !known.contains(id.as_str()))
            .collect();
        if !unknown.is_empty() {
            return Err(Error::config(format!(
                "no such case: {}. The corpus holds: {}",
                unknown
                    .iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join(", "),
                known.into_iter().collect::<Vec<_>>().join(", "),
            )));
        }
        self.cases.retain(|c| ids.contains(&c.case.id));
        Ok(self)
    }
}

/// Read every case under `root/cases`, reporting every problem at once.
pub fn load(root: &Path) -> Result<Corpus> {
    let dir = root.join("cases");
    let entries = std::fs::read_dir(&dir)
        .map_err(|err| Error::path(&dir, format!("no corpus here ({err})")))?;

    let mut paths: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "toml"))
        .collect();
    paths.sort();

    let mut cases = Vec::new();
    let mut problems = Vec::new();
    let mut hasher = Sha256::new();

    for path in paths {
        let raw = match std::fs::read_to_string(&path) {
            Ok(raw) => raw,
            Err(err) => {
                problems.push(format!("{}: {err}", path.display()));
                continue;
            }
        };
        // Hash the bytes on disk, not the parsed struct: a comment explaining
        // why an expectation is worded the way it is changes what a reviewer
        // of the corpus sees, and two baselines from different labels must not
        // silently compare.
        hasher.update(raw.as_bytes());

        let case: Case = match toml::from_str(&raw) {
            Ok(case) => case,
            Err(err) => {
                problems.push(format!("{}: {err}", path.display()));
                continue;
            }
        };

        match validate(&case, &path) {
            Ok(()) => {}
            Err(reasons) => {
                problems.extend(reasons);
                continue;
            }
        }

        let fixture_path = path.parent().unwrap_or(Path::new(".")).join(&case.fixture);
        // The fixture path is data in a case file. `../fixtures/` is how every
        // case reaches its frozen forge state, but `../../..` would walk out of
        // the corpus and read an operator's files into the model. Resolving
        // both sides against the filesystem keeps every `..` honest.
        if let (Ok(root), Ok(fixture)) = (
            std::fs::canonicalize(&root),
            std::fs::canonicalize(&fixture_path),
        ) && !fixture.starts_with(&root)
        {
            problems.push(format!(
                "{} ({}): fixture `{}` resolves outside the corpus",
                path.display(),
                case.id,
                case.fixture
            ));
            continue;
        }
        let fixture = match read_fixture(&fixture_path) {
            Ok(fixture) => fixture,
            Err(err) => {
                problems.push(format!("{} ({}): {err}", path.display(), case.id));
                continue;
            }
        };

        cases.push(LoadedCase {
            case,
            fixture,
            path,
        });
    }

    if !problems.is_empty() {
        return Err(Error::config(format!(
            "{} problem(s) in {}:\n  - {}",
            problems.len(),
            dir.display(),
            problems.join("\n  - ")
        )));
    }

    let duplicates = duplicate_ids(&cases);
    if !duplicates.is_empty() {
        return Err(Error::config(format!(
            "duplicate case id(s): {}. The id keys the cassette directory and the report, \
             so two cases sharing one would overwrite each other's recording.",
            duplicates.join(", ")
        )));
    }

    cases.sort_by(|a, b| a.case.id.cmp(&b.case.id));
    let digest = hasher
        .finalize()
        .iter()
        .take(8)
        .map(|byte| format!("{byte:02x}"))
        .collect();

    Ok(Corpus {
        root: root.to_path_buf(),
        cases,
        digest,
    })
}

/// Case ids that appear more than once.
fn duplicate_ids(cases: &[LoadedCase]) -> Vec<String> {
    let mut seen = BTreeSet::new();
    let mut duplicated = BTreeSet::new();
    for case in cases {
        if !seen.insert(case.case.id.clone()) {
            duplicated.insert(case.case.id.clone());
        }
    }
    duplicated.into_iter().collect()
}

/// Everything wrong with one case.
fn validate(case: &Case, path: &Path) -> std::result::Result<(), Vec<String>> {
    let mut problems = Vec::new();
    let at = |message: String| format!("{} ({}): {message}", path.display(), case.id);

    if case.schema != SCHEMA {
        problems.push(at(format!(
            "schema {}, but this build reads schema {SCHEMA}",
            case.schema
        )));
    }
    if case.id.trim().is_empty() {
        problems.push(at("id is empty".into()));
    }
    if case.id.contains(['/', '\\']) || case.id.contains("..") {
        // The id becomes a directory name under `cassettes/`.
        problems.push(at("id must not contain a path separator or `..`".into()));
    }

    // The rule that keeps the corpus honest. An expectation justified by the
    // bot's own output measures whether the bot still agrees with itself.
    if case.provenance.evidence.trim().is_empty() {
        problems.push(at(
            "provenance.evidence is empty. Every expectation needs a source outside this bot \
             — a follow-up fix, an acted-on review comment, a revert — or the corpus just \
             records today's blind spots as the target."
                .into(),
        ));
    }
    if case.provenance.labelled_by.trim().is_empty() {
        problems.push(at("provenance.labelled_by is empty".into()));
    }

    let mut ids = BTreeSet::new();
    for expected in &case.expected {
        if !ids.insert(expected.id.clone()) {
            problems.push(at(format!("duplicate expectation id `{}`", expected.id)));
        }
        if let Some((start, end)) = expected.lines
            && start > end
        {
            problems.push(at(format!(
                "expectation `{}` has lines = [{start}, {end}], which is backwards",
                expected.id
            )));
        }
        if expected
            .must_mention
            .iter()
            .any(|slot| slot.trim().is_empty())
        {
            problems.push(at(format!(
                "expectation `{}` has an empty must_mention slot, which every finding satisfies",
                expected.id
            )));
        }
    }

    for forbidden in &case.forbidden {
        if !ids.insert(forbidden.id.clone()) {
            problems.push(at(format!("duplicate id `{}`", forbidden.id)));
        }
        if forbidden.reason.trim().is_empty() {
            problems.push(at(format!(
                "forbidden `{}` has no reason. The next person to read it will assume it is \
                 a mistake and delete it.",
                forbidden.id
            )));
        }
        if !forbidden.is_constrained() {
            problems.push(at(format!(
                "forbidden `{}` narrows nothing — no path, lane, line range or keyword — so it \
                 rules out every finding on the case",
                forbidden.id
            )));
        }
    }

    if problems.is_empty() {
        Ok(())
    } else {
        Err(problems)
    }
}

/// Read one frozen fixture.
fn read_fixture(path: &Path) -> Result<Fixture> {
    let raw = std::fs::read_to_string(path).map_err(|err| Error::path(path, err))?;
    let fixture: Fixture = serde_json::from_str(&raw)
        .map_err(|err| Error::path(path, format!("not a fixture: {err}")))?;
    if fixture.pull_request.head_sha.trim().is_empty() {
        return Err(Error::path(
            path,
            "the fixture has no head sha; every finding's identity is keyed on it",
        ));
    }
    Ok(fixture)
}

#[cfg(test)]
#[path = "corpus_test.rs"]
mod tests;
