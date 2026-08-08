//! `tinysweeper check` and `tinysweeper doctor`.
//!
//! `check` answers "is this config usable" and reports every problem at once.
//! `doctor` answers the harder question — "what is actually going to happen"  —
//! by rendering the effective values, which layer set each one, and which
//! credentials are present. Both read from the same merge, so neither can
//! describe a configuration the other would not produce.

use std::path::Path;

use serde_json::json;

use crate::config::{self, Layer, Loaded};
use crate::error::{Error, Result};

/// Validate the config at `path`, printing a report.
///
/// Returns an error when the config is unusable, so the process exits non-zero
/// and CI fails.
pub fn check(path: &Path) -> Result<()> {
    let (root, explicit) = split_target(path);
    let loaded = config::load(&root, explicit.as_deref())?;
    let problems = config::validate::validate(&loaded.config);

    let where_from = loaded
        .source
        .as_ref()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "built-in defaults (no config file found)".to_string());

    if problems.is_empty() {
        println!("ok  {where_from}");
        if let Some(preset) = &loaded.preset_source {
            println!("    preset: {}", preset.display());
        }
        let lanes = loaded.config.enabled_lanes();
        println!(
            "    {} lane{}: {}",
            lanes.len(),
            if lanes.len() == 1 { "" } else { "s" },
            lanes
                .iter()
                .map(|l| l.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
        return Ok(());
    }

    println!("{where_from}");
    for problem in &problems {
        println!("  - {problem}");
    }

    Err(Error::config(format!(
        "{} problem{} found",
        problems.len(),
        if problems.len() == 1 { "" } else { "s" }
    )))
}

/// Report the effective configuration and where each value came from.
pub fn doctor(path: &Path, as_json: bool) -> Result<()> {
    let (root, explicit) = split_target(path);
    let loaded = config::load(&root, explicit.as_deref())?;

    if as_json {
        print_json(&loaded)?;
    } else {
        print_prose(&loaded);
    }

    Ok(())
}

fn print_json(loaded: &Loaded) -> Result<()> {
    let provenance: serde_json::Map<String, serde_json::Value> = loaded
        .provenance
        .iter()
        .map(|(key, layer)| (key.to_string(), json!(layer.as_str())))
        .collect();

    let report = json!({
        "source": loaded.source.as_ref().map(|p| p.display().to_string()),
        "preset": loaded.preset_source.as_ref().map(|p| p.display().to_string()),
        "config": loaded.config,
        "provenance": provenance,
        "problems": config::validate::validate(&loaded.config),
        "credentials": credentials(loaded),
    });

    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

fn print_prose(loaded: &Loaded) {
    let config = &loaded.config;

    println!("config");
    println!(
        "  file    {}",
        loaded
            .source
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "<none — running on built-in defaults>".into())
    );
    if let Some(preset) = &loaded.preset_source {
        println!("  preset  {}", preset.display());
    }

    println!("\nreview");
    println!(
        "  lanes            {}",
        config
            .enabled_lanes()
            .iter()
            .map(|l| l.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    );
    println!("  strictness       {}", config.review.strictness);
    println!("  severity gate    {}", config.severity_gate());
    println!("  confidence min   {}", config.review.confidence_min);
    println!("  max comments     {}", config.review.max_comments);
    println!("  incremental      {}", config.review.incremental);

    println!("\nmodels");
    println!("  gateway          {}", config.models.gateway);
    println!("  base url         {}", config.models.base_url);
    println!("  scan tier        {}", config.models.scan);
    println!("  deep tier        {}", config.models.deep);
    println!("  budget per PR    ${:.2}", config.models.budget_usd_per_pr);
    for lane in config.enabled_lanes() {
        println!(
            "  {:<16} {}  (fails at {})",
            lane.as_str(),
            config.model_for(lane),
            config.fail_on(lane)
        );
    }

    println!("\ncapabilities");
    for (name, enabled) in [
        ("automerge", config.automerge.enabled),
        ("issue triage", config.issues.enabled),
        ("issue closing", config.issues.close.enabled),
        ("automation", config.automation.enabled),
        ("stale sweep", config.automation.stale.enabled),
        ("sentry promotion", config.sentry.enabled),
    ] {
        println!("  {:<16} {}", name, if enabled { "on" } else { "off" });
    }

    println!("\ncredentials");
    for (variable, present, needed_for) in credentials(loaded) {
        println!(
            "  {:<24} {:<8} {}",
            variable,
            if present { "present" } else { "MISSING" },
            needed_for
        );
    }

    // The point of doctor is to explain surprises, and the surprising values
    // are exactly the ones a preset or the repository moved off the default.
    let overridden: Vec<_> = loaded.provenance.overridden().collect();
    println!(
        "\noverridden ({} of {})",
        overridden.len(),
        loaded.provenance.len()
    );
    if overridden.is_empty() {
        println!("  <nothing — every value is a built-in default>");
    } else {
        for (key, layer) in overridden {
            println!("  {:<40} set by {}", key, layer);
        }
    }

    let problems = config::validate::validate(&loaded.config);
    if problems.is_empty() {
        println!("\nno problems");
    } else {
        println!("\n{} problem(s)", problems.len());
        for problem in problems {
            println!("  - {problem}");
        }
    }
}

/// Which credentials this configuration needs, and whether they are present.
///
/// Only presence is reported. A value never is, and never should be.
fn credentials(loaded: &Loaded) -> Vec<(String, bool, &'static str)> {
    let config = &loaded.config;
    let mut wanted: Vec<(String, &'static str)> = vec![
        (config.models.api_key_env.clone(), "model calls"),
        ("GITHUB_TOKEN".to_string(), "reading and posting to GitHub"),
    ];

    if config.sentry.enabled {
        wanted.push((config.sentry.token_env.clone(), "Sentry promotion"));
    }

    wanted
        .into_iter()
        .map(|(variable, needed_for)| {
            let present = std::env::var(&variable)
                .map(|value| !value.trim().is_empty())
                .unwrap_or(false);
            (variable, present, needed_for)
        })
        .collect()
}

/// Split a user-supplied target into a repository root and an explicit config
/// path.
///
/// `check path/to/file.toml` should validate that file while still resolving
/// presets relative to the repository it sits in, so a file target contributes
/// both halves.
fn split_target(path: &Path) -> (std::path::PathBuf, Option<std::path::PathBuf>) {
    if path.is_file() {
        let root = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .map(Path::to_path_buf)
            .unwrap_or_else(|| std::path::PathBuf::from("."));
        // A config discovered inside .github/ belongs to the repository above
        // it, not to .github/ — otherwise presets would never be found.
        let root = if root.file_name().map(|n| n == ".github").unwrap_or(false) {
            root.parent().map(Path::to_path_buf).unwrap_or(root)
        } else {
            root
        };
        (root, Some(path.to_path_buf()))
    } else {
        (path.to_path_buf(), None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo(config: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join(".tinysweeper.toml"), config).expect("write");
        dir
    }

    #[test]
    fn check_passes_a_valid_config() {
        let dir = repo("version = 1\n[review]\nstrictness = 3\n");
        check(dir.path()).expect("valid");
    }

    #[test]
    fn check_fails_and_counts_the_problems() {
        let dir = repo("version = 1\n[review]\nstrictness = 9\nmax_comments = 0\n");
        let err = check(dir.path()).unwrap_err().to_string();
        assert!(err.contains("2 problems"), "{err}");
    }

    #[test]
    fn check_accepts_a_repository_with_no_config_at_all() {
        let dir = tempfile::tempdir().expect("tempdir");
        check(dir.path()).expect("defaults are valid");
    }

    #[test]
    fn doctor_runs_in_both_output_modes() {
        let dir = repo("version = 1\n");
        doctor(dir.path(), false).expect("prose");
        doctor(dir.path(), true).expect("json");
    }

    #[test]
    fn a_file_target_resolves_presets_against_its_repository() {
        let dir = tempfile::tempdir().expect("tempdir");
        let preset_dir = dir.path().join("presets").join("house");
        std::fs::create_dir_all(&preset_dir).expect("mkdir");
        std::fs::write(preset_dir.join("preset.toml"), "version = 1\n").expect("write preset");

        let config_path = dir.path().join("custom.toml");
        std::fs::write(&config_path, "version = 1\npreset = \"house\"\n").expect("write");

        check(&config_path).expect("preset resolved relative to the repository root");
    }

    #[test]
    fn a_config_under_dot_github_resolves_presets_from_the_repository_root() {
        let dir = tempfile::tempdir().expect("tempdir");
        let preset_dir = dir.path().join("presets").join("house");
        std::fs::create_dir_all(&preset_dir).expect("mkdir");
        std::fs::write(preset_dir.join("preset.toml"), "version = 1\n").expect("write preset");

        std::fs::create_dir_all(dir.path().join(".github")).expect("mkdir");
        let config_path = dir.path().join(".github/tinysweeper.toml");
        std::fs::write(&config_path, "version = 1\npreset = \"house\"\n").expect("write");

        check(&config_path).expect("preset resolved from above .github/");
    }

    #[test]
    fn credentials_report_presence_only() {
        let dir = repo("version = 1\n");
        let loaded = config::load(dir.path(), None).expect("loads");
        let reported = credentials(&loaded);

        assert!(
            reported
                .iter()
                .any(|(var, _, _)| var == "OPENROUTER_API_KEY")
        );
        assert!(reported.iter().any(|(var, _, _)| var == "GITHUB_TOKEN"));
    }

    #[test]
    fn sentry_credentials_are_only_wanted_when_sentry_is_on() {
        let dir = repo("version = 1\n");
        let loaded = config::load(dir.path(), None).expect("loads");
        assert!(
            !credentials(&loaded)
                .iter()
                .any(|(var, _, _)| var == "SENTRY_AUTH_TOKEN")
        );

        let dir =
            repo("version = 1\n[sentry]\nenabled = true\norg = \"acme\"\nprojects = [\"api\"]\n");
        let loaded = config::load(dir.path(), None).expect("loads");
        assert!(
            credentials(&loaded)
                .iter()
                .any(|(var, _, _)| var == "SENTRY_AUTH_TOKEN")
        );
    }

    #[test]
    fn layers_are_reported_for_overridden_keys_only() {
        let dir = repo("version = 1\n[review]\nstrictness = 3\n");
        let loaded = config::load(dir.path(), None).expect("loads");

        let overridden: Vec<_> = loaded.provenance.overridden().collect();
        // `version` is restated by every config file, so it is an override too;
        // the interesting entry is the one the author actually changed.
        assert!(overridden.contains(&("review.strictness", Layer::Repo)));
        assert!(
            !overridden
                .iter()
                .any(|(key, _)| *key == "review.max_comments"),
            "untouched defaults must not be reported as overridden: {overridden:?}"
        );
    }
}
