//! Deterministic Markdown rendering for the one durable pull-request comment.

use std::fmt::Write as _;

use crate::VERSION;
use crate::app::review::Proposal;
use crate::config::types::{Config, Severity, SummarySection};
use crate::summary::types::ReviewSummary;

/// Marker for the durable review comment.
pub const MARKER: &str = "<!-- tinysweeper:review-hub -->";
/// Marker used by the former standalone change-map comment.
pub const LEGACY_MARKER: &str = "<!-- tinysweeper:change-map -->";

/// Render the initial state while model work is running, retaining the last report.
pub fn in_progress(head_sha: &str, previous: Option<&str>) -> String {
    let mut body = format!(
        "{MARKER}\n\n# Tiny Sweeper review\n\n> ⏳ **Reviewing `{}` now.** The last completed report, when available, remains below until this pass finishes.\n",
        short(head_sha)
    );
    if let Some(previous) = previous.and_then(trustworthy_report) {
        body.push_str("\n<details>\n<summary>Previous completed report</summary>\n\n");
        body.push_str(previous);
        body.push_str("\n</details>\n");
    }
    body
}

/// Render a prominent failure without destroying the prior trustworthy report.
pub fn failed(head_sha: &str, message: &str, previous: Option<&str>) -> String {
    let mut body = format!(
        "{MARKER}\n\n# Tiny Sweeper review\n\n> ⚠️ **Review failed for `{}`.** {}\n",
        short(head_sha),
        crate::scan::scrub(message)
    );
    if let Some(previous) = previous.and_then(trustworthy_report) {
        body.push_str("\n<details open>\n<summary>Last completed report</summary>\n\n");
        body.push_str(previous);
        body.push_str("\n</details>\n");
    }
    body
}

/// Render a completed review. Verdict-bearing content comes only from `proposal`.
pub fn render(config: &Config, proposal: &Proposal) -> String {
    let summary = proposal.summary.as_ref().cloned().unwrap_or_default();
    let mut body = String::with_capacity(8_192);
    let _ = write!(body, "{MARKER}\n\n# Tiny Sweeper review\n\n");
    let executive = if summary.executive_summary.trim().is_empty() {
        "Tiny Sweeper completed its review. The deterministic snapshot and lane details below are the authoritative result."
    } else {
        summary.executive_summary.trim()
    };
    let _ = writeln!(body, "{executive}\n");
    let _ = writeln!(body, "**State:** {}  ", state(proposal));
    let _ = writeln!(body, "**Priority:** {}  ", priority(proposal));
    let _ = writeln!(body, "**Reviewed head:** `{}`", short(&proposal.head_sha));
    let _ = writeln!(
        body,
        "**Updated:** {} (Unix time)",
        summary.updated_at_epoch
    );

    for section in &config.summary.sections {
        match section {
            SummarySection::Snapshot => snapshot(&mut body, proposal, &summary),
            SummarySection::Changes => changes(&mut body, &summary),
            SummarySection::Features => features(&mut body, &summary),
            SummarySection::Tests => tests(&mut body, proposal, &summary),
            SummarySection::Findings => findings(&mut body, proposal),
            SummarySection::BeforeMerge => before_merge(&mut body, proposal),
            SummarySection::Flow if config.overview.enabled => flow(&mut body, proposal),
            SummarySection::Flow => {}
            SummarySection::AgentDetails => agent_details(&mut body, proposal, &summary),
            SummarySection::RunDetails => run_details(&mut body, proposal, &summary),
        }
    }
    let _ = write!(
        body,
        "\n![tinysweeper {VERSION}](https://img.shields.io/badge/tinysweeper-{}-8b949e?style=flat-square)\n",
        VERSION.replace('-', "--")
    );
    body
}

