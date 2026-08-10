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
    validate_embeddings(config, &mut problems);
    validate_retrieval(config, &mut problems);
    validate_overview(config, &mut problems);
    validate_lanes(config, &mut problems);
    validate_council(config, &mut problems);
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
        // `gate` gets its own message below. Falling through to "unknown lane"
        // as well would tell someone whose config predates the change that they
        // made a typo, and then separately that they did not.
        if LaneId::parse(name).is_none() && name.trim() != "gate" {
            problems.push(format!(
                "`review.lanes` contains unknown lane `{name}`; expected one of {}",
                known(&LaneId::ALL.map(|l| l.as_str()))
            ));
        }
    }
    // `gate` was a lane once. It published `tinysweeper/gate`, an aggregate of
    // every other lane, and it is gone: the same verdict now arrives as the
    // bot's own review, which is what branch protection should require.
    // Named explicitly rather than falling through to "unknown lane", because
    // a configuration carried over from before the change deserves to be told
    // what replaced it rather than that it made a typo.
    if review.lanes.iter().any(|l| l.trim() == "gate") {
        problems.push(
            "`review.lanes` lists `gate`; the aggregate check run no longer exists — \
             the bot's approving review carries that verdict now, so remove it"
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

    // Reasoning is drawn from the same allowance as the answer, and the effort
    // key does not bound it — measured, both configured models spend the
    // *entire* budget thinking at `high` and at `low` alike, then return empty
    // content with `finish_reason = "length"`. The runs that back this are the
    // table in `config/defaults.toml` next to `reasoning_effort`: both models
    // at 8000 tokens, showing a full reasoning burn and no content at either
    // effort, `off` clearing the same budget.
    //
    // A floor rather than a formula because the failure is bimodal: there is no
    // setting at which the model thinks proportionally less, so there is no
    // ratio to compute. 12000 sits below the 16000 that cleared this in
    // production and above the 8000 that reproduced it every time.
    //
    // Caught here because the alternative is catching it in production, which
    // is what happened: every review failed over to the last model in the
    // fallback chain, and the only symptom was a warning line nobody was
    // reading. A configuration that cannot work should not start.
    const REASONING_FLOOR: u32 = 12_000;
    if models.reasoning_effort.trim() != "off"
        && !models.reasoning_effort.trim().is_empty()
        && models.max_tokens < REASONING_FLOOR
    {
        problems.push(format!(
            "`models.max_tokens = {}` is too small with `models.reasoning_effort = \"{}\"`: \
             reasoning is billed against the same allowance and measurably consumes all of it, \
             leaving nothing to answer with. Raise it to at least {REASONING_FLOOR}, or set \
             `models.reasoning_effort = \"off\"` — lowering the effort does not bound it",
            models.max_tokens,
            models.reasoning_effort.trim(),
        ));
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

/// Check `[embeddings]`.
///
/// Every field is checked *only* when the section is enabled. A disabled
/// section is allowed to be blank, because that is what the default is, and
/// rejecting a blank disabled block would make every deployment that does not
/// want retrieval fill in a model id it will never call.
fn validate_embeddings(config: &Config, problems: &mut Vec<String>) {
    let embeddings = &config.embeddings;
    if !embeddings.enabled {
        return;
    }

    // The three halves of the signature. A blank one is not a default anyone
    // could want: it would silently partition the index under `":"`, which is a
    // key that reads as configured and matches nothing a provider ever wrote.
    for (key, value) in [
        ("provider", &embeddings.provider),
        ("model", &embeddings.model),
        ("api_key_env", &embeddings.api_key_env),
    ] {
        if value.trim().is_empty() {
            problems.push(format!(
                "`embeddings.{key}` is empty but `embeddings.enabled = true`"
            ));
        }
    }

    if embeddings
        .api_key_env
        .contains(|c: char| c.is_ascii_lowercase())
    {
        // Same heuristic, same reason, same redaction as `models.api_key_env`:
        // this message reaches a check-run summary.
        problems.push(format!(
            "`embeddings.api_key_env` ({}) looks like a value, not an environment variable name; \
             never put a key in the config file",
            crate::scan::types::redact(&embeddings.api_key_env)
        ));
    }

    if embeddings.dimensions == 0 {
        problems.push(
            "`embeddings.dimensions = 0` describes no vector; it is the width the search index is \
             created with and must match what the provider returns"
                .into(),
        );
    }

    if !embeddings.base_url.trim().is_empty()
        && !embeddings.base_url.starts_with("http://")
        && !embeddings.base_url.starts_with("https://")
    {
        problems.push(format!(
            "`embeddings.base_url = \"{}\"` is not an http(s) URL; leave it empty for the \
             provider's own default",
            embeddings.base_url
        ));
    }

    if embeddings.batch == 0 {
        problems.push("`embeddings.batch = 0` would send no texts per call".into());
    }

    // Same `!is_finite()` guard as the review budget, and for the same reason:
    // nan sails through `<= 0.0` and would leave indexing unbounded.
    if !embeddings.budget_usd_per_index.is_finite() || embeddings.budget_usd_per_index <= 0.0 {
        problems.push(format!(
            "`embeddings.budget_usd_per_index = {}` must be a finite number above zero; it is the \
             hard ceiling for indexing one repository",
            embeddings.budget_usd_per_index
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

/// Catch a retrieval config that would run and retrieve nothing.
///
/// Every check here is for a value that leaves retrieval *enabled* while making
/// it incapable of producing context. That combination is worse than turning it
/// off: the review still pays to embed a query, and nobody reading the check
/// run can tell why the reviewer never saw any related code.
fn validate_retrieval(config: &Config, problems: &mut Vec<String>) {
    let retrieval = &config.retrieval;
    if !retrieval.enabled {
        return;
    }

    for (name, value) in [
        ("query_chars", retrieval.query_chars),
        ("context_tokens", retrieval.context_tokens),
        ("max_chunks", retrieval.max_chunks),
    ] {
        if value == 0 {
            problems.push(format!(
                "`retrieval.{name} = 0` with `retrieval.enabled = true` retrieves nothing while \
                 still paying to embed a query; set it above zero or set \
                 `retrieval.enabled = false`"
            ));
        }
    }

    // Not an error — a hop-less retrieval is still a working similarity search
    // — but it silently gives up the one thing similarity cannot do.
    if retrieval.graph_hops > 0 && retrieval.max_graph_nodes == 0 {
        problems.push(
            "`retrieval.max_graph_nodes = 0` disables graph expansion while `retrieval.graph_hops` \
             is set; the walk would be discarded, so set the cap or set `graph_hops = 0`"
                .into(),
        );
    }

    // Three hops out of a widely imported module is most of the repository.
    // The node cap would truncate it anyway, so this only ever buys a slower
    // query for the same answer.
    if retrieval.graph_hops > 2 {
        problems.push(format!(
            "`retrieval.graph_hops = {}` walks most of a repository; the node cap truncates it \
             back, so this costs query time and returns no more context. Use 1 or 2",
            retrieval.graph_hops
        ));
    }
}

fn validate_overview(config: &Config, problems: &mut Vec<String>) {
    let overview = &config.overview;
    if !overview.enabled {
        return;
    }

    // A zero here does not disable the feature, it produces a comment with an
    // empty diagram in it — which reads as "this change touches nothing".
    // Turning the map off is one key, and it is not this one.
    for (name, value) in [
        ("max_components", overview.max_components),
        ("max_paths_per_component", overview.max_paths_per_component),
    ] {
        if value == 0 {
            problems.push(format!(
                "`overview.{name} = 0` with `overview.enabled = true` would post an empty \
                 diagram; set it above zero or set `overview.enabled = false`"
            ));
        }
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
            "`automerge.enabled = true` with an empty `automerge.require_checks` would merge on no evidence at all; name the checks that must pass, e.g. `tinysweeper/security`"
                .into(),
        );
    }

    // The policy fails closed on a glob it cannot compile, which is safe but
    // silent: the operator sees a pull request that never merges and no reason
    // why. Saying so here turns a bug report into a typo.
    for (field, patterns) in [
        ("sensitive_paths", &automerge.sensitive_paths),
        ("dependency_paths", &automerge.dependency_paths),
    ] {
        for pattern in patterns {
            if let Err(err) = Glob::new(pattern) {
                problems.push(format!(
                    "`automerge.{field}` contains `{pattern}`, which is not a valid glob: {err}"
                ));
            }
        }
    }

    // A zero cap refuses everything. That is the correct reading of a
    // misconfigured threshold — never "unlimited" — but it is almost certainly
    // not what was meant, so it is said out loud.
    if automerge.max_files == 0 {
        problems.push(
            "`automerge.max_files = 0` refuses every pull request; a cap of zero is not `unlimited`"
                .into(),
        );
    }
    for (field, value) in [
        ("max_changed_lines", automerge.max_changed_lines),
        ("max_hunks", automerge.max_hunks as u64),
        ("max_directories", automerge.max_directories as u64),
    ] {
        if value == 0 {
            problems.push(format!(
                "`automerge.{field} = 0` refuses every pull request; a cap of zero is not `unlimited`"
            ));
        }
    }

    if automerge.allow_dependency_bumps && automerge.dependency_bots.is_empty() {
        problems.push(
            "`automerge.allow_dependency_bumps = true` with no `automerge.dependency_bots` exempts nothing; name the bot logins or turn the exemption off"
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

/// The council: who reviews, with what character.
fn validate_council(config: &Config, problems: &mut Vec<String>) {
    let council = &config.council;

    if council.enabled && council.agents.is_empty() {
        problems.push(
            "`council.enabled = true` with no `[[council.agents]]` reviews nothing differently; \
             either add an agent or leave the council off"
                .into(),
        );
    }

    let mut seen = std::collections::BTreeSet::new();
    for agent in &council.agents {
        if agent.id.trim().is_empty() {
            problems.push("a `[[council.agents]]` entry has no `id`".into());
        } else if !seen.insert(agent.id.as_str()) {
            // The id names the agent in the cost line and the check summary, so
            // two agents sharing one makes the report unreadable.
            problems.push(format!(
                "two `[[council.agents]]` entries share the id `{}`",
                agent.id
            ));
        }

        if let Some(persona) = agent.persona.as_deref()
            && crate::council::persona::lookup(persona).is_none()
        {
            // A persona is a name, never text: repository prose reaches a
            // prompt through exactly one door and this is not it. So a typo has
            // to be an error rather than a reviewer with no character.
            problems.push(format!(
                "`{}` names the persona `{persona}`, which does not exist. Known: {}",
                agent.id,
                known(&crate::council::persona::NAMES)
            ));
        }

        for lane in &agent.lanes {
            if !config.review.lanes.iter().any(|name| name == lane.as_str()) {
                problems.push(format!(
                    "`{}` reviews the `{lane}` lane, which `review.lanes` does not enable",
                    agent.id
                ));
            }
        }
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
