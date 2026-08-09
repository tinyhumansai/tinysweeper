//! The reviewed repository's own config, laid over the operator's.
//!
//! Always compiled: the fetch speaks the [`ForgeRead`] port, so the whole path
//! runs offline against `MockForge`.
//!
//! # Why this exists
//!
//! Under `serve` there is no checkout of the repository being reviewed — the
//! server talks to it over the API — so [`crate::config::load`], which reads
//! `.tinysweeper.toml` off the local filesystem, reads the *server's own*
//! config for every repository it reviews. Every repository therefore got
//! tinysweeper's policy instead of its own. This module fetches the reviewed
//! repository's file through the forge instead.
//!
//! # Why it is an allow-list and not a merge
//!
//! `.tinysweeper.toml` in the reviewed repository is written by whoever opened
//! the pull request. It is untrusted input, exactly like `AGENTS.md`, with one
//! difference that makes it more dangerous: `AGENTS.md` is prose that a
//! sandboxed extractor turns into fenced advice, whereas a config key is
//! *acted on deterministically*. Merging one wholesale would hand a contributor
//! the operator's model budget, the operator's credentials by name, and the
//! operator's merge button.
//!
//! So only [`OVERRIDABLE_KEYS`] are applied and everything else is dropped and
//! reported. The line is drawn at whose interest a key serves:
//!
//! - **Overridable — how loud this repository's own review is.** Strictness and
//!   the gates it sets, the comment cap, which lanes run, which paths are
//!   reviewed, the kill-switch label names, and which instruction filenames
//!   count as policy. A repository is entitled to decide how it is reviewed,
//!   and the worst it can do with these is get a quieter review of itself.
//! - **Not overridable — anything that spends the operator's money, names the
//!   operator's secrets, writes to GitHub, or partitions shared state.**
//!   `[models]` and `[embeddings]` (model choice, `base_url`, `api_key_env`,
//!   the per-pull-request budget, and the provider/model/dimensions triple that
//!   is the index partition key — one repository changing it invalidates every
//!   other repository's vectors), `[automerge]`, `[issues]`, `[automation]`,
//!   `[sentry]` (which also names a token environment variable), and
//!   `review.request_changes_at` / `review.approve_when_clean`, which decide
//!   whether the review blocks the merge button or produces an approval that
//!   can satisfy a branch protection rule.
//! - **Not overridable — anything that puts repository prose into a prompt.**
//!   `path_instructions` is free text injected straight into a lane's
//!   instructions, unfenced. Repository prose reaches a prompt through exactly
//!   one door, the sandboxed extraction in [`crate::knowledge`], and this must
//!   not become a second one.
//! - **Not overridable — `preset`.** A preset is read from the *server's*
//!   filesystem and may set any key at all, so honouring one named by the
//!   reviewed repository would make every exclusion above reachable in one
//!   line.
//!
//! # Which commit it is read at
//!
//! The base branch's tip, not the pull request's head. Instruction files are
//! read at the head because they are sandboxed advice about the tree being
//! reviewed; a config is policy that is acted on, and reading policy from the
//! branch under review would let a pull request grade its own exam — push a
//! `.tinysweeper.toml` that disables the `security` lane in the same commit
//! that needs it. The base tip is the policy the repository has actually
//! committed to. The cost is that a pull request which *adds* or fixes a config
//! is reviewed under the old one until it merges, which is the same trade every
//! forge makes for workflow files on fork pull requests.

use toml::{Table, Value};

use crate::config::merge::{self, Layer, Provenance};
use crate::config::types::Config;
use crate::config::{CONFIG_NAMES, validate};
use crate::error::{Error, Result};
use crate::forge::types::RepoId;
use crate::ports::forge::ForgeRead;

/// Dotted keys a reviewed repository may set. A `*` matches one path segment.
///
/// Kept sorted, and every entry is covered by a test that names the reason it
/// is safe. See the module documentation for the split; changing this list is a
/// change to the security boundary in `AGENTS.md` and needs saying so in the
/// pull request.
pub const OVERRIDABLE_KEYS: [&str; 14] = [
    "knowledge.extract",
    "knowledge.files",
    "labels.human_review",
    "labels.manual_only",
    "lanes.*.fail_on",
    "paths.ignore",
    "review.confidence_min",
    "review.draft_prs",
    "review.incremental",
    "review.lanes",
    "review.max_comments",
    "review.respect_agents_md",
    "review.severity_gate",
    "review.strictness",
];

/// Whether `key` is one a reviewed repository may set.
pub fn overridable(key: &str) -> bool {
    OVERRIDABLE_KEYS
        .iter()
        .any(|pattern| pattern_matches(pattern, key))
}

/// Whether `pattern`, whose `*` segments match any one segment, matches `key`.
///
/// Segment-wise rather than a glob: a `*` that could match a separator would
/// make `lanes.*.fail_on` accept `lanes.a.b.fail_on`, and the point of the
/// wildcard is "any lane", not "any depth".
fn pattern_matches(pattern: &str, key: &str) -> bool {
    let mut pattern_segments = pattern.split('.');
    let mut key_segments = key.split('.');
    loop {
        match (pattern_segments.next(), key_segments.next()) {
            (None, None) => return true,
            (Some(expected), Some(actual)) if expected == "*" || expected == actual => {}
            _ => return false,
        }
    }
}

