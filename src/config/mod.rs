//! Configuration: discovery, layered merge, and validation.
//!
//! A tinysweeper config is assembled from three layers, later winning:
//!
//! 1. [`DEFAULTS`], compiled into the binary
//! 2. the named preset, if the repository asked for one
//! 3. the repository's own `.tinysweeper.toml`
//!
//! The merge happens at the TOML level and records provenance per key, so
//! `tinysweeper doctor` can say which layer set each effective value without
//! anyone maintaining a second copy of that knowledge.

pub mod merge;
pub mod remote;
pub mod types;
pub mod validate;

#[cfg(test)]
mod test;

use std::path::{Path, PathBuf};

use toml::Table;

use crate::error::{Error, Result};

pub use crate::config::merge::{Layer, Provenance};
pub use crate::config::types::{
    AutoMerge, Automation, Cache, Config, IssueClose, Issues, Labeler, Labels, Lane, LaneId, Mcp,
    MergeMethod, ModelRef, Models, PathInstruction, Paths, Review, Sentry, Severity, Stale,
    Summary, SummarySection,
};

/// The built-in defaults, compiled in so a repository with no config at all
/// still gets a sane, conservative review.
pub const DEFAULTS: &str = include_str!("defaults.toml");

/// Config file names, in the order they are searched for in a directory.
pub const CONFIG_NAMES: [&str; 2] = [".tinysweeper.toml", ".github/tinysweeper.toml"];

/// Environment variable overriding where presets are read from. Set in the
/// Docker image, where presets ship at a fixed path outside the checkout.
pub const PRESETS_DIR_ENV: &str = "TINYSWEEPER_PRESETS_DIR";

/// A config file located on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Located {
    /// The file that was found.
    pub path: PathBuf,
}

/// The result of loading a config: the effective values plus where each came
/// from.
#[derive(Debug, Clone)]
pub struct Loaded {
    /// The effective configuration.
    pub config: Config,
    /// Which layer set each key.
    pub provenance: Provenance,
    /// The repository config file, if one was found. Absent means the defaults
    /// (plus any preset) are running unmodified.
    pub source: Option<PathBuf>,
    /// The preset file that was merged, if the config named one.
    pub preset_source: Option<PathBuf>,
}

/// Find a config file at or under `path`.
///
/// `path` may be the file itself or a directory to search. Returns `None` when
/// a directory contains no config — which is not an error: running on defaults
/// is a supported mode.
pub fn discover(path: &Path) -> Result<Option<Located>> {
    if path.is_file() {
        return Ok(Some(Located {
            path: path.to_path_buf(),
        }));
    }

    if !path.is_dir() {
        return Err(Error::path(path, "no such file or directory"));
    }

    for name in CONFIG_NAMES {
        let candidate = path.join(name);
        if candidate.is_file() {
            return Ok(Some(Located { path: candidate }));
        }
    }

    Ok(None)
}

/// Load the effective configuration for the repository rooted at `root`.
///
/// `explicit` overrides discovery when the caller passed `--config`.
pub fn load(root: &Path, explicit: Option<&Path>) -> Result<Loaded> {
    let located = match explicit {
        Some(path) => Some(discover(path)?.ok_or_else(|| {
            Error::ConfigNotFound(format!("{} contains no tinysweeper config", path.display()))
        })?),
        None => discover(root)?,
    };

    let repo_table = match &located {
        Some(found) => Some(read_table(&found.path)?),
        None => None,
    };

    // The preset is named by the repository's own file, so it has to be read
    // before the merge can be assembled.
    let preset_name = repo_table
        .as_ref()
        .and_then(|t| t.get("preset"))
        .and_then(|v| v.as_str())
        .map(str::to_string);

    let mut merged: Table = DEFAULTS
        .parse()
        .expect("built-in defaults must parse; this is a build-time invariant");
    let mut provenance = Provenance::default();
    merge::record_all(&merged, Layer::Defaults, &mut provenance);

    let mut preset_source = None;
    if let Some(name) = &preset_name {
        let path = resolve_preset(root, name)?;
        let table = read_table(&path)?;
        merge::merge_layer(&mut merged, &table, Layer::Preset, &mut provenance);
        preset_source = Some(path);
    }

    if let Some(table) = &repo_table {
        merge::merge_layer(&mut merged, table, Layer::Repo, &mut provenance);
    }

    let unknown = unknown_keys(&merged);
    if !unknown.is_empty() {
        return Err(Error::config(format!(
            "{} unknown configuration key{}:\n{}",
            unknown.len(),
            if unknown.len() == 1 { "" } else { "s" },
            unknown
                .iter()
                .map(|key| format!("  - `{key}`"))
                .collect::<Vec<_>>()
                .join("\n")
        )));
    }

    let mut config: Config = merged.try_into().map_err(|err| {
        Error::config(format!(
            "the merged configuration is not valid: {err}\n\
             (this usually means an unknown key; run `tinysweeper check` for details)"
        ))
    })?;

    load_rule_documents(root, &mut config)?;

    Ok(Loaded {
        config,
        provenance,
        source: located.map(|l| l.path),
        preset_source,
    })
}