fn snapshot(out: &mut String, proposal: &Proposal, summary: &ReviewSummary) {
    let (active, noted, resolved, pending) = counts(proposal);
    out.push_str("\n## Review snapshot\n\n| Change surface | Files | Review signal | Count |\n|---|---:|---|---:|\n");
    let _ = writeln!(
        out,
        "| Production | {} | Active findings | {active} |",
        summary.surface.production
    );
    let _ = writeln!(
        out,
        "| Tests | {} | Noted findings | {noted} |",
        summary.surface.tests
    );
    let _ = writeln!(
        out,
        "| Documentation | {} | Resolved findings | {resolved} |",
        summary.surface.documentation
    );
    let _ = writeln!(
        out,
        "| Configuration | {} | Pending checks/questions | {pending} |",
        summary.surface.configuration
    );
    let _ = writeln!(
        out,
        "\n**Completeness:** {}  ",
        if proposal.complete() {
            "Complete"
        } else {
            "Incomplete"
        }
    );
    let assessment = if summary.tests.is_empty() {
        "No supported feature-to-test mapping was available; this does not mean tests are absent or passed."
    } else {
        "Test coverage is assessed from changed tests and lane evidence; execution is not claimed without trusted check data."
    };
    let _ = writeln!(out, "**Test assessment:** {assessment}");
}

fn changes(out: &mut String, summary: &ReviewSummary) {
    out.push_str("\n## What changed\n\n");
    out.push_str(if summary.changes.trim().is_empty() {
        "No supported behavioral explanation was produced."
    } else {
        summary.changes.trim()
    });
    out.push('\n');
}

fn features(out: &mut String, summary: &ReviewSummary) {
    out.push_str("\n## Features\n\n");
    if summary.features.is_empty() {
        out.push_str("None identified with supported citations.\n");
        return;
    }
    for feature in &summary.features {
        let _ = writeln!(
            out,
            "- **{} — {}:** {} _({})_",
            feature.kind.label(),
            feature.name,
            feature.impact,
            feature.citations.join(", ")
        );
    }
    if summary.omitted_features > 0 {
        let _ = writeln!(
            out,
            "- _{} additional supported feature(s) omitted by the configured limit._",
            summary.omitted_features
        );
    }
}

fn tests(out: &mut String, proposal: &Proposal, summary: &ReviewSummary) {
    out.push_str("\n## Tests\n\n");
    if summary.tests.is_empty() {
        out.push_str(
            "No supported feature-to-test mapping was produced. Test execution is not inferred.\n",
        );
    }
    for test in &summary.tests {
        let _ = writeln!(
            out,
            "- **{} — {}:** {} _({})_",
            test.kind,
            test.behavior,
            test.assessment,
            test.citations.join(", ")
        );
    }
    if summary.omitted_tests > 0 {
        let _ = writeln!(
            out,
            "- _{} additional supported test mapping(s) omitted by the configured limit._",
            summary.omitted_tests
        );
    }
    for lane in &proposal.lanes {
        if lane.lane == crate::config::types::LaneId::Tests && !lane.unanswered.is_empty() {
            let _ = writeln!(out, "- **Unreviewed:** {}", lane.unanswered.join(", "));
        }
    }
    let current_titles: std::collections::BTreeSet<&str> = proposal
        .findings()
        .map(|finding| finding.title.as_str())
        .collect();
    let carried: Vec<_> = proposal
        .prior_findings
        .iter()
        .filter(|title| !current_titles.contains(title.as_str()))
        .collect();
    if !carried.is_empty() {
        out.push_str("\n**Previously reported and still active**\n");
        for title in carried {
            let _ = writeln!(out, "- {title}");
        }
    }
}

