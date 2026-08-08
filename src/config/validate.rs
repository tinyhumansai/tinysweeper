//! Configuration validation.
//!
//! The rule here is that a single run reports **every** problem, not the first
//! one. Someone fixing their config should need one round trip, not six — and
//! in CI, where each round trip is a push and a wait, that difference is the
//! whole experience.
//!
//! Messages name the key, say what is wrong, and say what would be right.

use globset::Glob;

use crate::config::types::{Config, LaneId, MergeMethod, ModelRef, Severity};

/// The maximum schema version this build understands.
pub const SCHEMA_VERSION: u32 = 1;

/// Check `config`, returning every problem found.
///
/// An empty vector means the config is usable. It does not mean the config is
/// *sensible* — that is what `doctor` is for.
pub fn validate(config: &Config) -> Vec<String> {
    let mut problems = Vec::new();

    validate_version(config, &mut problems);
    validate_review(config, &mut problems);
    validate_paths(config, &mut problems);
    validate_models(config, &mut problems);
    validate_knowledge(config, &mut problems);
    validate_lanes(config, &mut problems);
    validate_automerge(config, &mut problems);
    validate_issues(config, &mut problems);
    validate_automation(config, &mut problems);
    validate_sentry(config, &mut problems);

    problems
}

fn validate_version(config: &Config, problems: &mut Vec<String>) {
    if config.version == 0 {
        problems.push("`version` is missing; set `version = 1`".into());
    } else if config.version > SCHEMA_VERSION {
        problems.push(format!(
            "`version = {}` is newer than this build understands (max {SCHEMA_VERSION}); upgrade tinysweeper",
            config.version
        ));
    }
}

fn validate_review(config: &Config, problems: &mut Vec<String>) {
    let review = &config.review;

    if review.lanes.is_empty() {
        problems.push("`review.lanes` is empty; nothing would run".into());
    }
    for name in &review.lanes {
        if LaneId::parse(name).is_none() {
            problems.push(format!(
                "`review.lanes` contains unknown lane `{name}`; expected one of {}",
                known(&LaneId::ALL.map(|l| l.as_str()))
            ));
        }
    }
    // The gate is deterministic and always publishes; listing it is a sign
    // someone thinks it is optional, which would be a nasty surprise when they
    // remove it and branch protection keeps waiting for a check that never
    // arrives.
    if review
        .lanes
        .iter()
        .any(|l| LaneId::parse(l) == Some(LaneId::Gate))
    {
        problems.push(
            "`review.lanes` lists `gate`; the gate is deterministic and always runs, so remove it"
                .into(),
        );
    }

    if !(1..=3).contains(&review.strictness) {
        problems.push(format!(
            "`review.strictness = {}` is out of range; expected 1 (chill), 2 (default) or 3 (assertive)",
            review.strictness
        ));
    }

    if let Some(gate) = &review.severity_gate
        && Severity::parse(gate).is_none()
    {
        problems.push(format!(
            "`review.severity_gate = \"{gate}\"` is not a severity; expected one of {}",
            known(&Severity::ALL.map(|s| s.as_str()))
        ));
    }

    if let Some(confidence) = review.confidence_min
        && !(0.0..=1.0).contains(&confidence)
    {
        problems.push(format!(
            "`review.confidence_min = {confidence}` is out of range; expected 0.0 to 1.0"
        ));
    }

    let blocking = review.request_changes_at.trim();
    if !blocking.eq_ignore_ascii_case("off") && Severity::parse(blocking).is_none() {
        problems.push(format!(
            "`review.request_changes_at = \"{blocking}\"` is not a severity; expected `off` or one of {}",
            known(&Severity::ALL.map(|s| s.as_str()))
        ));
    }

    if review.max_comments == 0 {
        problems.push(
            "`review.max_comments = 0` would suppress every comment; set it above zero, or disable the lanes you do not want"
                .into(),
        );
    }
}

fn validate_paths(config: &Config, problems: &mut Vec<String>) {
    for pattern in &config.paths.ignore {
        if let Err(err) = Glob::new(pattern) {
            problems.push(format!(
                "`paths.ignore` contains invalid glob `{pattern}`: {err}"
            ));
        }
    }

    for (index, instruction) in config.path_instructions.iter().enumerate() {
        if instruction.glob.trim().is_empty() {
            problems.push(format!("`path_instructions[{index}].glob` is empty"));
        } else if let Err(err) = Glob::new(&instruction.glob) {
            problems.push(format!(
                "`path_instructions[{index}].glob` is invalid (`{}`): {err}",
                instruction.glob
            ));
        }
        if instruction.instructions.trim().is_empty() {
            problems.push(format!(
                "`path_instructions[{index}].instructions` is empty; a glob with no rule does nothing"
            ));
        }
    }
}

