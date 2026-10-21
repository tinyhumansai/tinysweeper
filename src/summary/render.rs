//! Deterministic Markdown rendering for the one durable pull-request comment.

use std::fmt::Write as _;

use crate::VERSION;
use crate::app::review::Proposal;
use crate::config::types::{Config, Severity, SummarySection};
use crate::markdown::{code_span, prose};
use crate::summary::types::ReviewSummary;

fn render_citations(citations: &[String], proposal: &Proposal) -> String {
    citations
        .iter()
        .map(|citation| crate::markdown::citation(citation, &proposal.repo, &proposal.head_sha))
        .collect::<Vec<_>>()
        .join(", ")
}

fn code_list<'a>(paths: impl Iterator<Item = &'a str>) -> String {
    paths.map(code_span).collect::<Vec<_>>().join(", ")
}

/// Marker for the durable review comment.
pub const MARKER: &str = "<!-- tinysweeper:review-hub -->";
/// Marker used by the former standalone change-map comment.
pub const LEGACY_MARKER: &str = "<!-- tinysweeper:change-map -->";
const LAST_REPORT_START: &str = "<!-- tinysweeper:last-report:start -->";
const LAST_REPORT_END: &str = "<!-- tinysweeper:last-report:end -->";

/// Render the initial state while model work is running, retaining the last report.
pub fn in_progress(head_sha: &str, previous: Option<&str>) -> String {
    let mut body = format!(
        "{MARKER}\n\n# Tiny Sweeper review\n\n> ⏳ **Reviewing `{}` now.** The last completed report, when available, remains below until this pass finishes.\n",
        short(head_sha)
    );
    if let Some(previous) = previous.and_then(trustworthy_report) {
        body.push_str("\n<details>\n<summary>Previous completed report</summary>\n\n");
        body.push_str(LAST_REPORT_START);
        body.push('\n');
        body.push_str(previous);
        body.push('\n');
        body.push_str(LAST_REPORT_END);
        body.push_str("\n</details>\n");
    }
    body
}

/// Render a prominent failure without destroying the prior trustworthy report.
pub fn failed(head_sha: &str, message: &str, previous: Option<&str>) -> String {
    let reason = safe_failure_reason(message);
    let mut body = format!(
        "{MARKER}\n\n# Tiny Sweeper review\n\n> ⚠️ **Review of `{}` {}.**\n",
        short(head_sha),
        reason,
    );
    if let Some(previous) = previous.and_then(trustworthy_report) {
        if let Some(sha) = reviewed_sha(previous) {
            let _ = writeln!(body, " The last completed review (`{sha}`) is shown below.");
        }
        body.push_str("\n<details open>\n<summary>Last completed report</summary>\n\n");
        body.push_str(LAST_REPORT_START);
        body.push('\n');
        body.push_str(previous);
        body.push('\n');
        body.push_str(LAST_REPORT_END);
        body.push_str("\n</details>\n");
    }
    body
}

/// Render a completed review. Verdict-bearing content comes only from `proposal`.
pub fn render(config: &Config, proposal: &Proposal) -> String {
    let summary = proposal.summary.as_ref().cloned().unwrap_or_default();
    let mut body = String::with_capacity(8_192);
    let _ = write!(body, "{MARKER}\n\n# Tiny Sweeper review\n\n");
    // Failure reports trust only a previously completed report. Recording the
    // reviewed head gives that preservation boundary an explicit identity;
    // partial reports must not be mistaken for completed work.
    if proposal.complete() {
        let _ = writeln!(
            body,
            "**Reviewed head:** {}\n",
            code_span(&proposal.head_sha)
        );
    }
    let show_findings = config.summary.sections.contains(&SummarySection::Findings);
    let has_findings = show_findings && has_finding_signal(proposal);
    if has_findings {
        findings(&mut body, proposal);
        let _ = writeln!(
            body,
            "\n**Verdict:** {} · **Priority:** {}",
            state(proposal),
            priority(proposal)
        );
        coverage_line(&mut body, proposal, &summary);
        lane_gaps(&mut body, proposal);
    } else {
        let _ = writeln!(
            body,
            "**Verdict:** {} · **Priority:** {}",
            state(proposal),
            priority(proposal)
        );
        coverage_line(&mut body, proposal, &summary);
        lane_gaps(&mut body, proposal);
        let executive = summary.executive_summary.trim();
        let sentence = if executive.is_empty() && proposal.complete() {
            "The review found no actionable issues.".to_string()
        } else if executive.is_empty() || contains_run_diagnostics(executive) {
            "No actionable findings were reported in the available review results.".to_string()
        } else {
            bounded_sentence(executive, 50)
        };
        let _ = writeln!(body, "{}\n", prose(&sentence));
    }

    body.push_str("<details>\n<summary>Diagnostics and supporting details</summary>\n\n");
    summary_details(&mut body, &summary);
    coverage_details(&mut body, proposal, &summary);
    for section in &config.summary.sections {
        match section {
            SummarySection::Snapshot => snapshot(&mut body, proposal, &summary),
            SummarySection::Changes => changes(&mut body, &summary),
            SummarySection::Features => features(&mut body, proposal, &summary),
            SummarySection::Tests => tests(&mut body, proposal, &summary),
            SummarySection::Findings if !has_findings => findings(&mut body, proposal),
            SummarySection::Findings => {}
            SummarySection::BeforeMerge => before_merge(&mut body, proposal),
            SummarySection::Flow if config.overview.enabled => flow(&mut body, proposal),
            SummarySection::Flow => {}
            SummarySection::AgentDetails => agent_details(&mut body, proposal, &summary),
            SummarySection::RunDetails => run_details(&mut body, proposal, &summary),
        }
    }
    body.push_str("</details>\n");
    let _ = write!(
        body,
        "\n![tinysweeper {VERSION}](https://img.shields.io/badge/tinysweeper-{}-8b949e?style=flat-square)\n",
        VERSION.replace('-', "--")
    );
    body
}

fn first_sentence(text: &str) -> &str {
    let text = text.lines().next().unwrap_or(text).trim();
    for (index, character) in text.char_indices() {
        if matches!(character, '.' | '!' | '?')
            && text[index + character.len_utf8()..]
                .chars()
                .next()
                .is_none_or(char::is_whitespace)
        {
            return &text[..index + character.len_utf8()];
        }
    }
    text
}

