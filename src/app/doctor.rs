//! `tinysweeper check` and `tinysweeper doctor`.
//!
//! `check` answers "is this config usable" and reports every problem at once.
//! `doctor` answers the harder question — "what is actually going to happen"  —
//! by rendering the effective values, which layer set each one, and which
//! credentials are present. Both read from the same merge, so neither can
//! describe a configuration the other would not produce.

use std::path::Path;

use serde_json::json;

use crate::config::types::Config;
use crate::config::{self, Loaded};
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
        "config": redacted_config(&loaded.config)?,
        "provenance": provenance,
        "problems": config::validate::validate(&loaded.config),
        "credentials": credentials(loaded),
    });

    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

/// Serialize the config with credential-bearing fields redacted.
///
/// `models.api_key_env` and `sentry.token_env` are supposed to hold the *name*
/// of an environment variable. When someone pastes the key itself — the exact
/// mistake validation warns about — serializing the config verbatim prints that
/// key into a terminal or a CI log. `doctor --json` is often the first thing
/// run when something is wrong, so it is precisely the wrong moment to echo a
/// credential.
fn redacted_config(config: &Config) -> Result<serde_json::Value> {
    let mut value = serde_json::to_value(config)?;

    for (section, field) in [("models", "api_key_env"), ("sentry", "token_env")] {
        if let Some(current) = value
            .get_mut(section)
            .and_then(|s| s.get_mut(field))
            .and_then(|f| f.as_str().map(str::to_string))
            && looks_like_a_value(&current)
        {
            value[section][field] = json!(crate::scan::types::redact(&current));
        }
    }

    Ok(value)
}

/// Whether a field that should name an environment variable holds something
/// else. Environment variable names are conventionally SCREAMING_SNAKE_CASE, so
/// a lowercase letter is the cheap tell that a value was pasted instead.
fn looks_like_a_value(text: &str) -> bool {
    text.contains(|c: char| c.is_ascii_lowercase())
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
    println!(
        "  strictness       {}  ({})",
        config.review.strictness,
        crate::config::types::Strictness::for_level(config.review.strictness).label
    );
    println!(
        "  severity gate    {}{}",
        config.severity_gate(),
        if config.review.severity_gate.is_some() {
            "  (set explicitly)"
        } else {
            ""
        }
    );
    println!(
        "  confidence min   {}{}",
        config.confidence_min(),
        if config.review.confidence_min.is_some() {
            "  (set explicitly)"
        } else {
            ""
        }
    );
    println!("  max comments     {}", config.review.max_comments);
    println!("  incremental      {}", config.review.incremental);

    println!("\nmodels");
    println!("  gateway          {}", config.models.gateway);
    println!("  base url         {}", config.models.base_url);
    println!("  scan tier        {}", config.models.scan);
    println!("  deep tier        {}", config.models.deep);
    println!("  flash tier       {}", config.models.flash);
    if !config.models.provider.is_empty() {
        println!(
            "  provider         {}{}",
            config.models.provider.order.join(", "),
            if config.models.provider.allow_fallbacks {
                "  (the gateway may route elsewhere)"
            } else {
                "  (pinned)"
            }
        );
    }
    println!("  budget per PR    ${:.2}", config.models.budget_usd_per_pr);

    // The model a lane actually calls, which is the panel's tier — not
    // `model_for`, which names the tier the lane *would* use for a single
    // call. Reporting the latter is how this line came to describe a model no
    // review had run on since the lanes became panels.
    for lane in config.enabled_lanes() {
        let reviewers = crate::council::reviewers(config, lane);
        let models: Vec<&str> = reviewers.iter().map(|r| r.model).collect();

        println!(
            "  {:<16} {}  ({} reviewer{}, fails at {})",
            lane.as_str(),
            models.join(", "),
            reviewers.len(),
            if reviewers.len() == 1 { "" } else { "s" },
            config.fail_on(lane)
        );
    }

    // A model with no price is billed at the ceiling, so it will not escape the
    // budget — but it will over-report, and the fix is a one-line table entry.
    // Saying so here is what makes a stale price table visible before a review
    // runs rather than after it has been billed.
    let configured: Vec<&str> = config
        .enabled_lanes()
        .into_iter()
        .map(|lane| config.model_for(lane))
        .chain([
            config.models.scan.as_str(),
            config.models.deep.as_str(),
            // The tier every panel actually runs on. Leaving it out meant the
            // one model every review calls was the one model whose price was
            // never checked.
            config.models.flash.as_str(),
        ])
        .chain(config.models.fallback.iter().map(String::as_str))
        .collect();
    let unpriced = crate::harness::pricing::unpriced(configured);
    if !unpriced.is_empty() {
        println!(
            "  unpriced         {} (billed at the most expensive known rate)",
            unpriced.join(", ")
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

    // Routing is reported here rather than as a validation problem, because an
    // unrouted project is a loud runtime skip and not a refusal to start. But
    // it is exactly the "looks applied and is not" defect `doctor` exists to
    // surface: a project that sweeps into nowhere is invisible otherwise.
    if config.sentry.enabled {
        println!("\nsentry routing");
        if config.sentry.projects.is_empty() {
            println!("  <no projects configured>");
        }
        for project in &config.sentry.projects {
            match config.sentry.route_for(project) {
                Some(route) => println!("  {:<24} -> {}", project, route.repo),
                None => println!(
                    "  {:<24} NO ROUTE — this project is skipped; add a [[sentry.route]] for it",
                    project
                ),
            }
        }
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
    use crate::config::Layer;

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
    fn a_key_pasted_into_the_env_var_field_is_not_echoed_by_doctor_json() {
        // `doctor --json` is often the first thing run when something is wrong,
        // which makes it exactly the wrong moment to print a credential.
        let key = format!("{}{}", "sk-or-", "v1-0f1e2d3c4b5a69788796a5b4c3d2e1f0");
        let dir = repo(&format!("version = 1\n[models]\napi_key_env = \"{key}\"\n"));
        let loaded = config::load(dir.path(), None).expect("loads");

        let rendered = serde_json::to_string(&redacted_config(&loaded.config).expect("redacts"))
            .expect("serialises");
        assert!(!rendered.contains("0f1e2d3c"), "{rendered}");
        assert!(rendered.contains("redacted"), "{rendered}");
    }

    #[test]
    fn a_genuine_variable_name_is_left_readable() {
        let dir = repo("version = 1\n");
        let loaded = config::load(dir.path(), None).expect("loads");
        let rendered = serde_json::to_string(&redacted_config(&loaded.config).expect("redacts"))
            .expect("serialises");
        assert!(rendered.contains("OPENROUTER_API_KEY"), "{rendered}");
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
