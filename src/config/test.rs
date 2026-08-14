//! Tests for configuration loading, merging and validation.
//!
//! Split out of `mod.rs` because they cover three modules and would otherwise
//! bury the loader they are testing.

use std::path::Path;

use tempfile::TempDir;

use crate::config::types::{Config, LaneId, Severity, StructuredOutput};
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
fn reasoning_with_too_small_a_budget_is_rejected() {
    // The production failure this exists to stop: reasoning and the answer are
    // drawn from one allowance, and at 8000 both configured models — the
    // z-ai/glm-5.2 deep tier and the deepseek-v4-pro fallback — spent the whole
    // of it thinking and returned empty content, `finish_reason = "length"`.
    // Every review then failed over to the last model in the fallback chain,
    // silently. The measured rows live in `config/defaults.toml`.
    //
    // 8000 here is deliberately a failing budget, not the shipped one. The
    // defaults ship `max_tokens = 16000`, above the 12000 floor, and
    // `the_built_in_defaults_are_valid` below asserts DEFAULTS runs clean
    // through `validate::validate` — so the floor and the shipped defaults
    // cannot disagree.
    let config = parse("version = 1\n[models]\nmax_tokens = 8000\nreasoning_effort = \"high\"\n");
    let joined = validate::validate(&config).join("\n");
    assert!(joined.contains("models.max_tokens = 8000"), "{joined}");
    assert!(joined.contains("reasoning_effort"), "{joined}");
}

#[test]
fn lowering_the_effort_does_not_satisfy_the_budget_floor() {
    // Measured at both settings: the table in `config/defaults.toml` lists
    // `low` rows for each configured model and they burn the entire allowance
    // exactly as the `high` rows do. This key picks a style of thinking, never
    // an amount, so treating `low` as a smaller `high` would reintroduce the
    // failure while looking like a fix for it.
    let config = parse("version = 1\n[models]\nmax_tokens = 8000\nreasoning_effort = \"low\"\n");
    assert!(
        validate::validate(&config)
            .join("\n")
            .contains("models.max_tokens = 8000"),
        "`low` was accepted at a budget that cannot work"
    );
}

#[test]
fn reasoning_is_accepted_at_exactly_the_floor() {
    // The boundary the other three tests bracket. A regression from
    // `< REASONING_FLOOR` to `<= REASONING_FLOOR` would reject the floor itself,
    // and no test today would notice — 12000 is the largest budget never
    // checked. It is also the smallest budget the validation accepts, so it is
    // the value a traced regression would land on.
    let config = parse("version = 1\n[models]\nmax_tokens = 12000\nreasoning_effort = \"high\"\n");
    let problems = validate::validate(&config);
    assert!(
        !problems.iter().any(|p| p.contains("reasoning_effort")),
        "12000 is the floor and must be accepted: {problems:#?}"
    );
}

#[test]
fn one_token_below_the_floor_is_rejected() {
    // A hair under the boundary is still under it; this pins the cut to the
    // exact value rather than a range.
    let config = parse("version = 1\n[models]\nmax_tokens = 11999\nreasoning_effort = \"high\"\n");
    let joined = validate::validate(&config).join("\n");
    assert!(
        joined.contains("reasoning_effort"),
        "11999 is below the floor and must be rejected: {joined}"
    );
}

#[test]
fn turning_reasoning_off_makes_a_small_budget_fine() {
    // The escape hatch has to actually work, or the floor is just a wall: with
    // no reasoning the whole allowance goes to the answer, and 8000 was
    // measured as ample — 2572 tokens and 13 findings on a 23k-token diff.
    let config = parse("version = 1\n[models]\nmax_tokens = 8000\nreasoning_effort = \"off\"\n");
    let problems = validate::validate(&config);
    assert!(
        problems.is_empty(),
        "`off` should not be held to the reasoning floor: {problems:#?}"
    );
}

#[test]
fn the_built_in_defaults_are_valid() {
    let config: Config = DEFAULTS.parse::<toml::Table>().unwrap().try_into().unwrap();
    let problems = validate::validate(&config);
    assert!(problems.is_empty(), "{problems:#?}");
}

#[test]
fn the_shipped_defaults_pin_the_upstream_provider() {
    // Unpinned, the gateway load-balances across providers whose prices span
    // 4x while `harness::pricing` keeps one price per model id — so the cost
    // line and `budget_usd_per_pr` stop describing anything real. This asserts
    // the *shipped* config rather than the type's default, because the default
    // is deliberately empty (an operator pointing `base_url` at a gateway with
    // no provider routing must not have a stray `provider` block sent).
    let config: Config = DEFAULTS.parse::<toml::Table>().unwrap().try_into().unwrap();

    assert_eq!(config.models.provider.order, vec!["deepseek".to_string()]);
    assert!(
        !config.models.provider.allow_fallbacks,
        "a pin the gateway may route around is not a pin"
    );
}