fn validate_models(config: &Config, problems: &mut Vec<String>) {
    let models = &config.models;

    if models.base_url.trim().is_empty() {
        problems.push("`models.base_url` is empty".into());
    } else if !models.base_url.starts_with("http://") && !models.base_url.starts_with("https://") {
        problems.push(format!(
            "`models.base_url = \"{}\"` is not an http(s) URL",
            models.base_url
        ));
    }

    if models.api_key_env.trim().is_empty() {
        problems.push(
            "`models.api_key_env` is empty; it names the environment variable holding the key, never the key itself"
                .into(),
        );
    } else if models
        .api_key_env
        .contains(|c: char| c.is_ascii_lowercase())
    {
        // Cheap heuristic that has caught the real mistake: pasting the key in
        // where the variable name goes.
        // The whole point of this check is that the value may *be* a
        // credential, and this message ends up in a check-run summary that
        // anyone with read access can see. Echoing it would leak the key the
        // check exists to catch.
        problems.push(format!(
            "`models.api_key_env` ({}) looks like a value, not an environment variable name; \
             never put a key in the config file",
            crate::scan::types::redact(&models.api_key_env)
        ));
    }

    if models.scan.trim().is_empty() {
        problems.push("`models.scan` is empty; it names the cheap tier's model id".into());
    }
    if models.deep.trim().is_empty() {
        problems.push("`models.deep` is empty; it names the deep tier's model id".into());
    }

    if models.max_tokens == 0 {
        problems.push("`models.max_tokens = 0` would produce no output".into());
    }

    // `!is_finite()` catches nan and inf, which sail straight through a
    // `<= 0.0` comparison and would disable the spend ceiling entirely.
    if !models.budget_usd_per_pr.is_finite() || models.budget_usd_per_pr <= 0.0 {
        problems.push(format!(
            "`models.budget_usd_per_pr = {}` must be a finite number above zero; it is the hard ceiling for one pull request",
            models.budget_usd_per_pr
        ));
    }
}

fn validate_knowledge(config: &Config, problems: &mut Vec<String>) {
    let knowledge = &config.knowledge;

    // Reported rather than silently dropped. The *runtime* still drops a bad
    // name — a review must not die because someone mistyped a filename — but a
    // config that will never read the file the author meant is exactly what
    // validation exists to say out loud.
    for name in &knowledge.files {
        if !crate::knowledge::types::valid_instruction_file(name.trim()) {
            problems.push(format!(
                "`knowledge.files` contains `{name}`, which is not a plain repository-root \
                 filename; path separators and `..` are refused because this list is editable \
                 by whoever opens a pull request"
            ));
        }
    }

    if knowledge.extract && knowledge.max_file_bytes == 0 {
        problems.push(
            "`knowledge.max_file_bytes = 0` with `knowledge.extract = true` would send an empty \
             file to the extractor; set a byte limit or turn extraction off"
                .into(),
        );
    }
}

fn validate_lanes(config: &Config, problems: &mut Vec<String>) {
    for (name, lane) in &config.lanes {
        let Some(lane_id) = LaneId::parse(name) else {
            problems.push(format!(
                "`lanes.{name}` is not a lane; expected one of {}",
                known(&LaneId::ALL.map(|l| l.as_str()))
            ));
            continue;
        };

        if lane_id == LaneId::Gate {
            problems.push(
                "`lanes.gate` cannot be configured; the gate is deterministic and takes no model"
                    .into(),
            );
        }

        if let Some(model) = &lane.model
            && model.0.trim().is_empty()
        {
            problems.push(format!(
                "`lanes.{name}.model` is empty; expected a tier ({}) or a model id",
                known(&ModelRef::TIERS)
            ));
        }

        if let Some(fail_on) = &lane.fail_on
            && Severity::parse(fail_on).is_none()
        {
            problems.push(format!(
                "`lanes.{name}.fail_on = \"{fail_on}\"` is not a severity; expected one of {}",
                known(&Severity::ALL.map(|s| s.as_str()))
            ));
        }

        // Fields that only mean something on one lane are worth flagging: a
        // silently ignored setting reads as working.
        if lane_id != LaneId::Commits {
            if lane.secret_rulepack.is_some() {
                problems.push(format!(
                    "`lanes.{name}.secret_rulepack` applies only to the `commits` lane"
                ));
            }
            if lane.max_blob_bytes.is_some() {
                problems.push(format!(
                    "`lanes.{name}.max_blob_bytes` applies only to the `commits` lane"
                ));
            }
        }

        if lane.max_blob_bytes == Some(0) {
            problems.push(format!(
                "`lanes.{name}.max_blob_bytes = 0` would flag every committed file"
            ));
        }
    }
}

fn validate_automerge(config: &Config, problems: &mut Vec<String>) {
    let automerge = &config.automerge;

    if MergeMethod::ALL
        .iter()
        .all(|m| m.as_str() != automerge.method)
    {
        problems.push(format!(
            "`automerge.method = \"{}\"` is not a merge method; expected one of {}",
            automerge.method,
            known(&MergeMethod::ALL.map(|m| m.as_str()))
        ));
    }

    if !automerge.enabled {
        return;
    }

    if automerge.require_checks.is_empty() {
        problems.push(
            "`automerge.enabled = true` with an empty `automerge.require_checks` would merge on no evidence at all; require at least `tinysweeper/gate`"
                .into(),
        );
    }

    for label in automerge
        .allow_labels
        .iter()
        .filter(|label| automerge.block_labels.contains(label))
    {
        problems.push(format!(
            "`{label}` appears in both `automerge.allow_labels` and `automerge.block_labels`; blocking wins, so the allow entry is dead"
        ));
    }
}

