//! Tests for configuration loading, merging and validation.
//!
//! Split out of `mod.rs` because they cover three modules and would otherwise
//! bury the loader they are testing.

use std::path::Path;

use tempfile::TempDir;

use crate::config::types::{Config, LaneId, Severity};
use crate::config::{DEFAULTS, Layer, load, load_validated, validate};

/// Build a repository skeleton with an optional config file and preset.
fn repo(config: Option<&str>, presets: &[(&str, &str)]) -> TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    if let Some(text) = config {
        std::fs::write(dir.path().join(".tinysweeper.toml"), text).expect("write config");
    }
    for (name, text) in presets {
        let preset_dir = dir.path().join("presets").join(name);
        std::fs::create_dir_all(&preset_dir).expect("create preset dir");
        std::fs::write(preset_dir.join("preset.toml"), text).expect("write preset");
    }
    dir
}

/// Write a rule document into a repository skeleton's `presets/rules/`.
fn with_rules(dir: &TempDir, name: &str, text: &str) {
    let rules_dir = dir.path().join("presets").join("rules");
    std::fs::create_dir_all(&rules_dir).expect("create rules dir");
    std::fs::write(rules_dir.join(format!("{name}.md")), text).expect("write rule document");
}

/// Copy this repository's own rule documents into a skeleton, so a shipped
/// preset that references one resolves.
fn with_shipped_rules(dir: &TempDir) {
    let source = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("presets")
        .join("rules");
    for entry in std::fs::read_dir(&source).expect("read shipped rules") {
        let entry = entry.expect("dir entry");
        if entry.path().extension().is_some_and(|e| e == "md") {
            let name = entry
                .path()
                .file_stem()
                .unwrap()
                .to_string_lossy()
                .into_owned();
            with_rules(
                dir,
                &name,
                &std::fs::read_to_string(entry.path()).expect("read rule document"),
            );
        }
    }
}

fn parse(text: &str) -> Config {
    let dir = repo(Some(text), &[]);
    load(dir.path(), None).expect("loads").config
}

#[test]
fn the_built_in_defaults_are_valid() {
    let config: Config = DEFAULTS.parse::<toml::Table>().unwrap().try_into().unwrap();
    let problems = validate::validate(&config);
    assert!(problems.is_empty(), "{problems:#?}");
}

#[test]
fn a_repository_with_no_config_runs_on_defaults() {
    let dir = repo(None, &[]);
    let loaded = load(dir.path(), None).expect("loads");

    assert!(loaded.source.is_none());
    assert_eq!(loaded.config.review.strictness, 2);
    // Quiet by default: high severity and real confidence. A reviewer that
    // raises four things on every pull request gets ignored within a week.
    assert_eq!(loaded.config.severity_gate(), Severity::High);
    assert_eq!(loaded.config.confidence_min(), 0.75);
    assert!(!loaded.config.automerge.enabled);
}

#[test]
fn strictness_actually_moves_the_gates() {
    // It was documented as "the single noise dial", validated for range, and
    // read by nothing at all.
    let quiet = parse("version = 1\n[review]\nstrictness = 1\n");
    let loud = parse("version = 1\n[review]\nstrictness = 3\n");

    assert_eq!(quiet.severity_gate(), Severity::Critical);
    assert_eq!(loud.severity_gate(), Severity::Medium);
    assert!(quiet.confidence_min() > loud.confidence_min());
}

#[test]
fn an_explicit_gate_overrides_the_dial() {
    let config = parse(
        "version = 1\n[review]\nstrictness = 1\nseverity_gate = \"low\"\nconfidence_min = 0.1\n",
    );
    assert_eq!(config.severity_gate(), Severity::Low);
    assert_eq!(config.confidence_min(), 0.1);
}

#[test]
fn every_scaffolded_capability_is_off_by_default() {
    let dir = repo(None, &[]);
    let config = load(dir.path(), None).expect("loads").config;

    assert!(!config.issues.enabled);
    assert!(!config.issues.close.enabled);
    assert!(!config.automation.enabled);
    assert!(!config.automation.stale.enabled);
    assert!(!config.sentry.enabled);
    assert!(!config.automerge.enabled);
}

#[test]
fn stale_handling_defaults_to_marking_never_closing() {
    let dir = repo(None, &[]);
    let config = load(dir.path(), None).expect("loads").config;
    assert_eq!(config.automation.stale.days_until_close, None);
}

#[test]
fn a_repository_setting_overrides_a_default_and_is_attributed() {
    let dir = repo(Some("version = 1\n[review]\nstrictness = 3\n"), &[]);
    let loaded = load(dir.path(), None).expect("loads");

    assert_eq!(loaded.config.review.strictness, 3);
    assert_eq!(
        loaded.provenance.get("review.strictness"),
        Some(Layer::Repo)
    );
    // Untouched neighbours keep their default attribution.
    assert_eq!(
        loaded.provenance.get("review.max_comments"),
        Some(Layer::Defaults)
    );
}