fn contains_run_diagnostics(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    ["cache chain", "index is behind", "ladder/", "per evidence"]
        .iter()
        .any(|term| lower.contains(term))
}

fn bounded_sentence(text: &str, limit: usize) -> String {
    let sentence = first_sentence(text);
    let mut chars = sentence.chars();
    let clipped: String = chars.by_ref().take(limit).collect();
    if chars.next().is_some() {
        format!("{}…", clipped.trim_end())
    } else {
        clipped
    }
}

fn bounded_paths(paths: &[String], shown: usize) -> String {
    let listed = paths
        .iter()
        .take(shown)
        .map(|path| {
            let mut chars = path.chars();
            let clipped: String = chars.by_ref().take(16).collect();
            let path = if chars.next().is_some() {
                format!("{}…", clipped)
            } else {
                clipped
            };
            code_span(&path)
        })
        .collect::<Vec<_>>()
        .join(", ");
    let remaining = paths.len().saturating_sub(shown.min(paths.len()));
    if remaining == 0 {
        listed
    } else if listed.is_empty() {
        format!("{remaining} additional unreviewed files")
    } else {
        format!("{listed}, and {remaining} additional unreviewed files")
    }
}

fn coverage_details(out: &mut String, proposal: &Proposal, summary: &ReviewSummary) {
    let mut paths = std::collections::BTreeSet::new();
    if let Some(coverage) = &summary.file_coverage {
        paths.extend(coverage.unreviewed.iter().cloned());
    }
    paths.extend(proposal.unreviewed.iter().cloned());
    if !paths.is_empty() {
        out.push_str("\n**Unreviewed paths**\n\n");
        for path in &paths {
            let _ = writeln!(out, "- {}", code_span(path));
        }
    }
    if let Some(partial) = &proposal.partial_review
        && !partial.omitted.is_empty()
    {
        out.push_str("\n**Partial review reasons**\n\n");
        for omission in &partial.omitted {
            let lane = omission
                .lane
                .map(|lane| format!("{}: ", lane))
                .unwrap_or_default();
            let _ = writeln!(
                out,
                "- {}{} — {}",
                lane,
                code_span(&omission.path),
                partial_reason_description(omission.reason)
            );
        }
    }
}

fn partial_reason_label(reason: Option<crate::app::review::PartialReviewReason>) -> &'static str {
    match reason {
        Some(crate::app::review::PartialReviewReason::NotSelected) => "not reviewed (large PR)",
        Some(
            crate::app::review::PartialReviewReason::SoftDeadline
            | crate::app::review::PartialReviewReason::DrainDeadline,
        ) => "not reviewed (time limit)",
        Some(crate::app::review::PartialReviewReason::CallLimit) => "not reviewed (call budget)",
        Some(crate::app::review::PartialReviewReason::Cancelled) => {
            "not reviewed (review cancelled)"
        }
        Some(crate::app::review::PartialReviewReason::UnavailableDiff) => {
            "not reviewed (unavailable diff)"
        }
        Some(crate::app::review::PartialReviewReason::Unanswered) | None => "not reviewed",
    }
}

fn render_unreviewed_labels(proposal: &Proposal, paths: &[String]) -> String {
    let mut reasons = std::collections::BTreeMap::<
        &str,
        std::collections::BTreeSet<crate::app::review::PartialReviewReason>,
    >::new();
    for omission in proposal
        .partial_review
        .iter()
        .flat_map(|partial| &partial.omitted)
    {
        reasons
            .entry(omission.path.as_str())
            .or_default()
            .insert(omission.reason);
    }
    let mut grouped =
        std::collections::BTreeMap::<&'static str, std::collections::BTreeSet<String>>::new();
    for path in paths {
        if let Some(path_reasons) = reasons.get(path.as_str()) {
            for reason in path_reasons {
                grouped
                    .entry(partial_reason_label(Some(*reason)))
                    .or_default()
                    .insert(path.clone());
            }
        } else {
            grouped
                .entry(partial_reason_label(None))
                .or_default()
                .insert(path.clone());
        }
    }
    grouped
        .into_iter()
        .map(|(label, paths)| {
            format!(
                "{label}: {}",
                bounded_paths(&paths.into_iter().collect::<Vec<_>>(), 2)
            )
        })
        .collect::<Vec<_>>()
        .join("; ")
}

fn partial_reason_description(reason: crate::app::review::PartialReviewReason) -> &'static str {
    use crate::app::review::PartialReviewReason;
    match reason {
        PartialReviewReason::UnavailableDiff => "the forge did not provide a usable diff",
        PartialReviewReason::NotSelected => "not selected by the large-review budget",
        PartialReviewReason::SoftDeadline | PartialReviewReason::DrainDeadline => {
            "not reviewed (time limit)"
        }
        PartialReviewReason::CallLimit => "not reviewed (call budget)",
        PartialReviewReason::Cancelled => "not reviewed (review cancelled)",
        PartialReviewReason::Unanswered => "the lane did not answer for this path",
    }
}

fn summary_details(out: &mut String, summary: &ReviewSummary) {
    let executive = summary.executive_summary.trim();
    if !executive.is_empty() {
        out.push_str("## Full summary\n\n");
        out.push_str(&prose(executive));
        out.push_str("\n\n");
    }
}

fn has_finding_signal(proposal: &Proposal) -> bool {
    proposal.findings().next().is_some()
        || proposal
            .prior_findings
            .iter()
            .any(|title| !proposal.reported().any(|finding| finding.title == *title))
        || proposal.overflowed().next().is_some()
        || proposal
            .lanes
            .iter()
            .any(|lane| !lane.noted.is_empty() || !lane.resolved.is_empty())
}

fn coverage_line(out: &mut String, proposal: &Proposal, summary: &ReviewSummary) {
    let mut uncovered = std::collections::BTreeSet::new();
    if let Some(coverage) = &summary.file_coverage {
        uncovered.extend(coverage.unreviewed.iter().cloned());
    }
    uncovered.extend(proposal.unreviewed.iter().cloned());
    if let Some(coverage) = &summary.file_coverage {
        let _ = write!(
            out,
            "**Coverage:** Reviewed {} of {} files",
            coverage.total.saturating_sub(uncovered.len()),
            coverage.total
        );
        if !uncovered.is_empty() {
            let _ = write!(
                out,
                "; {}",
                render_unreviewed_labels(proposal, &uncovered.into_iter().collect::<Vec<_>>())
            );
        }
        out.push_str(".  \n");
    } else if !uncovered.is_empty() {
        let _ = writeln!(
            out,
            "**Coverage:** File coverage is unavailable; {}.",
            render_unreviewed_labels(proposal, &uncovered.into_iter().collect::<Vec<_>>())
        );
    } else {
        out.push_str("**Coverage:** File coverage is unavailable.  \n");
    }
}