fn findings(out: &mut String, proposal: &Proposal) {
    out.push_str("\n## Findings\n\n");
    if proposal.findings().next().is_none() {
        out.push_str("No active actionable findings.\n");
    }
    for lane in &proposal.lanes {
        for finding in &lane.findings {
            let _ = writeln!(
                out,
                "- **{} · {} · {}** — {} (`{}`{})",
                severity(finding.severity),
                lane.lane,
                finding.title,
                concise(&finding.body),
                finding.path,
                finding
                    .line
                    .map(|line| format!(":{line}"))
                    .unwrap_or_default()
            );
        }
    }
    let noted: Vec<_> = proposal
        .lanes
        .iter()
        .flat_map(|lane| lane.noted.iter())
        .collect();
    if !noted.is_empty() {
        out.push_str("\n**Lower-confidence notes**\n");
        for finding in noted {
            let _ = writeln!(
                out,
                "- {} · {} — {} (`{}`)",
                severity(finding.severity),
                finding.lane,
                finding.title,
                finding.path
            );
        }
    }
    let resolved: Vec<_> = proposal
        .lanes
        .iter()
        .flat_map(|lane| lane.resolved.iter())
        .collect();
    if !resolved.is_empty() {
        out.push_str("\n**Resolved this pass**\n");
        for title in resolved {
            let _ = writeln!(out, "- {title}");
        }
    }
    let pending: Vec<_> = proposal
        .lanes
        .iter()
        .flat_map(|lane| lane.pending.iter())
        .collect();
    if !pending.is_empty() {
        let _ = writeln!(
            out,
            "\n**Pending checks:** {}",
            pending.into_iter().cloned().collect::<Vec<_>>().join(", ")
        );
    }
    if !proposal.unanswered().is_empty() {
        let _ = writeln!(
            out,
            "\n**Could not review:** {}",
            proposal.unanswered().join(", ")
        );
    }
}

fn before_merge(out: &mut String, proposal: &Proposal) {
    out.push_str("\n## Before merge\n\n");
    let mut any = false;
    for lane in &proposal.lanes {
        for finding in &lane.findings {
            if finding.severity >= Severity::High {
                any = true;
                let _ = writeln!(
                    out,
                    "- [ ] Address **{}** (`{}`).",
                    finding.title, finding.path
                );
            }
        }
        if !lane.pending.is_empty() {
            any = true;
            let _ = writeln!(out, "- [ ] Wait for {}.", lane.pending.join(", "));
        }
        if !lane.unanswered.is_empty() {
            any = true;
            let _ = writeln!(
                out,
                "- [ ] Complete the {} review for {}.",
                lane.lane,
                lane.unanswered.join(", ")
            );
        }
    }
    if !proposal.unreviewed.is_empty() {
        any = true;
        let _ = writeln!(
            out,
            "- [ ] Review unavailable diffs: {}.",
            proposal.unreviewed.join(", ")
        );
    }
    if !any {
        out.push_str("None.\n");
    }
}

fn flow(out: &mut String, proposal: &Proposal) {
    let Some(diagram) = proposal
        .overview
        .as_ref()
        .and_then(crate::overview::flowchart)
    else {
        return;
    };
    out.push_str("\n## How this fits together\n\n");
    out.push_str(&diagram);
}

fn agent_details(out: &mut String, proposal: &Proposal, summary: &ReviewSummary) {
    out.push_str("\n<details>\n<summary><strong>Agent review details</strong></summary>\n\n");
    for lane in &proposal.lanes {
        let _ = writeln!(
            out,
            "### {}\n\n- **Conclusion:** {:?}\n- **Scope reviewed:** {}",
            lane.lane,
            lane.conclusion,
            if lane.unanswered.is_empty() {
                "all assigned evidence".into()
            } else {
                format!("incomplete; unanswered: {}", lane.unanswered.join(", "))
            }
        );
        if let Some(observations) = summary.positive_observations.get(&lane.lane) {
            for observation in observations {
                let _ = writeln!(out, "- **Positive:** {observation}");
            }
        }
        let _ = writeln!(out, "- **Lane summary:** {}", lane.summary);
        if !lane.pending.is_empty() {
            let _ = writeln!(
                out,
                "- **Unresolved questions/checks:** {}",
                lane.pending.join(", ")
            );
        }
        for finding in &lane.findings {
            let _ = writeln!(
                out,
                "- **Evidence:** `{}` — {}",
                finding.path, finding.title
            );
        }
        out.push('\n');
    }
    out.push_str("</details>\n");
}