#[test]
fn a_preset_sits_between_defaults_and_the_repository() {
    let dir = repo(
        Some("version = 1\npreset = \"strict\"\n[review]\nmax_comments = 5\n"),
        &[(
            "strict",
            "version = 1\n[review]\nstrictness = 3\nmax_comments = 99\n",
        )],
    );
    let loaded = load(dir.path(), None).expect("loads");

    assert_eq!(loaded.config.review.strictness, 3, "preset beats defaults");
    assert_eq!(loaded.config.review.max_comments, 5, "repo beats preset");
    assert_eq!(
        loaded.provenance.get("review.strictness"),
        Some(Layer::Preset)
    );
    assert_eq!(
        loaded.provenance.get("review.max_comments"),
        Some(Layer::Repo)
    );
    assert!(loaded.preset_source.is_some());
}

#[test]
fn a_missing_preset_names_every_path_it_looked_in() {
    let dir = repo(Some("version = 1\npreset = \"nope\"\n"), &[]);
    let err = load(dir.path(), None).unwrap_err().to_string();

    assert!(err.contains("preset `nope` not found"), "{err}");
    assert!(err.contains("presets/nope/preset.toml"), "{err}");
}

#[test]
fn a_preset_name_cannot_escape_the_presets_directory() {
    let dir = repo(Some("version = 1\npreset = \"../../etc\"\n"), &[]);
    let err = load(dir.path(), None).unwrap_err().to_string();
    assert!(err.contains("must not contain a path separator"), "{err}");
}

#[test]
fn config_is_discovered_under_dot_github_too() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(dir.path().join(".github")).expect("mkdir");
    std::fs::write(
        dir.path().join(".github/tinysweeper.toml"),
        "version = 1\n[review]\nstrictness = 1\n",
    )
    .expect("write");

    let loaded = load(dir.path(), None).expect("loads");
    assert_eq!(loaded.config.review.strictness, 1);
    assert!(
        loaded
            .source
            .as_ref()
            .expect("source")
            .ends_with(".github/tinysweeper.toml")
    );
}

#[test]
fn the_root_config_wins_over_the_dot_github_one() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(dir.path().join(".github")).expect("mkdir");
    std::fs::write(
        dir.path().join(".tinysweeper.toml"),
        "version = 1\n[review]\nstrictness = 1\n",
    )
    .expect("write");
    std::fs::write(
        dir.path().join(".github/tinysweeper.toml"),
        "version = 1\n[review]\nstrictness = 3\n",
    )
    .expect("write");

    let loaded = load(dir.path(), None).expect("loads");
    assert_eq!(loaded.config.review.strictness, 1);
}

#[test]
fn an_unknown_key_is_rejected_rather_than_silently_ignored() {
    let dir = repo(Some("version = 1\n[review]\nstrictnes = 3\n"), &[]);
    let err = load(dir.path(), None).unwrap_err().to_string();
    assert!(err.contains("strictnes"), "{err}");
}

#[test]
fn validation_reports_every_problem_at_once() {
    let config = parse(
        r#"
version = 1

[review]
lanes = ["critique", "nonsense"]
strictness = 9
severity_gate = "urgent"
confidence_min = 4.0
max_comments = 0

[models]
base_url = "not-a-url"
scan = ""
budget_usd_per_pr = 0.0
"#,
    );

    let problems = validate::validate(&config);
    let joined = problems.join("\n");

    for expected in [
        "unknown lane `nonsense`",
        "review.strictness",
        "review.severity_gate",
        "review.confidence_min",
        "review.max_comments",
        "models.base_url",
        "models.scan",
        "models.budget_usd_per_pr",
    ] {
        assert!(
            joined.contains(expected),
            "missing `{expected}` in:\n{joined}"
        );
    }
    assert!(
        problems.len() >= 8,
        "expected every problem, got {problems:#?}"
    );
}

#[test]
fn an_api_key_pasted_where_the_variable_name_goes_is_caught() {
    let config = parse("version = 1\n[models]\napi_key_env = \"sk-or-v1-abc123\"\n");
    let problems = validate::validate(&config);
    assert!(
        problems
            .iter()
            .any(|p| p.contains("looks like a value, not an environment variable name")),
        "{problems:#?}"
    );
}

#[test]
fn listing_the_gate_as_a_lane_says_what_replaced_it() {
    // A config written before the aggregate check run was removed. Telling
    // someone in that position that `gate` is an unknown lane would be true and
    // useless; they need to know where the verdict went.
    let config = parse("version = 1\n[review]\nlanes = [\"critique\", \"gate\"]\n");
    let problems = validate::validate(&config);

    assert!(
        problems
            .iter()
            .any(|p| p.contains("the bot's approving review carries that verdict now")),
        "{problems:#?}"
    );
    assert!(
        !problems.iter().any(|p| p.contains("unknown lane `gate`")),
        "one message, not two: {problems:#?}"
    );
}

