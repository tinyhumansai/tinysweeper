//! The sweep: the four steps, in the one order that is correct.
//!
//! ```text
//! route ─▶ fetch ─▶ filter ─▶ dedupe ─▶ cap ─▶ redact ─▶ promote ─▶ link
//! ```
//!
//! ## Why dedupe sits between filter and cap
//!
//! This is the ordering decision worth reading twice. If the cap ran before
//! the GitHub search, ten already-tracked issues would consume the entire
//! `max_per_run` budget and the sweep would promote nothing — while reporting
//! a truncation, which reads as "there was more to do" when in fact everything
//! was already done. Running dedupe first means the cap counts issues that
//! would actually be *created*, which is the thing the setting exists to
//! bound. `the_cap_counts_promotable_issues_not_tracked_ones` pins it.
//!
//! ## Degradation
//!
//! A project that fails is logged and the sweep moves to the next one. One
//! unreachable project must not take the other five down with it, and a
//! partial sweep is reported as partial — [`SweepReport::failed`] carries the
//! projects that did not complete, so a caller can tell "nothing qualified"
//! apart from "we never looked".
//!
//! ## Nothing unscrubbed is logged
//!
//! Every log line here names identifiers — project slug, Sentry short id,
//! GitHub issue number, a count — or text that has already been through
//! [`crate::sentry::redact`]. A `RawIssue` is never formatted into a log,
//! which is the "tracing must never receive an unscrubbed event" rule from the
//! spec. `no_log_line_carries_unscrubbed_text` covers the paths a test can
//! reach.

use crate::config::types::Config;
use crate::error::Result;
use crate::forge::types::RepoId;
use crate::ports::forge::{ForgeRead, ForgeWrite};
use crate::ports::sentry::SentryApi;
use crate::sentry::dedupe::{self, Tracked};
use crate::sentry::types::Skipped;
use crate::sentry::{link, promote, redact, select};

/// How many issues to fetch per project, relative to `max_per_run`.
///
/// Filters run *after* the fetch, so fetching exactly the cap would starve a
/// project whose most recent issues are all below `min_events` — the sweep
/// would report "nothing qualified" having never looked at the issues that
/// would have. Ten times the cap is enough headroom for the default gates
/// without paging a busy project in full.
const FETCH_MULTIPLIER: usize = 10;

/// Hard ceiling on one project's fetch, matching Sentry's page size.
const FETCH_CEILING: usize = 100;

/// One issue that was promoted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Promoted {
    /// The Sentry project slug.
    pub project: String,
    /// The Sentry short id.
    pub short_id: String,
    /// The repository it was promoted into.
    pub repo: String,
    /// The new GitHub issue number.
    pub number: u64,
    /// Whether the Sentry issue was annotated with the GitHub link.
    pub annotated: bool,
}

/// One issue that was not promoted, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skip {
    /// The Sentry project slug.
    pub project: String,
    /// The Sentry short id.
    pub short_id: String,
    /// Why it was skipped.
    pub reason: Skipped,
}

/// What one sweep did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SweepReport {
    /// Issues promoted, in the order they were created.
    pub promoted: Vec<Promoted>,
    /// Issues skipped, with reasons.
    pub skipped: Vec<Skip>,
    /// Projects in `sentry.projects` with no `[[sentry.route]]` entry.
    pub unrouted: Vec<String>,
    /// Projects whose sweep failed, with the error rendered.
    pub failed: Vec<(String, String)>,
    /// Sentry issue ids resolved because their tracking issue is closed.
    pub resolved: Vec<String>,
    /// Per project, the `max_per_run` cap that truncated it.
    pub truncated: Vec<(String, usize)>,
    /// Whether this was a dry run, so a caller cannot report a dry run's
    /// numbers as writes.
    pub dry_run: bool,
}

impl SweepReport {
    /// Whether the sweep wrote nothing anywhere.
    pub fn wrote_nothing(&self) -> bool {
        self.promoted.is_empty() && self.resolved.is_empty()
    }
}

/// What a sweep answers when promotion is disabled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SweepOutcome {
    /// `sentry.enabled` is off; nothing was read or written.
    Disabled,
    /// The sweep ran.
    Ran(Box<SweepReport>),
}