/// Collect every key Serde's `deny_unknown_fields` would otherwise report one
/// at a time. The schema names configuration *tables*, not values, so maps
/// such as `automation.labeler.area` remain deliberately open.
fn unknown_keys(table: &Table) -> Vec<String> {
    let mut unknown = Vec::new();
    collect_unknown_keys(table, "", &mut unknown);
    unknown
}

fn collect_unknown_keys(table: &Table, path: &str, unknown: &mut Vec<String>) {
    let Some(known) = known_keys(path) else {
        return;
    };

    for (key, value) in table {
        let key_path = join_key(path, key);
        // `lanes` is the one keyed table: its entry name is a lane id, while
        // the fields inside every entry still have a closed schema.
        if path != "lanes" && !known.contains(&key.as_str()) {
            unknown.push(key_path);
            continue;
        }

        match value {
            toml::Value::Table(child) => collect_unknown_keys(child, &key_path, unknown),
            toml::Value::Array(items) => {
                for (index, item) in items.iter().enumerate() {
                    if let toml::Value::Table(child) = item {
                        collect_unknown_keys(child, &format!("{key_path}[{index}]"), unknown);
                    }
                }
            }
            _ => {}
        }
    }
}

fn join_key(path: &str, key: &str) -> String {
    if path.is_empty() {
        key.to_owned()
    } else {
        format!("{path}.{key}")
    }
}