fn lane_gaps(out: &mut String, proposal: &Proposal) {
    let gaps: Vec<_> = proposal
        .lanes
        .iter()
        .filter(|lane| {
            !lane.completed && lane.unanswered.iter().any(|item| item == &lane.check_name)
        })
        .collect();
    if gaps.is_empty() {
        return;
    }
    let names = gaps
        .iter()
        .map(|lane| lane.lane.to_string())
        .collect::<Vec<_>>();
    let _ = writeln!(
        out,
        "**Lane gaps:** The {} reviewer{} did not respond.  ",
        prose(&names.join(", ")),
        if names.len() == 1 { "" } else { "s" }
    );
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

/// The complete "no summary" texts earlier versions stored in `changes`, each
/// matched whole. A stored summary is carried forward from push to push, so
/// these keep arriving long after the code that wrote them is gone; a prefix
/// match would also swallow a real, supported summary that merely begins the
/// same way.
const LEGACY_UNSUPPORTED_CHANGES: [&str; 2] = [
    "No supported behavioral explanation was produced.",
    "The review could not produce a supported behavioral summary; inspect the cited changed surface and lane details below.",
];

/// "What changed", or nothing at all when no supported summary exists. A
/// line saying the summary failed tells the reader nothing the absence of the
/// section does not, and it was on most pull requests.
fn changes(out: &mut String, summary: &ReviewSummary) {
    let changes = summary.changes.trim();
    if changes.is_empty() || LEGACY_UNSUPPORTED_CHANGES.contains(&changes) {
        return;
    }
    out.push_str("\n## What changed\n\n");
    out.push_str(&prose(changes));
    out.push('\n');
}

fn features(out: &mut String, proposal: &Proposal, summary: &ReviewSummary) {
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
            prose(&feature.name),
            prose(&feature.impact),
            render_citations(&feature.citations, proposal)
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
            prose(&test.kind),
            prose(&test.behavior),
            prose(&test.assessment),
            render_citations(&test.citations, proposal)
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
        if lane.lane == crate::config::types::LaneId::Tests
            && !lane.unanswered.is_empty()
            && !lane.unanswered.iter().any(|item| item == &lane.check_name)
        {
            let _ = writeln!(
                out,
                "- **Unreviewed:** {}",
                code_list(lane.unanswered.iter().map(String::as_str))
            );
        }
    }
}

fn carried_findings(out: &mut String, proposal: &Proposal) {
    let current_titles: std::collections::BTreeSet<&str> = proposal
        .reported()
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
            let _ = writeln!(out, "- {}", prose(title));
        }
    }
}

/// Findings that qualified but did not fit the inline-comment budget.
///
/// Keep the finding's location, problem, and suggested fix visible even when
/// its inline conversation missed the comment budget.
fn over_budget(out: &mut String, proposal: &Proposal) {
    let over: Vec<_> = proposal.overflowed().collect();
    if over.is_empty() {
        return;
    }
    let _ = writeln!(
        out,
        "\n**Over the comment budget** — {} more, not posted inline\n",
        over.len()
    );
    for finding in over {
        render_finding(out, finding, proposal);
    }
}

fn findings(out: &mut String, proposal: &Proposal) {
    out.push_str("\n## Findings\n\n");
    let current_titles: std::collections::BTreeSet<&str> = proposal
        .reported()
        .map(|finding| finding.title.as_str())
        .collect();
    let has_carried = proposal
        .prior_findings
        .iter()
        .any(|title| !current_titles.contains(title.as_str()));
    // Only what `over_budget` will actually list counts: a grouped overflow
    // entry is not rendered there, so suppressing the empty state for it
    // would leave the section blank with no reason given.
    let has_over = proposal.overflowed().next().is_some();
    if proposal.findings().next().is_none() && !has_carried && !has_over {
        out.push_str("No active actionable findings.\n");
    }
    for lane in &proposal.lanes {
        for finding in &lane.findings {
            render_finding(out, finding, proposal);
        }
    }
    carried_findings(out, proposal);
    over_budget(out, proposal);
    let noted: Vec<_> = proposal
        .lanes
        .iter()
        .flat_map(|lane| {
            lane.noted
                .iter()
                .filter(|finding| !finding.grouped)
                .map(move |finding| (lane, finding))
        })
        .collect();
    if !noted.is_empty() {
        let has_unverified = noted.iter().any(|(lane, _)| lane.context_degraded);
        if has_unverified {
            let only_unverified = noted.iter().all(|(lane, _)| lane.context_degraded);
            if only_unverified {
                out.push_str("\n**Unverified notes — repository context was unavailable; these are not actionable findings**\n");
            } else {
                out.push_str("\n**Notes**\n");
            }
        } else {
            out.push_str("\n**Advisory notes**\n");
        }
        for (lane, finding) in noted {
            if lane.context_degraded {
                out.push_str("\n**Unverified note — ");
                out.push_str(&prose(&finding.lane.to_string()));
                out.push_str("**\n");
            }
            render_finding(out, finding, proposal);
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
            let _ = writeln!(out, "- {}", prose(title));
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
            prose(&pending.into_iter().cloned().collect::<Vec<_>>().join(", "))
        );
    }
}

fn render_finding(
    out: &mut String,
    finding: &crate::findings::types::Finding,
    proposal: &Proposal,
) {
    let citation = crate::markdown::citation(
        &format!(
            "{}{}",
            finding.path,
            finding
                .line
                .map(|line| format!(":{line}"))
                .unwrap_or_default()
        ),
        &proposal.repo,
        &proposal.head_sha,
    );
    let _ = writeln!(
        out,
        "- **{}{}: {}** ({})  \n  **Problem:** {}  \n  **Suggested fix:**",
        severity(finding.severity),
        if finding.late { " · late" } else { "" },
        prose(&finding.title),
        citation,
        prose(&concise(&finding.body))
    );
    if let Some(suggestion) = &finding.suggestion {
        let block = crate::markdown::code_block(suggestion);
        for line in block.lines() {
            let _ = writeln!(out, "  {line}");
        }
    } else {
        out.push_str("  No concrete fix was supplied.\n");
    }
}