/// Sweep every configured project.
///
/// `dry_run` reads and decides exactly as a live run does, and writes nothing
/// — so the report is what *would* have happened, not a different code path
/// that might disagree with the real one.
pub async fn sweep(
    read: &dyn ForgeRead,
    write: &dyn ForgeWrite,
    sentry: &dyn SentryApi,
    config: &Config,
    dry_run: bool,
) -> Result<SweepOutcome> {
    if !config.sentry.enabled {
        tracing::debug!("sentry promotion is disabled; not sweeping");
        return Ok(SweepOutcome::Disabled);
    }

    let Some(org) = config
        .sentry
        .org
        .as_deref()
        .filter(|o| !o.trim().is_empty())
    else {
        // `config::validate` already refuses this combination, so reaching it
        // means validation was bypassed. Refuse rather than guess an org.
        return Err(crate::Error::config(
            "`sentry.enabled = true` requires `sentry.org`",
        ));
    };

    let mut report = SweepReport {
        dry_run,
        ..SweepReport::default()
    };

    for project in &config.sentry.projects {
        let Some(route) = config.sentry.route_for(project) else {
            // Loud, and recorded — not a debug line somebody has to go looking
            // for. Guessing a repository from a project slug is how issues get
            // opened in someone else's tracker.
            tracing::warn!(
                project = %project,
                "no [[sentry.route]] for this project; skipping it. Add a route naming the \
                 repository its issues belong to — tinysweeper will not guess one."
            );
            report.unrouted.push(project.clone());
            continue;
        };

        let Some(repo) = RepoId::parse(&route.repo) else {
            tracing::warn!(
                project = %project,
                repo = %route.repo,
                "sentry.route repo is not owner/name; skipping the project"
            );
            report.failed.push((
                project.clone(),
                format!("`{}` is not owner/name", route.repo),
            ));
            continue;
        };

        if let Err(err) = sweep_project(
            read,
            write,
            sentry,
            config,
            org,
            project,
            &repo,
            dry_run,
            &mut report,
        )
        .await
        {
            // One project's failure must not take the rest down with it.
            tracing::warn!(project = %project, %err, "sentry sweep failed for this project");
            report.failed.push((project.clone(), err.to_string()));
        }
    }

    tracing::info!(
        promoted = report.promoted.len(),
        skipped = report.skipped.len(),
        unrouted = report.unrouted.len(),
        resolved = report.resolved.len(),
        failed = report.failed.len(),
        dry_run,
        "sentry sweep complete"
    );

    Ok(SweepOutcome::Ran(Box::new(report)))
}

