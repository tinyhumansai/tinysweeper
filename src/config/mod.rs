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
pub mod types;
pub mod validate;

#[cfg(test)]
mod test;

use std::path::{Path, PathBuf};

use toml::Table;

use crate::error::{Error, Result};

pub use crate::config::merge::{Layer, Provenance};
pub use crate::config::types::{
    AutoMerge, Automation, Cache, Config, IssueClose, Issues, Labeler, Labels, Lane, LaneId,
    MergeMethod, ModelRef, Models, PathInstruction, Paths, Review, Sentry, Severity, Stale,
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

    let mut config: Config = merged.try_into().map_err(|err| {
        Error::config(format!(
            "the merged configuration is not valid: {err}\n\
             (this usually means an unknown key; run `tinysweeper check` for details)"
        ))
    })?;

    Ok(Loaded {
        config,
        provenance,
        source: located.map(|l| l.path),
        preset_source,
    })
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