fn unanswered_paths(lane: &crate::app::review::LaneProposal) -> Vec<&str> {
    lane.unanswered
        .iter()
        .filter(|item| item.as_str() != lane.check_name.as_str())
        .map(String::as_str)
        .collect()
}

fn before_merge(out: &mut String, proposal: &Proposal) {
    out.push_str("\n## Before merge\n\n");
    let mut any = false;
    let current_titles: std::collections::BTreeSet<&str> = proposal
        .reported()
        .map(|finding| finding.title.as_str())
        .collect();
    for title in proposal
        .prior_findings
        .iter()
        .filter(|title| !current_titles.contains(title.as_str()))
    {
        any = true;
        let _ = writeln!(out, "- [ ] Address carried finding **{}**.", prose(title));
    }
    for lane in &proposal.lanes {
        for finding in lane
            .findings
            .iter()
            .chain(lane.overflow.iter().filter(|finding| !finding.grouped))
        {
            if finding.severity >= Severity::High {
                any = true;
                let _ = writeln!(
                    out,
                    "- [ ] Address **{}** ({}).",
                    prose(&finding.title),
                    code_span(&finding.path)
                );
            }
        }
        if !lane.pending.is_empty() {
            any = true;
            let _ = writeln!(out, "- [ ] Wait for {}.", prose(&lane.pending.join(", ")));
        }
        if !lane.unanswered.is_empty() {
            any = true;
            let paths = unanswered_paths(lane);
            if paths.is_empty() {
                let _ = writeln!(out, "- [ ] Complete the {} review.", lane.lane);
            } else {
                let _ = writeln!(
                    out,
                    "- [ ] Complete the {} review for {}.",
                    lane.lane,
                    code_list(paths.into_iter())
                );
            }
        }
    }
    if !proposal.unreviewed.is_empty() {
        any = true;
        let _ = writeln!(
            out,
            "- [ ] Review uncovered paths: {}.",
            code_list(proposal.unreviewed.iter().map(String::as_str))
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
                let paths = unanswered_paths(lane);
                if paths.is_empty() {
                    "no lane response".into()
                } else {
                    format!(
                        "incomplete; unanswered files: {}",
                        code_list(paths.into_iter())
                    )
                }
            }
        );
        if let Some(observations) = summary.positive_observations.get(&lane.lane) {
            for observation in observations {
                let _ = writeln!(out, "- **Positive:** {}", prose(observation));
            }
        }
        let _ = writeln!(out, "- **Lane summary:** {}", prose(&lane.summary));
        if !lane.pending.is_empty() {
            let _ = writeln!(
                out,
                "- **Unresolved questions/checks:** {}",
                prose(&lane.pending.join(", "))
            );
        }
        for finding in &lane.findings {
            let _ = writeln!(
                out,
                "- **Evidence:** {} — {}",
                code_span(&finding.path),
                prose(&finding.title)
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
            prose(&proposal.models.join(", "))
        }
    );
    let _ = writeln!(out, "- **Spend:** ${:.6}", proposal.cost_usd);
    if proposal
        .completion_accounting
        .as_ref()
        .is_some_and(|accounting| !accounting.complete)
    {
        out.push_str(
            "- **Completion usage:** incomplete; some provider completion costs are unavailable.\n",
        );
    }
    let _ = writeln!(
        out,
        "- **Tokens:** {} input · {} output · {} cached · {} embedding",
        proposal.input_tokens,
        proposal.output_tokens,
        proposal.cached_tokens,
        proposal.embed_tokens
    );
    if summary.updated_at_epoch > 0 {
        let _ = writeln!(
            out,
            "- **Updated:** {}",
            iso8601_utc(summary.updated_at_epoch)
        );
    }
    if summary.cache_chain_restarted {
        out.push_str("- **Continuity:** summary cache chain restarted at the storage ceiling.\n");
    }
    if !summary.history.is_empty() {
        out.push_str("\n| Head | State | Pass summary |\n|---|---|---|\n");
        for pass in &summary.history {
            let _ = writeln!(
                out,
                "| {} | {} | {}{} |",
                crate::markdown::table_code_span(short(&pass.head_sha)),
                crate::markdown::table_cell(&pass.state),
                crate::markdown::table_cell(&pass.summary),
                if pass.reviewed_at_epoch > 0 {
                    format!(" (at {})", iso8601_utc(pass.reviewed_at_epoch))
                } else {
                    String::new()
                },
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
        .reported()
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
            .map(|lane| {
                lane.findings
                    .iter()
                    .filter(|finding| !finding.grouped)
                    .count()
                    + lane
                        .overflow
                        .iter()
                        .filter(|finding| !finding.grouped)
                        .count()
            })
            .sum::<usize>()
            + carried,
        proposal
            .lanes
            .iter()
            .map(|lane| lane.noted.iter().filter(|finding| !finding.grouped).count())
            .sum(),
        proposal.lanes.iter().map(|lane| lane.resolved.len()).sum(),
        proposal
            .lanes
            .iter()
            .map(|lane| lane.pending.len() + lane.unanswered.len())
            .sum::<usize>()
            + proposal.unreviewed.len(),
    )
}
/// Seconds since the Unix epoch as `YYYY-MM-DDTHH:MM:SSZ`.
///
/// Hand-rolled rather than a date crate for one field: Howard Hinnant's
/// civil-from-days, exact for every date a review will carry.
fn iso8601_utc(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3_600,
        (rem % 3_600) / 60,
        rem % 60
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
    if let Some(start) = body.find(LAST_REPORT_START) {
        let content_start = start + LAST_REPORT_START.len();
        let end = body[content_start..].find(LAST_REPORT_END)? + content_start;
        return trustworthy_report(body[content_start..end].trim_matches('\n'));
    }

    if is_completed_report(body) {
        return Some(body);
    }

    // Older hubs nested the report in a details block before stable delimiters
    // were introduced. Extract that block so an upgrade during a review keeps
    // the report instead of replacing it with another status wrapper.
    let opening = body.find("<details")?;
    let content_start = body[opening..].find("</summary>")? + opening + "</summary>".len();
    let mut cursor = opening;
    let mut depth = 0usize;
    let closing = loop {
        let next_open = body[cursor..]
            .find("<details")
            .map(|offset| cursor + offset);
        let next_close = body[cursor..]
            .find("</details>")
            .map(|offset| cursor + offset);
        match (next_open, next_close) {
            (Some(open), Some(close)) if open < close => {
                depth += 1;
                cursor = open + "<details".len();
            }
            (_, Some(close)) => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    break close;
                }
                cursor = close + "</details>".len();
            }
            _ => return None,
        }
    };
    trustworthy_report(body[content_start..closing].trim())
}