/// Sweep one project. See the module docs for why the steps are in this order.
#[allow(clippy::too_many_arguments)]
async fn sweep_project(
    read: &dyn ForgeRead,
    write: &dyn ForgeWrite,
    sentry: &dyn SentryApi,
    config: &Config,
    org: &str,
    project: &str,
    repo: &RepoId,
    dry_run: bool,
    report: &mut SweepReport,
) -> Result<()> {
    let fetch_limit = config
        .sentry
        .max_per_run
        .saturating_mul(FETCH_MULTIPLIER)
        .clamp(1, FETCH_CEILING);

    let fetched = sentry.unresolved_issues(project, fetch_limit).await?;
    tracing::debug!(
        project,
        fetched = fetched.len(),
        "fetched unresolved issues"
    );

    // Step 1a — the pure gates.
    let filtered = select::filter(fetched, &config.sentry);
    for rejected in filtered.rejected {
        tracing::debug!(project, short_id = %rejected.short_id, reason = %rejected.reason, "skipped");
        report.skipped.push(Skip {
            project: project.to_string(),
            short_id: rejected.short_id,
            reason: rejected.reason,
        });
    }

    // Step 2 — dedupe against GitHub, before the cap.
    let mut candidates = Vec::new();
    for issue in filtered.selected {
        // Scrubbed on both sides, deliberately.
        //
        // `promote::body` writes the marker from `SafeIssue.short_id` and
        // `project`, which `redact::project` has scrubbed. Looking up with the
        // RAW values meant that if any `scrub_patterns` entry touched either,
        // the key searched for could never match the marker written — and the
        // sweep re-promoted the same issue on every run, forever. Duplicates
        // are the failure mode that scales, so both sides must derive the key
        // the same way.
        //
        // KNOWN EDGE, deliberately not fixed here: scrubbing is lossy, so a
        // pattern that matches part of a short id can collapse two distinct
        // ids to one string, and the second issue then looks already-tracked
        // and is silently never promoted. That is quieter than a duplicate but
        // it is still wrong. The real answer is that structural identifiers
        // should not be scrubbed at all, which is a design change rather than
        // a fix — tracked as a follow-up.
        // `marker_component`, not `scrub_text`: the marker is built from
        // values that promotion also truncates, so scrubbing alone would still
        // miss a marker for any value over the cap.
        let dedupe_short_id =
            redact::marker_component(&issue.short_id, &config.sentry.scrub_patterns);
        let dedupe_project = redact::marker_component(project, &config.sentry.scrub_patterns);
        match dedupe::find_tracked(read, repo, org, &dedupe_project, &dedupe_short_id).await? {
            Tracked::Yes(tracking) => {
                tracing::debug!(
                    project,
                    short_id = %issue.short_id,
                    number = tracking.number,
                    "already tracked"
                );
                report.skipped.push(Skip {
                    project: project.to_string(),
                    short_id: issue.short_id.clone(),
                    reason: Skipped::AlreadyTracked {
                        number: tracking.number,
                    },
                });

                // The close half: a tracking issue somebody has closed means
                // the error is fixed, so Sentry should stop reporting it.
                if !dry_run
                    && link::resolve_if_fixed(sentry, &config.sentry, &issue.id, &tracking).await?
                {
                    report.resolved.push(issue.id.clone());
                }
            }
            Tracked::Undedupable => {
                // Already warned inside `find_tracked`. Recorded here as well
                // so the report's tally accounts for every fetched issue —
                // "fetched 10, promoted 0, skipped 0" is a report that does
                // not add up, and one nobody can act on.
                report.skipped.push(Skip {
                    project: project.to_string(),
                    short_id: issue.short_id.clone(),
                    reason: Skipped::Undedupable,
                });
            }
            Tracked::No => candidates.push(issue),
        }
    }

    // Step 1b — the cap, over what would actually be created.
    let capped = select::apply_cap(candidates, &config.sentry);
    if !capped.rejected.is_empty() {
        // The acceptance criterion: a truncated sweep says so, at info level,
        // naming the cap. A silent truncation reads as a complete sweep.
        tracing::info!(
            project,
            cap = config.sentry.max_per_run,
            dropped = capped.rejected.len(),
            "sentry.max_per_run truncated this sweep; the remaining issues will be \
             promoted on the next run"
        );
        report
            .truncated
            .push((project.to_string(), config.sentry.max_per_run));
        for rejected in capped.rejected {
            report.skipped.push(Skip {
                project: project.to_string(),
                short_id: rejected.short_id,
                reason: rejected.reason,
            });
        }
    }

    // Steps 3 and 4.
    for issue in capped.selected {
        // Best-effort: an issue whose event body Sentry has expired is still
        // worth promoting, just without frames.
        let event = match sentry.latest_event(&issue.id).await {
            Ok(event) => event,
            Err(err) => {
                tracing::debug!(project, %err, "no latest event; promoting without frames");
                None
            }
        };

        // The redaction boundary. Nothing below this line touches `issue`
        // again except for its opaque Sentry id.
        let safe = redact::project(&issue, event.as_ref(), project, &config.sentry);

        if dry_run {
            tracing::info!(
                project,
                short_id = %safe.short_id,
                title = %promote::title(&safe),
                "would promote"
            );
            report.promoted.push(Promoted {
                project: project.to_string(),
                short_id: safe.short_id.clone(),
                repo: format!("{}/{}", repo.owner, repo.name),
                number: 0,
                annotated: false,
            });
            continue;
        }

        let number = promote::promote(read, write, config, repo, org, &safe).await?;
        let annotated = link::annotate(sentry, &config.sentry, &issue.id, repo, number)
            .await
            .unwrap_or_else(|err| {
                // Non-fatal: the durable half of the link is the marker in the
                // GitHub issue body, which is already written.
                tracing::warn!(number, %err, "could not annotate the sentry issue");
                false
            });

        tracing::info!(
            project,
            short_id = %safe.short_id,
            number,
            annotated,
            "promoted a sentry issue"
        );

        report.promoted.push(Promoted {
            project: project.to_string(),
            short_id: safe.short_id.clone(),
            repo: format!("{}/{}", repo.owner, repo.name),
            number,
            annotated,
        });
    }

    Ok(())
}

#[cfg(test)]
#[path = "sweep_test.rs"]
mod test;