/// Split a repository-supplied table into what may be applied and what was not.
///
/// The second half is the ignored dotted keys, sorted, so a caller can report
/// them. Reporting matters: a key that is silently dropped looks to whoever
/// wrote it exactly like a key that did not work, and they will keep editing it.
pub fn filter(table: &Table) -> (Table, Vec<String>) {
    let mut kept = Table::new();
    let mut ignored = Vec::new();
    filter_into(table, "", &mut kept, &mut ignored);
    ignored.sort();
    (kept, ignored)
}

fn filter_into(table: &Table, prefix: &str, kept: &mut Table, ignored: &mut Vec<String>) {
    for (key, value) in table {
        let path = if prefix.is_empty() {
            key.clone()
        } else {
            format!("{prefix}.{key}")
        };

        // A table is a namespace, never a leaf, so it is descended into rather
        // than tested. That is what makes a repository writing a table where
        // the schema wants a scalar — `[review.strictness]` — fall out as
        // ignored keys instead of replacing the scalar with a table.
        if let Value::Table(inner) = value {
            let mut inner_kept = Table::new();
            filter_into(inner, &path, &mut inner_kept, ignored);
            if !inner_kept.is_empty() {
                kept.insert(key.clone(), Value::Table(inner_kept));
            }
            continue;
        }

        if overridable(&path) {
            kept.insert(key.clone(), value.clone());
        } else {
            ignored.push(path);
        }
    }
}

/// Layer a repository-supplied config document over `base`.
///
/// Returns the effective config and the keys that were ignored. Errors when the
/// document is not TOML, or when the *result* fails validation — a repository
/// that sets `strictness = 99` gets the operator's config rather than a
/// half-applied one, because a partly applied override is a configuration no
/// layer ever wrote and nobody can reason about.
pub fn apply(base: &Config, document: &str) -> Result<(Config, Vec<String>)> {
    let table = document.parse::<Table>().map_err(|err| {
        Error::config(format!("the repository's config is not valid TOML: {err}"))
    })?;
    let (allowed, ignored) = filter(&table);

    // Back to a table so the existing layered merge does the work. Serialising
    // the already-merged config is what keeps this honest: there is one merge
    // implementation, so the repository layer composes with the defaults and
    // the preset exactly as it does on the filesystem path.
    let Value::Table(mut merged) = Value::try_from(base).map_err(|err| {
        Error::config(format!(
            "the effective configuration is not representable: {err}"
        ))
    })?
    else {
        return Err(Error::config(
            "the effective configuration is not a table; this is a build-time invariant",
        ));
    };

    let mut provenance = Provenance::default();
    merge::merge_layer(&mut merged, &allowed, Layer::Repo, &mut provenance);

    let config: Config = merged
        .try_into()
        .map_err(|err| Error::config(format!("the merged configuration is not valid: {err}")))?;

    let problems = validate::validate(&config);
    if !problems.is_empty() {
        return Err(Error::config(format!(
            "the repository's config is not usable:\n{}",
            problems
                .iter()
                .map(|problem| format!("  - {problem}"))
                .collect::<Vec<_>>()
                .join("\n")
        )));
    }

    Ok((config, ignored))
}

/// What the reviewed repository contributed to the effective config.
#[derive(Debug, Clone)]
pub struct RepoOverlay {
    /// The effective configuration to review with.
    pub config: Config,
    /// The file it came from, or `None` when the repository has none and the
    /// operator's configuration is running unmodified.
    pub source: Option<String>,
    /// Keys the repository set that this deployment does not let it set.
    pub ignored: Vec<String>,
}

/// Fetch the reviewed repository's own config at `sha` and lay it over `base`.
///
/// Best-effort, like the knowledge centre and for the same reason: a forge that
/// will not answer, a file that is not TOML and a config that fails validation
/// all cost the repository its own settings and none of them cost it the
/// review. Failing here would hand any contributor a way to break the bot by
/// committing one broken line.
pub async fn overlay(
    forge: &dyn ForgeRead,
    repo: &RepoId,
    sha: &str,
    base: &Config,
) -> RepoOverlay {
    let unmodified = || RepoOverlay {
        config: base.clone(),
        source: None,
        ignored: Vec::new(),
    };

    // In `CONFIG_NAMES` order, the same order the filesystem path searches, so
    // a repository that works with the CLI works under the server.
    for name in CONFIG_NAMES {
        let document = match forge.file_at(repo, name, sha).await {
            Ok(Some(document)) => document,
            Ok(None) => continue,
            Err(err) => {
                tracing::warn!(%err, %repo, %name, "could not read the repository's config");
                return unmodified();
            }
        };

        return match apply(base, &document) {
            Ok((config, ignored)) => {
                if !ignored.is_empty() {
                    // Reported rather than dropped silently: whoever wrote these
                    // keys expects them to work, and a review that quietly
                    // ignores half a config is one nobody can debug.
                    tracing::warn!(
                        %repo,
                        %name,
                        keys = %ignored.join(", "),
                        "ignoring configuration keys a reviewed repository may not set"
                    );
                }
                RepoOverlay {
                    config,
                    source: Some(name.to_string()),
                    ignored,
                }
            }
            Err(err) => {
                tracing::warn!(%err, %repo, %name, "the repository's config is unusable; reviewing on the deployment's own");
                unmodified()
            }
        };
    }

    unmodified()
}

#[cfg(test)]
#[path = "remote_test.rs"]
mod tests;
