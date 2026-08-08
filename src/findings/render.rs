//! Rendering findings for a human reading a check run.
//!
//! A check-run summary is the only place most people will ever meet this tool,
//! and a wall of undifferentiated prose is a wall nobody reads. Severity is
//! carried by a coloured badge so it survives skimming, findings are grouped so
//! the worst are met first, and the detail sits in a `<details>` block that
//! costs nothing to leave folded.

use crate::config::types::Severity;
use crate::findings::types::Finding;

/// Colour and label for each severity, as a shields.io badge.
///
/// Static images rather than emoji: they render identically on every client,
/// they carry the word as well as the colour — which matters for anyone who
/// cannot distinguish red from amber — and they line up in a table.
pub fn badge(severity: Severity) -> String {
    let (label, colour) = match severity {
        Severity::Critical => ("critical", "b60205"),
        Severity::High => ("high", "d93f0b"),
        Severity::Medium => ("medium", "fbca04"),
        Severity::Low => ("low", "0e8a16"),
    };
    format!("![{label}](https://img.shields.io/badge/{label}-{colour}?style=flat-square)")
}

/// A confidence badge, bucketed rather than exact.
///
/// A model's 0.83 is not meaningfully different from its 0.79, and printing two
/// decimal places implies a precision that is not there.
pub fn confidence_badge(confidence: f64) -> String {
    let (label, colour) = match confidence {
        c if c >= 0.9 => ("confident", "1f6feb"),
        c if c >= 0.7 => ("likely", "6f42c1"),
        _ => ("uncertain", "8b949e"),
    };
    format!("![{label}](https://img.shields.io/badge/{label}-{colour}?style=flat-square)")
}

/// Render one lane's findings as a check-run summary.
pub fn lane_summary(summary: &str, findings: &[Finding], version: &str) -> String {
    let mut out = String::with_capacity(1024);
    out.push_str(summary.trim());
    out.push_str("\n\n");

    if findings.is_empty() {
        out.push_str("No findings.\n");
        out.push_str(&footer(version));
        return out;
    }

    // Sorted worst-first so the table reads as a priority list rather than as
    // whatever order the model happened to emit.
    let mut sorted: Vec<&Finding> = findings.iter().collect();
    sorted.sort_by(|a, b| {
        b.severity
            .cmp(&a.severity)
            .then(b.confidence.total_cmp(&a.confidence))
    });

    out.push_str("| | Finding | Where |\n|---|---|---|\n");
    for finding in &sorted {
        out.push_str(&format!(
            "| {} | {} | `{}`{} |\n",
            badge(finding.severity),
            escape_cell(&finding.title),
            finding.path,
            finding.line.map(|l| format!(":{l}")).unwrap_or_default()
        ));
    }

    out.push_str("\n");
    for finding in &sorted {
        out.push_str(&detail(finding));
    }

    out.push_str(&footer(version));
    out
}

/// One finding, folded away.
fn detail(finding: &Finding) -> String {
    let location = match finding.line {
        Some(line) => format!("`{}:{line}`", finding.path),
        None => format!("`{}`", finding.path),
    };

    let mut out = format!(
        "<details>\n<summary>{} {}</summary>\n\n{} {} {} · rule <code>{}</code>\n\n{}\n",
        badge(finding.severity),
        escape_html(&finding.title),
        location,
        confidence_badge(finding.confidence),
        if finding.late {
            "· ![pre-existing](https://img.shields.io/badge/pre--existing-8b949e?style=flat-square)"
        } else {
            ""
        },
        finding.rule,
        finding.body.trim(),
    );

    if let Some(suggestion) = &finding.suggestion {
        out.push_str(&format!(
            "\n**Suggested change**\n\n```\n{}\n```\n",
            suggestion.trim()
        ));
    }

    out.push_str("\n</details>\n\n");
    out
}

fn footer(version: &str) -> String {
    format!("\n<sub>tinysweeper {version}</sub>\n")
}

/// Make a model-authored title safe inside a table cell.
///
/// Two hazards, and both are needed: a bare `|` ends the cell, and GitHub
/// renders inline HTML in markdown tables, so a stray tag escapes into the
/// page. Escaping only pipes here was a bug a test caught — the disclosure
/// below was escaped and the table above it was not.
fn escape_cell(text: &str) -> String {
    escape_html(text).replace('|', "\\|")
}