/// Keys accepted in one table. An absent entry represents an intentionally
/// open map, while `*` stands for an entry in a keyed table or array of tables.
fn known_keys(path: &str) -> Option<&'static [&'static str]> {
    let parts = path
        .split('.')
        .map(|part| part.split_once('[').map_or(part, |(key, _)| key))
        .collect::<Vec<_>>();
    let path = match parts.as_slice() {
        ["lanes", _] => "lanes.*".to_owned(),
        ["path_instructions"] => "path_instructions.*".to_owned(),
        ["memory", "questions"] => "memory.questions.*".to_owned(),
        ["council", "agents"] => "council.agents.*".to_owned(),
        ["sentry", "route"] => "sentry.route.*".to_owned(),
        ["models", "routes"] => "models.routes.*".to_owned(),
        _ => parts.join("."),
    };
    match path.as_str() {
        "" => Some(&[
            "version",
            "preset",
            "review",
            "paths",
            "path_instructions",
            "cache",
            "labels",
            "models",
            "knowledge",
            "embeddings",
            "retrieval",
            "memory",
            "mcp",
            "lanes",
            "council",
            "lookup",
            "grouping",
            "automerge",
            "threads",
            "overview",
            "summary",
            "issues",
            "pr_triage",
            "automation",
            "sentry",
            "preview",
        ]),
        "review" => Some(&[
            "lanes",
            "strictness",
            "severity_gate",
            "confidence_min",
            "max_comments",
            "max_changed_files",
            "max_changed_lines",
            "note_confidence",
            "incremental",
            "draft_prs",
            "respect_agents_md",
            "request_changes_at",
            "approve_when_clean",
            "passes",
        ]),
        "threads" => Some(&["resolve_fixed", "ask_model", "comment_on_resolve"]),
        "overview" => Some(&[
            "enabled",
            "max_components",
            "max_impacted",
            "max_links",
            "max_paths_per_component",
        ]),
        "summary" => Some(&[
            "enabled",
            "sections",
            "max_features",
            "max_tests",
            "history_entries",
        ]),
        "paths" => Some(&["ignore"]),
        "path_instructions.*" => Some(&["glob", "instructions", "rules", "lanes", "merge"]),
        "cache" => Some(&["enabled", "semantic", "max_age_days"]),
        "labels" => Some(&["human_review", "manual_only"]),
        "models" => Some(&[
            "gateway",
            "base_url",
            "api_key_env",
            "scan",
            "deep",
            "flash",
            "fallback",
            "provider",
            "max_tokens",
            "reasoning_effort",
            "structured_output",
            "budget_usd_per_pr",
            "routes",
        ]),
        "models.routes.*" => Some(&["model", "order", "allow_fallbacks", "max_tokens"]),
        "models.provider" => Some(&[
            "order",
            "allow_fallbacks",
            "last_resort_unpinned",
            "unpinned_vendors",
        ]),
        "knowledge" => Some(&[
            "extract",
            "files",
            "max_file_bytes",
            "pinned_doc_chars",
            "pinned_total_chars",
        ]),
        "embeddings" => Some(&[
            "enabled",
            "provider",
            "model",
            "dimensions",
            "api_key_env",
            "base_url",
            "batch",
            "max_request_tokens",
            "requests_per_minute",
            "budget_usd_per_index",
        ]),
        "retrieval" => Some(&[
            "enabled",
            "query_chars",
            "context_tokens",
            "max_chunks",
            "graph_hops",
            "max_graph_nodes",
            "max_impact",
            "submodules",
        ]),
        "memory" => Some(&[
            "enabled",
            "provider",
            "endpoint",
            "api_key_env",
            "allow_private_http",
            "ingest_code",
            "ingest_conventions",
            "ingest_discussions",
            "convention_files",
            "convention_section_chars",
            "discussion_chars",
            "discussion_debounce_secs",
            "remember_reviews",
            "context_tokens",
            "max_recollections",
            "ask",
            "questions",
            "answer_chars",
            "answer_model",
            "query_terms",
        ]),
        "mcp" => Some(&["enabled", "token_env", "allowed_org"]),
        "memory.questions.*" => Some(&["section", "ask"]),
        "lanes" => Some(&[]),
        "lanes.*" => Some(&[
            "model",
            "fail_on",
            "secret_rulepack",
            "max_blob_bytes",
            "missing_harness",
            "paths",
            "workflows",
        ]),
        "council" => Some(&["enabled", "corroboration", "subagents", "agents"]),
        "council.agents.*" => Some(&["id", "lanes", "model", "persona"]),
        "lookup" => Some(&["enabled", "rounds", "per_round", "max_chars", "checkout"]),
        "grouping" => Some(&["enabled", "max_files", "max_hunk_chars"]),
        "automerge" => Some(&[
            "enabled",
            "require_checks",
            "require_approvals",
            "method",
            "allow_labels",
            "block_labels",
            "max_files",
            "max_changed_lines",
            "max_hunks",
            "max_directories",
            "sensitive_paths",
            "allow_dependency_bumps",
            "dependency_bots",
            "dependency_paths",
        ]),
        "issues" => Some(&[
            "enabled",
            "model",
            "comment",
            "apply_labels",
            "max_labels",
            "apply_issue_type",
            "allow_labels",
            "block_labels",
            "dedupe",
            "dedupe_confidence_min",
            "close",
        ]),
        "issues.close" => Some(&[
            "enabled",
            "min_age_days",
            "quiet_days",
            "confidence_min",
            "protected_labels",
            "protected_authors",
            "dry_run",
        ]),
        "pr_triage" => Some(&[
            "enabled",
            "max_pull_requests",
            "max_landed_files",
            "min_landed_lines",
            "max_base_reads",
            "duplicate_path_overlap_min",
            "duplicate_line_overlap_min",
            "comment",
            "apply_labels",
            "flag_promotional",
            "sweep_every_minutes",
            "sweep_repositories",
            "close",
        ]),
        "pr_triage.close" => Some(&[
            "enabled",
            "min_age_days",
            "quiet_days",
            "protected_labels",
            "protected_authors",
            "dry_run",
        ]),
        "automation" => Some(&[
            "enabled",
            "stale",
            "labeler",
            "merge_sweep",
            "nudge_after_days",
        ]),
        "automation.stale" => Some(&[
            "enabled",
            "days_until_stale",
            "days_until_close",
            "label",
            "exempt_labels",
        ]),
        "automation.labeler" => Some(&["enabled", "size", "area"]),
        "automation.labeler.area" => None,
        "sentry" => Some(&[
            "enabled",
            "org",
            "projects",
            "token_env",
            "base_url",
            "min_events",
            "min_users",
            "ignore_culprits",
            "labels",
            "max_per_run",
            "annotate_sentry",
            "resolve_when_tracked",
            "scrub_patterns",
            "route",
        ]),
        "sentry.route.*" => Some(&["project", "repo", "labels"]),
        "preview" => Some(&[
            "enabled",
            "public_base_url",
            "max_flows",
            "max_steps",
            "budget_usd",
            "caption",
        ]),
        _ => Some(&[]),
    }
}