fn validate_issues(config: &Config, problems: &mut Vec<String>) {
    let issues = &config.issues;

    if !(0.0..=1.0).contains(&issues.dedupe_confidence_min) {
        problems.push(format!(
            "`issues.dedupe_confidence_min = {}` is out of range; expected 0.0 to 1.0",
            issues.dedupe_confidence_min
        ));
    }

    if issues.apply_labels && issues.max_labels == 0 {
        problems.push(
            "`issues.apply_labels = true` with `issues.max_labels = 0` would never apply a label"
                .into(),
        );
    }

    let close = &issues.close;
    if !(0.0..=1.0).contains(&close.confidence_min) {
        problems.push(format!(
            "`issues.close.confidence_min = {}` is out of range; expected 0.0 to 1.0",
            close.confidence_min
        ));
    }

    if close.enabled && !issues.enabled {
        problems.push(
            "`issues.close.enabled = true` has no effect while `issues.enabled = false`".into(),
        );
    }

    // Closing is the one action here that cannot be undone cheaply, so a
    // configuration that lets it fire on fresh issues is called out even though
    // it is technically valid.
    if close.enabled && !close.dry_run && close.min_age_days == 0 {
        problems.push(
            "`issues.close` is live with `min_age_days = 0`; that would close issues the moment they are opened. Set an age floor, or keep `dry_run = true` until you trust it"
                .into(),
        );
    }
}

fn validate_automation(config: &Config, problems: &mut Vec<String>) {
    let automation = &config.automation;
    let stale = &automation.stale;

    if stale.enabled && stale.days_until_stale == 0 {
        problems.push(
            "`automation.stale.days_until_stale = 0` would mark everything stale immediately"
                .into(),
        );
    }

    if stale.enabled && stale.label.trim().is_empty() {
        problems.push("`automation.stale.label` is empty; there is nothing to apply".into());
    }

    if let Some(days) = stale.days_until_close
        && days == 0
        && stale.enabled
    {
        problems.push(
            "`automation.stale.days_until_close = 0` would close an item in the same run that marks it stale; omit the key to only mark, or give the author time to respond"
                .into(),
        );
    }

    if !automation.enabled {
        for (key, on) in [
            ("automation.stale.enabled", stale.enabled),
            ("automation.labeler.enabled", automation.labeler.enabled),
            ("automation.merge_sweep", automation.merge_sweep),
        ] {
            if on {
                problems.push(format!(
                    "`{key} = true` has no effect while `automation.enabled = false`"
                ));
            }
        }
    }

    if automation.merge_sweep && !config.automerge.enabled {
        problems.push(
            "`automation.merge_sweep = true` has no effect while `automerge.enabled = false`"
                .into(),
        );
    }

    for (glob, label) in &automation.labeler.area {
        if let Err(err) = Glob::new(glob) {
            problems.push(format!(
                "`automation.labeler.area` key `{glob}` is not a valid glob: {err}"
            ));
        }
        if label.trim().is_empty() {
            problems.push(format!(
                "`automation.labeler.area.\"{glob}\"` maps to an empty label"
            ));
        }
    }
}

fn validate_sentry(config: &Config, problems: &mut Vec<String>) {
    let sentry = &config.sentry;

    if sentry.base_url.trim().is_empty() {
        problems.push("`sentry.base_url` is empty".into());
    }

    if sentry.token_env.trim().is_empty() {
        problems.push(
            "`sentry.token_env` is empty; it names the environment variable holding the token, never the token itself"
                .into(),
        );
    }

    for pattern in &sentry.ignore_culprits {
        if let Err(err) = Glob::new(pattern) {
            problems.push(format!(
                "`sentry.ignore_culprits` contains invalid glob `{pattern}`: {err}"
            ));
        }
    }

    if !sentry.enabled {
        return;
    }

    if sentry.org.as_deref().unwrap_or("").trim().is_empty() {
        problems.push("`sentry.enabled = true` requires `sentry.org`".into());
    }
    if sentry.projects.is_empty() {
        problems
            .push("`sentry.enabled = true` requires at least one `sentry.projects` entry".into());
    }
    if sentry.max_per_run == 0 {
        problems.push(
            "`sentry.max_per_run = 0` would promote nothing; it exists to stop a Sentry spike flooding the tracker, not to disable promotion"
                .into(),
        );
    }
}

/// Render a list of accepted values for an error message.
fn known(values: &[&str]) -> String {
    values
        .iter()
        .map(|v| format!("`{v}`"))
        .collect::<Vec<_>>()
        .join(", ")
}