#[test]
fn a_sub_table_never_swallows_the_model_scalars() {
    // `[models.provider]` ends the `[models]` table, so a scalar written after
    // it is parsed as `models.provider.<key>` and the whole config fails to
    // load with an error naming an unrelated key. That is exactly how this was
    // first written, and the message it produced pointed nowhere near the
    // mistake.
    let config: Config = DEFAULTS.parse::<toml::Table>().unwrap().try_into().unwrap();

    assert_eq!(config.models.max_tokens, 16_000);
    assert_eq!(config.models.reasoning_effort, "medium");
    assert!((config.models.budget_usd_per_pr - 1.0).abs() < f64::EPSILON);
}

#[test]
fn a_provider_pin_is_only_shipped_alongside_a_mode_that_provider_accepts() {
    // The two keys are one decision, and getting it wrong is silent in the
    // worst way. Pinning `deepseek` while asking for a strict schema yields
    // `404 No endpoints found` on *every* model and *every* call — the whole
    // review goes neutral and nothing in the check output says why. That was
    // shipped once and only a live run against a real pull request caught it.
    let config: Config = DEFAULTS.parse::<toml::Table>().unwrap().try_into().unwrap();

    if config.models.provider.order.iter().any(|p| p == "deepseek") {
        assert_eq!(
            config.models.structured_output,
            StructuredOutput::JsonObject,
            "a DeepSeek pin cannot serve a strict-schema request"
        );
    }
}

#[test]
fn the_defaults_pair_the_selected_model_with_a_mode_it_can_answer() {
    // These two keys are one decision. `deepseek-v4-pro-0813` returns 400
    // "This response_format type is unavailable now" for a strict schema, so
    // selecting it while leaving `structured_output = "schema"` produces a
    // deployment where every single review fails over to the fallback and looks
    // healthy doing it. Measured: 29 of 29 corpus recordings answered by GLM.
    let config: Config = DEFAULTS.parse::<toml::Table>().unwrap().try_into().unwrap();

    if config.models.scan.contains("deepseek-v4-pro-0813")
        || config.models.deep.contains("deepseek-v4-pro-0813")
    {
        assert_eq!(
            config.models.structured_output,
            StructuredOutput::JsonObject,
            "the pinned DeepSeek snapshot cannot answer a strict schema request"
        );
    }
}