fn is_completed_report(body: &str) -> bool {
    body.contains(MARKER)
        && body.contains("**Reviewed head:**")
        && !body.contains("**Reviewing `")
        && !body.contains("**Review failed for `")
}

fn reviewed_sha(report: &str) -> Option<&str> {
    let line = report
        .lines()
        .find(|line| line.contains("**Reviewed head:**"))?;
    let value = line.split("**Reviewed head:**").nth(1)?.trim();
    value.strip_prefix('`')?.split('`').next()
}

fn safe_failure_reason(message: &str) -> String {
    let message = message.to_ascii_lowercase();
    if message.contains("pull request may have closed or changed") {
        return "stopped because the pull request closed or changed".to_string();
    }
    if message.contains("timeout") || message.contains("timed out") || message.contains("within ") {
        let seconds = message
            .split_once("within ")
            .and_then(|(_, rest)| rest.split_once('s'))
            .and_then(|(seconds, _)| seconds.trim().parse::<u64>().ok());
        return match seconds {
            Some(seconds) if seconds >= 60 => {
                format!("timed out after {} min", seconds.saturating_add(59) / 60)
            }
            Some(seconds) => format!("timed out after {seconds} sec"),
            None => "timed out".to_string(),
        };
    }
    if message.contains("budget") {
        return "ran out of budget".to_string();
    }
    if message.contains("model") || message.contains("gateway") || message.contains("provider") {
        return "could not reach a model".to_string();
    }
    "could not complete".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn proposal(summary: ReviewSummary, prior_findings: &[&str]) -> Proposal {
        serde_json::from_value(json!({
            "version": crate::app::review::PROPOSAL_VERSION,
            "repo": "acme/widget",
            "number": 1,
            "head_sha": "abcdef1234567890",
            "lanes": [],
            "summary": summary,
            "prior_findings": prior_findings,
            "cost_usd": 0.0,
            "cached_tokens": 0
        }))
        .expect("proposal")
    }

    #[test]
    fn progress_retains_only_a_completed_bot_report() {
        let prior = format!("{MARKER}\n\n**Reviewed head:** `abc`");
        assert!(in_progress("def", Some(&prior)).contains("Previous completed report"));
        assert!(
            !in_progress("def", Some("contributor text")).contains("Previous completed report")
        );
    }

    #[test]
    fn progress_does_not_recursively_retain_a_failure_wrapper() {
        let completed = format!("{MARKER}\n\n**Reviewed head:** `abc`");
        let first_progress = in_progress("def", Some(&completed));
        let failure = failed("def", "network error", Some(&first_progress));
        let second_progress = in_progress("ghi", Some(&failure));
        let second_failure = failed("ghi", "network error", Some(&second_progress));

        assert_eq!(
            second_failure.matches("**Reviewed head:** `abc`").count(),
            1
        );
        assert!(second_failure.contains("Last completed report"));
    }

    #[test]
    fn legacy_progress_wrapper_keeps_its_completed_report() {
        let completed = format!(
            "{MARKER}\n\n**Reviewed head:** `abc`\n\n<details>\n<summary>Report detail</summary>\n\nDetails.\n</details>"
        );
        let legacy_progress = format!(
            "{MARKER}\n\n**Reviewing `def` now.**\n<details>\n<summary>Previous completed report</summary>\n\n{completed}\n</details>"
        );

        let failure = failed("def", "network error", Some(&legacy_progress));
        let next_progress = in_progress("ghi", Some(&failure));

        assert_eq!(next_progress.matches("**Reviewed head:** `abc`").count(), 1);
        assert!(!next_progress.contains("**Reviewing `def`"));
        assert!(next_progress.contains("<summary>Report detail</summary>"));
    }

    #[test]
    fn legacy_failure_wrapper_extracts_a_report_with_nested_details() {
        let completed = format!(
            "{MARKER}\n\n**Reviewed head:** `abc`\n\n<details>\n<summary>Report detail</summary>\n\nDetails.\n</details>"
        );
        let legacy_failure = format!(
            "{MARKER}\n\n**Review failed for `def`.** timed out\n<details open>\n<summary>Last completed report</summary>\n\n{completed}\n</details>"
        );

        let progress = in_progress("ghi", Some(&legacy_failure));
        let failure = failed("ghi", "timeout", Some(&progress));

        assert_eq!(failure.matches("**Reviewed head:** `abc`").count(), 1);
        assert!(failure.contains("<summary>Report detail</summary>"));
        assert!(!failure.contains("**Review failed for `def`"));
    }

    #[test]
    fn failed_copy_is_safe_and_names_the_preserved_head() {
        let completed = format!("{MARKER}\n\n**Reviewed head:** `def5678`");
        let failure = failed(
            "abc1234",
            "#49 Error::Timeout: raw details",
            Some(&completed),
        );

        assert!(failure.contains("`def5678`"));
        assert!(!failure.contains("\\#"));
        assert!(!failure.contains("Error::"));
    }

    #[test]
    fn failure_wrappers_remain_bounded_across_ten_cycles() {
        let completed = format!(
            "{MARKER}\n\n**Reviewed head:** `abc`\n{}",
            "report ".repeat(1_000)
        );
        let mut body = completed.clone();

        for cycle in 0..10 {
            body = in_progress(&format!("progress-{cycle}"), Some(&body));
            body = failed(&format!("failed-{cycle}"), "timeout", Some(&body));
        }

        assert_eq!(body.matches("**Reviewed head:** `abc`").count(), 1);
        assert!(body.len() < completed.len() + 2_000);
    }

    #[test]
    fn render_escapes_model_and_contributor_controlled_markdown() {
        let proposal = proposal(
            ReviewSummary {
                executive_summary: "[click](https://evil.example) @maintainer <img src=x> # forged"
                    .into(),
                changes: "**trusted**".into(),
                ..ReviewSummary::default()
            },
            &[],
        );

        let rendered = render(&Config::default(), &proposal);

        assert!(!rendered.contains("[click](https://evil.example)"));
        assert!(!rendered.contains("@maintainer"));
        assert!(!rendered.contains("<img src=x>"));
        assert!(rendered.contains("\\[click\\](https&#58;//evil&#46;example)"));
        assert!(rendered.contains("&#64;maintainer"));
        assert!(rendered.contains("&lt;img src=x&gt;"));
    }

    #[test]
    fn run_history_escapes_table_pipes_in_state_summary_and_head() {
        let mut config = Config::default();
        config.summary.sections = vec![SummarySection::RunDetails];
        let rendered = render(
            &config,
            &proposal(
                ReviewSummary {
                    history: vec![crate::summary::types::ReviewPass {
                        head_sha: "sha|injected".into(),
                        state: "ready | **blocked**".into(),
                        summary: "done | [click](https://evil.example)".into(),
                        ..Default::default()
                    }],
                    ..ReviewSummary::default()
                },
                &[],
            ),
        );
        assert!(rendered.contains("`sha\\|injected`"), "{rendered}");
        assert!(
            rendered.contains("ready \\| \\*\\*blocked\\*\\*"),
            "{rendered}"
        );
        assert!(rendered.contains("done \\| \\[click\\]"), "{rendered}");
        assert!(!rendered.contains("| **blocked** |"), "{rendered}");
        assert!(
            !rendered.contains("| [click](https://evil.example) |"),
            "{rendered}"
        );
        let row = rendered
            .lines()
            .find(|line| line.starts_with("| `sha"))
            .expect("history row");
        assert_eq!(row.matches(" | ").count(), 2, "{row}");
    }

    #[test]
    fn run_details_disclose_incomplete_provider_completion_usage() {
        let mut config = Config::default();
        config.summary.sections = vec![SummarySection::RunDetails];
        let mut proposal = proposal(ReviewSummary::default(), &[]);
        proposal.cost_usd = 0.875;
        proposal.embed_tokens = 7;
        proposal.completion_accounting = Some(crate::app::review::CompletionAccounting {
            usage: crate::ports::model::Usage {
                input_tokens: 11,
                output_tokens: 4,
                cost_usd: 0.125,
                calls_attempted: 2,
                calls_succeeded: 2,
                ..Default::default()
            },
            usage_by_model: Default::default(),
            committed_calls: 2,
            terminal_count: 2,
            complete: false,
        });

        let rendered = render(&config, &proposal);

        assert!(rendered.contains("- **Spend:** $0.875000"), "{rendered}");
        assert!(
            rendered.contains("**Completion usage:** incomplete"),
            "{rendered}"
        );
        assert!(
            rendered.contains("some provider completion costs are unavailable"),
            "{rendered}"
        );
    }

    #[test]
    fn summary_citations_link_to_the_reviewed_head_and_keep_paths_in_code_spans() {
        let mut config = Config::default();
        config.summary.sections = vec![SummarySection::Features];
        let rendered = render(
            &config,
            &proposal(
                ReviewSummary {
                    features: vec![crate::summary::types::Feature {
                        kind: crate::summary::types::FeatureKind::Addition,
                        name: "Saved value".into(),
                        impact: "A new persisted field.".into(),
                        citations: vec!["src/savings_store.rs:42".into()],
                    }],
                    ..ReviewSummary::default()
                },
                &[],
            ),
        );

        assert!(rendered.contains(
            "[`src/savings_store.rs:42`](https://github.com/acme/widget/blob/abcdef1234567890/src/savings_store.rs#L42)"
        ));
        assert!(!rendered.contains("savings\\_store"));
    }

    #[test]
    fn findings_section_owns_carried_findings_when_tests_are_hidden() {
        let mut config = Config::default();
        config.summary.sections = vec![SummarySection::Findings];
        let rendered = render(
            &config,
            &proposal(ReviewSummary::default(), &["Earlier active finding"]),
        );

        assert!(rendered.contains("## Findings"));
        assert!(rendered.contains("Previously reported and still active"));
        assert!(rendered.contains("Earlier active finding"));
        assert!(!rendered.contains("No active actionable findings"));
        assert!(!rendered.contains("## Tests"));
    }

    #[test]
    fn carried_findings_are_merge_work_even_without_a_repeat() {
        let mut config = Config::default();
        config.summary.sections = vec![SummarySection::BeforeMerge];
        let rendered = render(
            &config,
            &proposal(ReviewSummary::default(), &["Earlier active finding"]),
        );

        assert!(rendered.contains("Address carried finding"));
        assert!(!rendered.contains("## Before merge\n\nNone."));
    }

    #[test]
    fn the_update_time_renders_as_iso_8601_utc_never_raw_epoch_seconds() {
        let mut config = Config::default();
        config.summary.sections = vec![SummarySection::RunDetails];
        let rendered = render(
            &config,
            &proposal(
                ReviewSummary {
                    updated_at_epoch: 1_791_459_570,
                    history: vec![crate::summary::types::ReviewPass {
                        head_sha: "abcdef1234567890".into(),
                        reviewed_at_epoch: 951_782_400,
                        ..Default::default()
                    }],
                    ..ReviewSummary::default()
                },
                &[],
            ),
        );

        assert!(
            rendered.contains("**Updated:** 2026-10-08T11:39:30Z"),
            "{rendered}"
        );
        assert!(rendered.contains("2000-02-29T00:00:00Z"), "{rendered}");
        assert!(!rendered.contains("1791459570"), "{rendered}");
        assert!(!rendered.contains("951782400"), "{rendered}");
        assert!(!rendered.contains("Unix time"), "{rendered}");
    }

    #[test]
    fn an_unknown_update_time_is_omitted_rather_than_dated_1970() {
        let rendered = render(&Config::default(), &proposal(ReviewSummary::default(), &[]));
        assert!(!rendered.contains("**Updated:**"), "{rendered}");
        assert!(!rendered.contains("1970"), "{rendered}");
    }

    #[test]
    fn what_changed_is_omitted_when_no_summary_could_be_produced() {
        let mut config = Config::default();
        config.summary.sections = vec![SummarySection::Changes];
        for failed in [
            "",
            "No supported behavioral explanation was produced.",
            "The review could not produce a supported behavioral summary; inspect the cited \
             changed surface and lane details below.",
        ] {
            let rendered = render(
                &config,
                &proposal(
                    ReviewSummary {
                        changes: failed.into(),
                        ..ReviewSummary::default()
                    },
                    &[],
                ),
            );
            assert!(!rendered.contains("What changed"), "{rendered}");
            assert!(!rendered.contains("could not produce"), "{rendered}");
            assert!(!rendered.contains("No supported behavioral"), "{rendered}");
        }

        let rendered = render(
            &config,
            &proposal(
                ReviewSummary {
                    changes: "The hub is edited in place.".into(),
                    ..ReviewSummary::default()
                },
                &[],
            ),
        );
        assert!(
            rendered.contains("## What changed\n\nThe hub is edited in place"),
            "{rendered}"
        );
    }

    #[test]
    fn a_grouped_observation_is_still_listed_in_the_hub_findings() {
        // A co-located observation is no longer nested into the primary inline
        // comment, so the hub's findings list is where the author reads it.
        let finding = |title: &str, grouped: bool| {
            json!({
                "lane": "security",
                "severity": "high",
                "confidence": 0.9,
                "path": "src/hub.rs",
                "line": 2,
                "end_line": null,
                "rule": "rule",
                "title": title,
                "body": "Body text.",
                "suggestion": null,
                "applicable": null,
                "late": false,
                "identity": null,
                "aliases": [],
                "grouped": grouped,
                "review_pass": 1,
                "corroboration": 1,
            })
        };
        let proposal: Proposal = serde_json::from_value(json!({
            "version": crate::app::review::PROPOSAL_VERSION,
            "repo": "acme/widget",
            "number": 1,
            "head_sha": "abcdef1234567890",
            "lanes": [{
                "lane": "security",
                "check_name": "tinysweeper/security",
                "conclusion": "neutral",
                "summary": "Reviewed.",
                "findings": [finding("Primary finding", false), finding("Grouped observation", true)],
            }],
            "summary": ReviewSummary::default(),
            "prior_findings": [],
            "cost_usd": 0.0,
            "cached_tokens": 0
        }))
        .expect("proposal");

        let rendered = render(&Config::default(), &proposal);
        assert!(rendered.contains("Primary finding"), "{rendered}");
        assert!(rendered.contains("Grouped observation"), "{rendered}");
    }

    #[test]
    fn a_supported_summary_that_only_begins_like_a_legacy_one_is_kept() {
        let mut config = Config::default();
        config.summary.sections = vec![SummarySection::Changes];
        let rendered = render(
            &config,
            &proposal(
                ReviewSummary {
                    changes: "No supported behavioral explanation was produced. The \
                              implementation nevertheless adds a retry to the sync loop."
                        .into(),
                    ..ReviewSummary::default()
                },
                &[],
            ),
        );
        assert!(rendered.contains("## What changed"), "{rendered}");
        assert!(
            rendered.contains("adds a retry to the sync loop"),
            "{rendered}"
        );
    }

    #[test]
    fn an_empty_review_has_a_compact_summary_and_collapsed_supporting_sections() {
        let rendered = render(
            &Config::default(),
            &proposal(
                ReviewSummary {
                    executive_summary: "The change adds a retry to the sync loop.".into(),
                    surface: crate::summary::types::ChangeSurface {
                        production: 4,
                        tests: 2,
                        documentation: 1,
                        configuration: 0,
                    },
                    file_coverage: Some(crate::summary::types::FileReviewCoverage {
                        reviewed: 7,
                        total: 7,
                        unreviewed: vec![],
                    }),
                    ..ReviewSummary::default()
                },
                &[],
            ),
        );
        let visible = rendered.split("<details").next().expect("public summary");

        assert!(visible.contains("**Verdict:**"), "{visible}");
        assert!(
            visible.contains("**Coverage:** Reviewed 7 of 7 files."),
            "{visible}"
        );
        assert!(
            visible.contains("The change adds a retry to the sync loop."),
            "{visible}"
        );
        assert!(rendered.contains("<details>"), "{rendered}");
        assert!(
            visible.len() <= 800,
            "{} chars outside details",
            visible.len()
        );
        assert!(!visible.contains("## Tests"), "{visible}");
        assert!(!visible.contains("## Review snapshot"), "{visible}");
    }

    #[test]
    fn the_public_summary_stays_bounded_for_long_prose_and_many_unreviewed_paths() {
        let long_paths: Vec<String> = (0..24)
            .map(|index| format!("src/{index}/{}.rs", "deeply_nested_module".repeat(5)))
            .collect();
        let proposal = proposal(
            ReviewSummary {
                executive_summary: format!("{}.", "A long supported summary".repeat(60)),
                ..ReviewSummary::default()
            },
            &[],
        );
        let mut proposal = proposal;
        proposal.unreviewed = long_paths;
        let rendered = render(&Config::default(), &proposal);
        let visible = rendered.split("<details").next().expect("public summary");

        assert!(
            visible.len() <= 800,
            "{} chars outside details",
            visible.len()
        );
        assert!(
            rendered.contains("additional unreviewed files"),
            "{rendered}"
        );
        assert!(rendered.contains("src/23/"), "{rendered}");
    }

    #[test]
    fn run_diagnostics_stay_inside_the_collapsed_block() {
        let proposal: Proposal = serde_json::from_value(json!({
            "version": crate::app::review::PROPOSAL_VERSION,
            "repo": "acme/widget", "number": 1, "head_sha": "abcdef1234567890",
            "lanes": [{
                "lane": "critique", "check_name": "tinysweeper/critique",
                "conclusion": "neutral", "summary": "The code index is behind.",
                "findings": []
            }],
            "summary": {
                "executive_summary": "The change updates the request handler. The index is behind.",
                "cache_chain_restarted": true,
                "tests": [{
                    "kind": "regression", "behavior": "request handler",
                    "assessment": "not executed per evidence", "citations": []
                }]
            },
            "models": ["ladder/reviewer"],
            "prior_findings": [], "cost_usd": 0.0, "cached_tokens": 0
        }))
        .expect("proposal");
        let rendered = render(&Config::default(), &proposal);
        let visible = rendered.split("<details").next().expect("public summary");

        for term in ["cache chain", "index is behind", "ladder/", "per evidence"] {
            assert!(
                !visible.contains(term),
                "{term} leaked outside details: {visible}"
            );
        }
        assert!(rendered.contains("cache chain restarted"), "{rendered}");
        assert!(rendered.contains("index is behind"), "{rendered}");
        assert!(rendered.contains("ladder/reviewer"), "{rendered}");
    }

    #[test]
    fn lane_gaps_are_not_rendered_as_unreviewed_file_paths() {
        let config = Config::default();
        let proposal: Proposal = serde_json::from_value(json!({
            "version": crate::app::review::PROPOSAL_VERSION,
            "repo": "acme/widget", "number": 1, "head_sha": "abcdef1234567890",
            "lanes": [{
                "lane": "tests", "check_name": "tinysweeper/tests",
                "conclusion": "neutral", "summary": "No answer.",
                "findings": [],
                "unanswered": ["tinysweeper/tests", "src/tested.rs"]
            }],
            "unreviewed": ["src/unread.rs"],
            "summary": ReviewSummary::default(), "prior_findings": [],
            "cost_usd": 0.0, "cached_tokens": 0
        }))
        .expect("proposal");

        let rendered = render(&config, &proposal);
        assert!(
            rendered.contains("not reviewed: `src/unread.rs`"),
            "{rendered}"
        );
        assert!(
            rendered.contains("tests reviewer did not respond"),
            "{rendered}"
        );
        assert!(rendered.contains("`src/tested.rs`"), "{rendered}");
        assert!(!rendered.contains("tinysweeper/"), "{rendered}");
    }

    #[test]
    fn partial_large_coverage_keeps_each_host_reason_and_escapes_paths() {
        use crate::app::review::{PartialReview, PartialReviewPath, PartialReviewReason};
        use crate::config::types::LaneId;
        use crate::lanes::large_pr::{ReviewEstimate, ReviewMode};

        let mut proposal = proposal(ReviewSummary::default(), &[]);
        proposal.unreviewed = vec![
            "src/a`quoted.rs".into(),
            "src/b.rs".into(),
            "src/c.rs".into(),
        ];
        proposal.partial_review = Some(PartialReview {
            mode: ReviewMode::Large,
            stop_reason: Some(PartialReviewReason::DrainDeadline),
            estimate: ReviewEstimate {
                files: 3,
                changed_lines: 3,
                groups: 4,
            },
            selected_groups: vec![],
            omitted: vec![
                PartialReviewPath {
                    path: "src/a`quoted.rs".into(),
                    lane: Some(LaneId::Critique),
                    reason: PartialReviewReason::NotSelected,
                },
                PartialReviewPath {
                    path: "src/b.rs".into(),
                    lane: Some(LaneId::Critique),
                    reason: PartialReviewReason::CallLimit,
                },
                PartialReviewPath {
                    path: "src/c.rs".into(),
                    lane: Some(LaneId::Critique),
                    reason: PartialReviewReason::NotSelected,
                },
                PartialReviewPath {
                    path: "src/c.rs".into(),
                    lane: Some(LaneId::Security),
                    reason: PartialReviewReason::DrainDeadline,
                },
                PartialReviewPath {
                    path: "src/c.rs".into(),
                    lane: Some(LaneId::Security),
                    reason: PartialReviewReason::SoftDeadline,
                },
            ],
        });

        let rendered = render(&Config::default(), &proposal);
        let visible = rendered.split("<details").next().expect("public summary");
        assert!(visible.contains("not reviewed (large PR):"), "{visible}");
        assert!(visible.contains("not reviewed (call budget):"), "{visible}");
        assert!(visible.contains("not reviewed (time limit):"), "{visible}");
        assert!(visible.contains(&code_span("src/a`quoted.rs")), "{visible}");
        assert!(rendered.contains("src/c.rs"), "{rendered}");
        assert!(
            rendered.contains("not selected by the large-review budget")
                && rendered.contains("not reviewed (time limit)"),
            "{rendered}"
        );

        proposal
            .partial_review
            .as_mut()
            .expect("partial metadata")
            .omitted
            .clear();
        proposal.unreviewed.clear();
        assert!(
            proposal.complete(),
            "Large metadata alone is not a coverage gap"
        );
    }

    #[test]
    fn findings_lead_with_a_location_problem_and_suggested_fix() {
        let mut config = Config::default();
        config.summary.sections = vec![SummarySection::Findings];
        let proposal: Proposal = serde_json::from_value(json!({
            "version": crate::app::review::PROPOSAL_VERSION,
            "repo": "acme/widget", "number": 1, "head_sha": "abcdef1234567890",
            "lanes": [{
                "lane": "security", "check_name": "tinysweeper/security",
                "conclusion": "failure", "summary": "Finding.",
                "findings": [{
                    "lane": "security", "severity": "high", "confidence": 0.99,
                    "path": "src/hub.rs", "line": 42, "rule": "unsafe-default",
                    "title": "Reject empty token", "body": "An empty token bypasses validation.",
                    "suggestion": "if token.is_empty() { return Err(Error::MissingToken); }\n</details>\n```",
                    "late": false, "aliases": [], "grouped": false, "review_pass": 1,
                    "corroboration": 1
                }]
            }],
            "summary": ReviewSummary::default(), "prior_findings": [],
            "cost_usd": 0.0, "cached_tokens": 0
        }))
        .expect("proposal");

        let rendered = render(&config, &proposal);
        let section = rendered
            .split("## Findings")
            .nth(1)
            .expect("findings section");
        assert!(section.contains("[`src/hub.rs:42`](https://github.com/acme/widget/blob/abcdef1234567890/src/hub.rs#L42)"), "{section}");
        assert!(
            section.contains("An empty token bypasses validation."),
            "{section}"
        );
        assert!(section.contains("if token.is_empty()"), "{section}");
        let lines: Vec<_> = section.lines().collect();
        let label = lines
            .iter()
            .position(|line| line.trim() == "**Suggested fix:**")
            .expect("suggested-fix label");
        assert_eq!(lines[label + 1].trim(), "````", "{section}");
        assert_eq!(
            lines[label + 2].trim(),
            "if token.is_empty() { return Err(Error::MissingToken); }",
            "{section}"
        );
        assert_eq!(lines[label + 3].trim(), "</details>", "{section}");
        assert_eq!(lines[label + 4].trim(), "```", "{section}");
        assert_eq!(lines[label + 5].trim(), "````", "{section}");
    }
}