#[test]
fn a_lane_option_that_only_applies_elsewhere_is_flagged() {
    let config = parse("version = 1\n[lanes.critique]\nmax_blob_bytes = 100\n");
    let problems = validate::validate(&config);
    assert!(
        problems
            .iter()
            .any(|p| p.contains("applies only to the `commits` lane")),
        "{problems:#?}"
    );
}

#[test]
fn automerge_with_no_required_checks_is_rejected() {
    let config = parse("version = 1\n[automerge]\nenabled = true\nrequire_checks = []\n");
    let problems = validate::validate(&config);
    assert!(
        problems.iter().any(|p| p.contains("merge on no evidence")),
        "{problems:#?}"
    );
}

#[test]
fn a_label_in_both_allow_and_block_lists_is_flagged_as_dead() {
    let config = parse(
        "version = 1\n[automerge]\nenabled = true\nallow_labels = [\"ship\"]\nblock_labels = [\"ship\"]\n",
    );
    let problems = validate::validate(&config);
    assert!(
        problems.iter().any(|p| p.contains("blocking wins")),
        "{problems:#?}"
    );
}

#[test]
fn live_issue_closing_with_no_age_floor_is_rejected() {
    let config = parse(
        r#"
version = 1
[issues]
enabled = true
[issues.close]
enabled = true
dry_run = false
min_age_days = 0
"#,
    );
    let problems = validate::validate(&config);
    assert!(
        problems
            .iter()
            .any(|p| p.contains("close issues the moment they are opened")),
        "{problems:#?}"
    );
}

#[test]
fn a_setting_with_no_effect_because_its_feature_is_off_is_flagged() {
    let config = parse("version = 1\n[issues.close]\nenabled = true\n");
    let problems = validate::validate(&config);
    assert!(
        problems
            .iter()
            .any(|p| p.contains("no effect while `issues.enabled = false`")),
        "{problems:#?}"
    );
}

#[test]
fn enabling_sentry_requires_an_org_and_a_project() {
    let config = parse("version = 1\n[sentry]\nenabled = true\n");
    let problems = validate::validate(&config);
    assert!(problems.iter().any(|p| p.contains("requires `sentry.org`")));
    assert!(problems.iter().any(|p| p.contains("sentry.projects")));
}

#[test]
fn invalid_globs_are_reported_with_their_pattern() {
    let config = parse("version = 1\n[paths]\nignore = [\"src/**/[\"]\n");
    let problems = validate::validate(&config);
    assert!(
        problems.iter().any(|p| p.contains("invalid glob")),
        "{problems:#?}"
    );
}

#[test]
fn a_future_schema_version_asks_the_user_to_upgrade() {
    let config = parse("version = 99\n");
    let problems = validate::validate(&config);
    assert!(
        problems.iter().any(|p| p.contains("upgrade tinysweeper")),
        "{problems:#?}"
    );
}

#[test]
fn load_validated_collects_the_problems_into_one_error() {
    let dir = repo(
        Some("version = 1\n[review]\nstrictness = 9\nmax_comments = 0\n"),
        &[],
    );
    let err = load_validated(dir.path(), None).unwrap_err().to_string();

    assert!(err.contains("2 problems"), "{err}");
    assert!(err.contains("review.strictness"), "{err}");
    assert!(err.contains("review.max_comments"), "{err}");
}

#[test]
fn an_explicit_config_path_is_used_verbatim() {
    let dir = repo(Some("version = 1\n[review]\nstrictness = 1\n"), &[]);
    let other = dir.path().join("other.toml");
    std::fs::write(&other, "version = 1\n[review]\nstrictness = 3\n").expect("write");

    let loaded = load(dir.path(), Some(&other)).expect("loads");
    assert_eq!(loaded.config.review.strictness, 3);
}

#[test]
fn an_explicit_path_that_does_not_exist_is_an_error() {
    let dir = repo(None, &[]);
    let err = load(dir.path(), Some(&dir.path().join("nope.toml")))
        .unwrap_err()
        .to_string();
    assert!(err.contains("no such file or directory"), "{err}");
}