#[test]
fn structured_output_defaults_to_the_strong_setting() {
    // Absent from a repository's own config, the mode must be the one that lets
    // the provider refuse a bad shape outright. A weaker default would silently
    // downgrade every deployment that never heard of this key.
    let models: crate::config::types::Models = toml::from_str("").expect("empty table");

    assert_eq!(models.structured_output, StructuredOutput::Schema);
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
fn auto_merge_ships_with_every_deterministic_threshold_set() {
    // Every threshold has to have a shipped value. A missing one deserialises
    // to `0`, and a zero cap refuses everything — safe, but it would make the
    // feature look broken rather than conservative, so the defaults are
    // asserted here rather than left to the derive.
    let dir = repo(None, &[]);
    let automerge = load(dir.path(), None).expect("loads").config.automerge;

    assert_eq!(automerge.max_files, 20);
    assert_eq!(automerge.max_changed_lines, 400);
    assert_eq!(automerge.max_hunks, 30);
    assert_eq!(automerge.max_directories, 5);
    assert!(
        automerge
            .sensitive_paths
            .contains(&".github/**".to_string()),
        "CI workflow paths must be sensitive by default: {:?}",
        automerge.sensitive_paths
    );
    assert!(automerge.allow_dependency_bumps);
    assert_eq!(
        automerge.dependency_bots,
        vec!["dependabot[bot]".to_string(), "renovate[bot]".to_string()]
    );
    assert!(
        automerge
            .dependency_paths
            .iter()
            .any(|glob| glob.contains("Cargo.lock"))
    );
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
fn an_unreadable_auto_merge_glob_is_caught_before_it_can_refuse_everything() {
    // The policy fails closed on a malformed glob, which is safe but silent:
    // the operator sees a pull request that never merges and no reason why.
    // Saying so at load time is the difference between a bug report and a typo.
    let config = parse(
        "version = 1\n[automerge]\nenabled = true\nsensitive_paths = [\"src/**/[\"]\ndependency_paths = [\"**/{\"]\n",
    );
    let problems = validate::validate(&config);

    assert!(
        problems
            .iter()
            .any(|p| p.contains("automerge.sensitive_paths")),
        "{problems:#?}"
    );
    assert!(
        problems
            .iter()
            .any(|p| p.contains("automerge.dependency_paths")),
        "{problems:#?}"
    );
}

#[test]
fn a_zero_auto_merge_cap_is_flagged_as_refusing_everything() {
    let config = parse("version = 1\n[automerge]\nenabled = true\nmax_files = 0\n");
    let problems = validate::validate(&config);
    assert!(
        problems
            .iter()
            .any(|p| p.contains("`automerge.max_files = 0`")),
        "{problems:#?}"
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
fn a_sentry_route_parses_as_a_table_array() {
    let config = parse(
        "version = 1\n[sentry]\nenabled = true\norg = \"acme\"\nprojects = [\"api\"]\n\
         \n[[sentry.route]]\nproject = \"api\"\nrepo = \"acme/backend\"\nlabels = [\"area: sentry\"]\n",
    );

    let route = config.sentry.route_for("api").expect("a route");
    assert_eq!(route.repo, "acme/backend");
    assert!(validate::validate(&config).is_empty());
}

/// Not a validation error: adding a project and routing it may be two
/// separate changes. The skip is loud at runtime and reported by `doctor`.
#[test]
fn a_project_with_no_route_is_not_a_validation_problem() {
    let config = parse(
        "version = 1\n[sentry]\nenabled = true\norg = \"acme\"\nprojects = [\"api\", \"web\"]\n\
         \n[[sentry.route]]\nproject = \"api\"\nrepo = \"acme/backend\"\n",
    );
    assert!(validate::validate(&config).is_empty());
    assert!(config.sentry.route_for("web").is_none());
}

#[test]
fn a_route_repo_that_is_not_owner_slash_name_is_rejected() {
    let config = parse(
        "version = 1\n[sentry]\nenabled = true\norg = \"acme\"\nprojects = [\"api\"]\n\
         \n[[sentry.route]]\nproject = \"api\"\nrepo = \"backend\"\n",
    );
    let problems = validate::validate(&config);
    assert!(
        problems.iter().any(|p| p.contains("not `owner/name`")),
        "{problems:#?}"
    );
}

#[test]
fn two_routes_for_one_project_are_rejected() {
    let config = parse(
        "version = 1\n[sentry]\nenabled = true\norg = \"acme\"\nprojects = [\"api\"]\n\
         \n[[sentry.route]]\nproject = \"api\"\nrepo = \"acme/one\"\n\
         \n[[sentry.route]]\nproject = \"api\"\nrepo = \"acme/two\"\n",
    );
    let problems = validate::validate(&config);
    assert!(
        problems.iter().any(|p| p.contains("more than once")),
        "{problems:#?}"
    );
}

/// A route for a project nobody sweeps writes to nothing — almost always a
/// typo in one of the two lists.
#[test]
fn a_route_for_an_unswept_project_is_reported() {
    let config = parse(
        "version = 1\n[sentry]\nenabled = true\norg = \"acme\"\nprojects = [\"api\"]\n\
         \n[[sentry.route]]\nproject = \"web\"\nrepo = \"acme/landing\"\n",
    );
    let problems = validate::validate(&config);
    assert!(
        problems
            .iter()
            .any(|p| p.contains("not in `sentry.projects`")),
        "{problems:#?}"
    );
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

#[test]
fn every_tier_name_resolves_to_a_model_id_rather_than_to_itself() {
    // The bug this pins: a name in `ModelRef::TIERS` that no resolver arm
    // handles falls through to the "explicit model id" branch and is sent to
    // the gateway *as the model id*. The provider 404s every call — or, with
    // fallbacks enabled, quietly answers from something nobody chose. `flash`
    // shipped that way, and only a live run would have shown it.
    let config = defaults();

    for tier in crate::config::types::ModelRef::TIERS {
        let reference = crate::config::types::ModelRef(tier.to_string());

        let agent = crate::config::types::CouncilAgent {
            id: "a".into(),
            lanes: vec![],
            model: Some(reference),
            persona: None,
        };
        let resolved = config.model_for_agent(&agent, LaneId::Critique);

        assert_ne!(
            resolved, tier,
            "`{tier}` reaches the gateway as a model id called `{tier}`"
        );
        assert!(
            !resolved.is_empty(),
            "`{tier}` resolves to an empty model id"
        );
    }
}

#[test]
fn a_lane_and_the_issue_workload_resolve_tiers_the_same_way() {
    // Three copies of one `match` drifted once already. This asserts they agree
    // rather than asserting each one's arms separately, which is the assertion
    // that would have caught it.
    let mut config = defaults();

    for tier in crate::config::types::ModelRef::TIERS {
        config.issues.model = Some(crate::config::types::ModelRef(tier.to_string()));
        let agent = crate::config::types::CouncilAgent {
            id: "a".into(),
            lanes: vec![],
            model: Some(crate::config::types::ModelRef(tier.to_string())),
            persona: None,
        };

        assert_eq!(
            config.model_for_issues(),
            config.model_for_agent(&agent, LaneId::Critique),
            "`{tier}` resolves differently for issues than for a council agent"
        );
    }
}