/// Inline every `path_instructions.rules` document into its instructions.
///
/// Resolved at load time rather than at prompt time so a missing or misnamed
/// rule document is a configuration error a human sees once, rather than a
/// silently weaker review every time the bot runs.
fn load_rule_documents(root: &Path, config: &mut Config) -> Result<()> {
    for rule in &mut config.path_instructions {
        let Some(name) = rule.rules.clone() else {
            continue;
        };
        let path = resolve_rules(root, &name)?;
        let text = std::fs::read_to_string(&path).map_err(|err| Error::path(&path, err))?;

        if rule.instructions.trim().is_empty() {
            rule.instructions = text;
        } else {
            rule.instructions = format!("{}\n\n{text}", rule.instructions.trim_end());
        }
    }
    Ok(())
}

/// Locate `presets/rules/<name>.md`, searched exactly like a preset.
fn resolve_rules(root: &Path, name: &str) -> Result<PathBuf> {
    if name.contains("..") || name.contains('/') || name.contains('\\') {
        return Err(Error::config(format!(
            "`rules = \"{name}\"` is not a rule document name; it must not contain a path separator"
        )));
    }

    let mut searched = Vec::new();
    for dir in preset_dirs(root) {
        let candidate = dir.join("rules").join(format!("{name}.md"));
        if candidate.is_file() {
            return Ok(candidate);
        }
        searched.push(candidate);
    }

    Err(Error::config(format!(
        "rule document `{name}` not found; looked in:\n{}",
        searched
            .iter()
            .map(|p| format!("  - {}", p.display()))
            .collect::<Vec<_>>()
            .join("\n")
    )))
}

/// Load and validate in one step, turning any problems into a single error
/// listing all of them.
pub fn load_validated(root: &Path, explicit: Option<&Path>) -> Result<Loaded> {
    let loaded = load(root, explicit)?;
    let problems = validate::validate(&loaded.config);
    if problems.is_empty() {
        return Ok(loaded);
    }

    let where_from = loaded
        .source
        .as_ref()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "built-in defaults".to_string());

    Err(Error::config(format!(
        "{} problem{} in {where_from}:\n{}",
        problems.len(),
        if problems.len() == 1 { "" } else { "s" },
        problems
            .iter()
            .map(|p| format!("  - {p}"))
            .collect::<Vec<_>>()
            .join("\n")
    )))
}

/// Locate `presets/<name>/preset.toml`.
///
/// Searched, in order: `$TINYSWEEPER_PRESETS_DIR`, then `presets/` under the
/// repository root. The error lists everywhere it looked, because "preset not
/// found" with no paths is the least useful message a tool can produce.
fn resolve_preset(root: &Path, name: &str) -> Result<PathBuf> {
    if name.contains("..") || name.contains('/') || name.contains('\\') {
        return Err(Error::config(format!(
            "`preset = \"{name}\"` is not a preset name; it must not contain a path separator"
        )));
    }

    let mut searched = Vec::new();
    for dir in preset_dirs(root) {
        let candidate = dir.join(name).join("preset.toml");
        if candidate.is_file() {
            return Ok(candidate);
        }
        searched.push(candidate);
    }

    Err(Error::config(format!(
        "preset `{name}` not found; looked in:\n{}",
        searched
            .iter()
            .map(|p| format!("  - {}", p.display()))
            .collect::<Vec<_>>()
            .join("\n")
    )))
}

fn preset_dirs(root: &Path) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Ok(dir) = std::env::var(PRESETS_DIR_ENV)
        && !dir.trim().is_empty()
    {
        dirs.push(PathBuf::from(dir));
    }
    dirs.push(root.join("presets"));
    dirs
}

fn read_table(path: &Path) -> Result<Table> {
    let text = std::fs::read_to_string(path).map_err(|err| Error::path(path, err))?;
    text.parse::<Table>()
        .map_err(|err| Error::config(format!("{}: {err}", path.display())))
}