#[test]
fn the_shipped_presets_load_and_validate() {
    // The presets in this repository are user-facing documentation as much as
    // configuration; a broken one is a broken example.
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    for name in ["rust-library", "security-strict"] {
        let dir = repo(
            Some(&format!("version = 1\npreset = \"{name}\"\n")),
            &[(
                name,
                &std::fs::read_to_string(root.join("presets").join(name).join("preset.toml"))
                    .expect("read shipped preset"),
            )],
        );
        with_shipped_rules(&dir);
        let loaded = load(dir.path(), None).unwrap_or_else(|e| panic!("{name}: {e}"));
        let problems = validate::validate(&loaded.config);
        assert!(problems.is_empty(), "{name}: {problems:#?}");

        // A rule document that failed to resolve would leave an entry with the
        // reference and no content, which reads as a rule with no rules.
        for rule in &loaded.config.path_instructions {
            assert!(
                !rule.instructions.trim().is_empty(),
                "{name}: `{}` has no instructions",
                rule.glob
            );
        }
    }
}

#[test]
fn the_shipped_security_taxonomy_is_scoped_to_the_security_lane() {
    // Unscoped, the document would be added to the cacheable prefix of every
    // lane on every file — a large, permanent bill for rules only one reviewer
    // can act on.
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let dir = repo(
        Some("version = 1\npreset = \"security-strict\"\n"),
        &[(
            "security-strict",
            &std::fs::read_to_string(root.join("presets/security-strict/preset.toml"))
                .expect("read shipped preset"),
        )],
    );
    with_shipped_rules(&dir);
    let config = load(dir.path(), None).expect("loads").config;

    let entry = config
        .path_instructions
        .iter()
        .find(|rule| rule.rules.as_deref() == Some("security"))
        .expect("the preset wires the security taxonomy");
    assert_eq!(entry.lanes, vec![LaneId::Security]);
    assert!(entry.instructions.contains("Do NOT report"));
}

#[test]
fn a_rule_document_is_inlined_into_its_path_instruction() {
    // Rule documents are data under presets/. Adding one is a file and a line
    // of TOML, never a module.
    let dir = repo(
        Some("version = 1\n[[path_instructions]]\nglob = \"**/*.rs\"\nrules = \"rust\"\n"),
        &[],
    );
    with_rules(&dir, "rust", "## Rust\n\nDo NOT report naming.\n");

    let config = load(dir.path(), None).expect("loads").config;
    assert_eq!(config.path_instructions.len(), 1);
    assert!(
        config.path_instructions[0]
            .instructions
            .contains("Do NOT report naming."),
        "{:?}",
        config.path_instructions[0]
    );
}

#[test]
fn inline_instructions_and_a_rule_document_are_both_kept() {
    let dir = repo(
        Some(
            "version = 1\n[[path_instructions]]\nglob = \"**/*.rs\"\n\
             instructions = \"One trait per file.\"\nrules = \"rust\"\n",
        ),
        &[],
    );
    with_rules(&dir, "rust", "Do NOT report naming.\n");

    let config = load(dir.path(), None).expect("loads").config;
    let text = &config.path_instructions[0].instructions;
    assert!(text.contains("One trait per file."), "{text}");
    assert!(text.contains("Do NOT report naming."), "{text}");
}

#[test]
fn a_missing_rule_document_names_every_path_it_looked_in() {
    // Resolved at load time on purpose: a typo has to be an error a human sees
    // once, not a silently weaker review on every run.
    let dir = repo(
        Some("version = 1\n[[path_instructions]]\nglob = \"**/*.rs\"\nrules = \"nope\"\n"),
        &[],
    );
    let err = load(dir.path(), None).unwrap_err().to_string();

    assert!(err.contains("rule document `nope` not found"), "{err}");
    assert!(err.contains("rules/nope.md"), "{err}");
}

#[test]
fn a_rule_document_name_cannot_escape_the_presets_directory() {
    let dir = repo(
        Some(
            "version = 1\n[[path_instructions]]\nglob = \"**/*.rs\"\n\
             rules = \"../../etc/passwd\"\n",
        ),
        &[],
    );
    let err = load(dir.path(), None).unwrap_err().to_string();
    assert!(err.contains("path separator"), "{err}");
}

#[test]
fn every_shipped_rule_document_carries_a_negative_list() {
    // Roughly half of each document is the "do NOT report" half, and that is
    // where the precision comes from. A rule document without one makes the
    // review noisier, not better.
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("presets")
        .join("rules");
    for entry in std::fs::read_dir(&dir).expect("read rules") {
        let path = entry.expect("dir entry").path();
        if path.extension().is_none_or(|e| e != "md") || path.ends_with("README.md") {
            continue;
        }
        let text = std::fs::read_to_string(&path).expect("read rule document");
        assert!(
            text.contains("Do NOT report") || text.contains("do NOT report"),
            "{} has no negative list",
            path.display()
        );
    }
}

#[test]
fn enabled_lanes_reflects_the_merged_config() {
    let config = parse("version = 1\n[review]\nlanes = [\"security\", \"critique\"]\n");
    assert_eq!(
        config.enabled_lanes(),
        vec![LaneId::Critique, LaneId::Security]
    );
}