/// A `<summary>` renders as HTML, so a stray tag in a title would break out of
/// the disclosure and mangle the rest of the page.
fn escape_html(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Render the cost line shown under every review.
///
/// Cache hit rate is here rather than buried in a log because it is the number
/// that decides whether re-reviewing a pull request is cheap or ruinous, and it
/// is the one figure a person tuning this will actually want.
pub fn cost_line(
    cost_usd: f64,
    input_tokens: u64,
    output_tokens: u64,
    cached_tokens: u64,
    models: &[String],
) -> String {
    let mut line = format!(
        "${cost_usd:.4} · {} in / {} out",
        thousands(input_tokens),
        thousands(output_tokens),
    );

    if input_tokens > 0 {
        let rate = cached_tokens as f64 / input_tokens as f64 * 100.0;
        line.push_str(&format!(
            " · {} cached ({rate:.0}%)",
            thousands(cached_tokens)
        ));
    }

    if !models.is_empty() {
        line.push_str(&format!(" · {}", models.join(", ")));
    }

    line
}

/// `12345` reads as `12,345`.
fn thousands(value: u64) -> String {
    let digits = value.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, ch) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::types::LaneId;

    fn finding(severity: Severity, title: &str) -> Finding {
        Finding {
            lane: LaneId::Critique,
            severity,
            confidence: 0.9,
            path: "src/main.rs".into(),
            line: Some(42),
            end_line: None,
            rule: "unchecked-index".into(),
            title: title.into(),
            body: "`i` is never bounds-checked.".into(),
            suggestion: None,
            late: false,
        }
    }

    #[test]
    fn every_severity_has_a_distinct_badge() {
        let badges: Vec<String> = Severity::ALL.into_iter().map(badge).collect();
        for severity in Severity::ALL {
            assert!(badge(severity).contains(severity.as_str()));
        }
        let unique: std::collections::BTreeSet<&String> = badges.iter().collect();
        assert_eq!(unique.len(), badges.len(), "two severities share a badge");
    }

    #[test]
    fn a_badge_carries_the_word_not_only_the_colour() {
        // Colour alone excludes anyone who cannot distinguish red from amber.
        assert!(badge(Severity::Critical).contains("critical"));
        assert!(badge(Severity::Low).contains("low"));
    }

    #[test]
    fn confidence_is_bucketed_rather_than_printed_to_two_decimals() {
        // 0.83 and 0.79 are not meaningfully different, and printing them
        // implies a precision the model does not have.
        assert_eq!(confidence_badge(0.95), confidence_badge(0.91));
        assert_ne!(confidence_badge(0.95), confidence_badge(0.5));
    }

    #[test]
    fn an_empty_review_says_so_and_stops() {
        let out = lane_summary("Looks sound.", &[], "0.1.0");
        assert!(out.contains("No findings."));
        assert!(!out.contains("<details>"), "nothing to fold away");
    }

    #[test]
    fn findings_are_listed_worst_first() {
        let out = lane_summary(
            "Two problems.",
            &[
                finding(Severity::Low, "Rename this"),
                finding(Severity::Critical, "Remove the key"),
            ],
            "0.1.0",
        );
        let critical = out.find("Remove the key").expect("present");
        let low = out.find("Rename this").expect("present");
        assert!(critical < low, "the table must read as a priority list");
    }

    #[test]
    fn a_pipe_in_a_title_cannot_break_the_table() {
        // Titles are model output, and a bare `|` ends the cell.
        let out = lane_summary("…", &[finding(Severity::High, "Handle a | b")], "0.1.0");
        let row = out.lines().find(|l| l.contains("Handle a")).expect("row");
        assert!(row.contains("\\|"), "{row}");
    }

    #[test]
    fn a_tag_in_a_title_cannot_escape_the_disclosure() {
        let out = lane_summary(
            "…",
            &[finding(Severity::High, "Fix <script>alert(1)</script>")],
            "0.1.0",
        );
        assert!(!out.contains("<script>"), "{out}");
        assert!(out.contains("&lt;script&gt;"));
    }

    #[test]
    fn a_suggestion_is_rendered_as_a_block() {
        let mut f = finding(Severity::High, "Guard the index");
        f.suggestion = Some("if let Some(x) = items.get(i) {".into());
        let out = lane_summary("…", &[f], "0.1.0");
        assert!(out.contains("Suggested change"));
        assert!(out.contains("items.get(i)"));
    }

    #[test]
    fn the_cost_line_reports_the_whole_token_breakdown() {
        let line = cost_line(
            0.1615,
            49_169,
            1_204,
            40_000,
            &["moonshotai/kimi-k3".into()],
        );
        assert!(line.contains("$0.1615"), "{line}");
        assert!(line.contains("49,169 in"), "{line}");
        assert!(line.contains("1,204 out"), "{line}");
        assert!(line.contains("40,000 cached (81%)"), "{line}");
        assert!(line.contains("kimi-k3"), "{line}");
    }

    #[test]
    fn a_run_that_sent_nothing_reports_no_cache_rate() {
        // 0% would read as "the cache missed" rather than "nothing was sent".
        let line = cost_line(0.0, 0, 0, 0, &[]);
        assert!(!line.contains('%'), "{line}");
    }

    #[test]
    fn thousands_separators_land_in_the_right_places() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1_000), "1,000");
        assert_eq!(thousands(49_169), "49,169");
        assert_eq!(thousands(1_234_567), "1,234,567");
    }
}