fn run_details(out: &mut String, proposal: &Proposal, summary: &ReviewSummary) {
    out.push_str("\n<details>\n<summary><strong>Evidence and run details</strong></summary>\n\n");
    let _ = writeln!(
        out,
        "- **Models:** {}",
        if proposal.models.is_empty() {
            "None".into()
        } else {
            proposal.models.join(", ")
        }
    );
    let _ = writeln!(out, "- **Spend:** ${:.6}", proposal.cost_usd);
    let _ = writeln!(
        out,
        "- **Tokens:** {} input · {} output · {} cached · {} embedding",
        proposal.input_tokens,
        proposal.output_tokens,
        proposal.cached_tokens,
        proposal.embed_tokens
    );
    if summary.cache_chain_restarted {
        out.push_str("- **Continuity:** summary cache chain restarted at the storage ceiling.\n");
    }
    if !summary.history.is_empty() {
        out.push_str("\n| Head | State | Pass summary |\n|---|---|---|\n");
        for pass in &summary.history {
            let _ = writeln!(
                out,
                "| `{}` | {} | {} (at {}) |",
                short(&pass.head_sha),
                pass.state,
                pass.summary,
                pass.reviewed_at_epoch,
            );
        }
    }
    out.push_str("\n</details>\n");
}

fn state(proposal: &Proposal) -> &'static str {
    if proposal.skipped.is_some() || !proposal.complete() {
        "Incomplete"
    } else if proposal.blocked() {
        "Changes requested"
    } else if proposal.lanes.iter().any(|lane| !lane.pending.is_empty()) {
        "Reviewing pending checks"
    } else {
        "Ready for maintainer review"
    }
}

fn priority(proposal: &Proposal) -> &'static str {
    proposal
        .lanes
        .iter()
        .filter_map(|lane| lane.highest_severity)
        .max()
        .map(severity)
        .unwrap_or("none")
}

fn severity(severity: Severity) -> &'static str {
    match severity {
        Severity::Critical => "critical",
        Severity::High => "high",
        Severity::Medium => "medium",
        Severity::Low => "low",
    }
}
fn counts(proposal: &Proposal) -> (usize, usize, usize, usize) {
    let current_titles: std::collections::BTreeSet<&str> = proposal
        .findings()
        .map(|finding| finding.title.as_str())
        .collect();
    let carried = proposal
        .prior_findings
        .iter()
        .filter(|title| !current_titles.contains(title.as_str()))
        .count();
    (
        proposal
            .lanes
            .iter()
            .map(|lane| lane.findings.len())
            .sum::<usize>()
            + carried,
        proposal.lanes.iter().map(|lane| lane.noted.len()).sum(),
        proposal.lanes.iter().map(|lane| lane.resolved.len()).sum(),
        proposal
            .lanes
            .iter()
            .map(|lane| lane.pending.len() + lane.unanswered.len())
            .sum::<usize>()
            + proposal.unreviewed.len(),
    )
}
fn short(sha: &str) -> &str {
    sha.get(..sha.len().min(12)).unwrap_or(sha)
}
fn concise(body: &str) -> String {
    body.lines()
        .next()
        .unwrap_or(body)
        .chars()
        .take(180)
        .collect()
}
fn trustworthy_report(body: &str) -> Option<&str> {
    if body.contains(MARKER)
        && body.contains("**Reviewed head:**")
        && !body.contains("**Reviewing `")
    {
        Some(body)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_retains_only_a_completed_bot_report() {
        let prior = format!("{MARKER}\n\n**Reviewed head:** `abc`");
        assert!(in_progress("def", Some(&prior)).contains("Previous completed report"));
        assert!(
            !in_progress("def", Some("contributor text")).contains("Previous completed report")
        );
    }
}
