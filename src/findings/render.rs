//! Rendering findings for a human reading a check run.
//!
//! A check-run summary is the only place most people will ever meet this tool,
//! and a wall of undifferentiated prose is a wall nobody reads. Severity is
//! carried by a coloured badge so it survives skimming, findings are grouped so
//! the worst are met first, and the detail sits in a `<details>` block that
//! costs nothing to leave folded.

use crate::config::types::Severity;
use crate::findings::types::Finding;
use crate::ports::model::Usage;

/// Colour and label for each severity, as a shields.io badge.
///
/// Static images rather than emoji: they render identically on every client,
/// they carry the word as well as the colour — which matters for anyone who
/// cannot distinguish red from amber — and they line up in a table.
pub fn badge(severity: Severity) -> String {
    let (label, colour) = severity_parts(severity);
    format!("![{label}](https://img.shields.io/badge/{label}-{colour}?style=flat-square)")
}

/// The severity badge, labelled, for the head of an inline comment.
///
/// `label=priority` puts the word "priority" in the grey half and the level in
/// the coloured half, so the badge reads as a sentence rather than a bare word
/// whose meaning depends on knowing our colour scheme. The unlabelled
/// [`badge`] stays for the summary table, where a column heading already says
/// what the colour means and repeating it on every row is noise.
pub fn priority_badge(severity: Severity) -> String {
    let (label, colour) = severity_parts(severity);
    format!(
        "![priority {label}](https://img.shields.io/badge/{label}-{colour}?label=priority)"
    )
}

/// The lane and how sure it is, as one badge.
///
/// Two facts that are only useful together: "tests" alone does not say whether
/// to act, and "likely" alone does not say who is talking. Pairing them costs
/// one badge instead of two and reads as `tests | likely`.
pub fn lane_confidence_badge(lane: LaneId, confidence: f64) -> String {
    let (word, colour) = confidence_parts(confidence);
    format!("![{lane} {word}](https://img.shields.io/badge/{lane}-{word}-{colour})")
}

/// Colour and word for a severity.
fn severity_parts(severity: Severity) -> (&'static str, &'static str) {
    match severity {
        Severity::Critical => ("critical", "b60205"),
        Severity::High => ("high", "d93f0b"),
        Severity::Medium => ("medium", "fbca04"),
        Severity::Low => ("low", "0e8a16"),
    }
}

/// Colour and word for a confidence, bucketed.
fn confidence_parts(confidence: f64) -> (&'static str, &'static str) {
    match confidence {
        c if c >= 0.9 => ("confident", "1f6feb"),
        c if c >= 0.7 => ("likely", "6f42c1"),
        _ => ("uncertain", "8b949e"),
    }
}

/// A confidence badge, bucketed rather than exact.
///
/// A model's 0.83 is not meaningfully different from its 0.79, and printing two
/// decimal places implies a precision that is not there.
pub fn confidence_badge(confidence: f64) -> String {
    let (label, colour) = confidence_parts(confidence);
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

    out.push('\n');
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
pub fn escape_cell(text: &str) -> String {
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
pub fn cost_line(usage: &Usage, models: &[String]) -> String {
    let mut line = format!(
        "${:.4} · {} in / {} out",
        usage.cost_usd,
        thousands(usage.input_tokens),
        thousands(usage.output_tokens),
    );

    if usage.input_tokens > 0 {
        let rate = usage.cached_tokens as f64 / usage.input_tokens as f64 * 100.0;
        line.push_str(&format!(
            " · {} cached ({rate:.0}%)",
            thousands(usage.cached_tokens)
        ));
    }

    // Only when there is any, so a review that never touched the index reads
    // exactly as it did before retrieval existed.
    if usage.embed_tokens > 0 {
        line.push_str(&format!(" · {} embedded", thousands(usage.embed_tokens)));
    }

    if !models.is_empty() {
        line.push_str(&format!(" · {}", models.join(", ")));
    }

    line
}

/// Render the per-lane breakdown of a run's spend.
///
/// A single total says a review was expensive; this says which lane made it so,
/// which is the only version of the number anyone can act on. Lanes that spent
/// nothing — skipped, or deterministic like the scanners and the gate — are
/// left out rather than listed as `$0.0000` noise.
pub fn per_lane_costs(lanes: &[(String, Usage, Vec<String>)]) -> String {
    let mut out = String::new();
    for (name, usage, models) in lanes {
        if usage.cost_usd <= 0.0 && usage.input_tokens == 0 && usage.embed_tokens == 0 {
            continue;
        }
        out.push_str(&format!("\n{name}: {}", cost_line(usage, models)));
    }
    out
}

/// The spend, as a column-aligned block in a fenced code span.
///
/// Prose wraps and reflows, so the same six numbers land in a different place
/// on every row and comparing two lanes means reading rather than glancing.
/// Fixed-width columns make the expensive lane the one your eye stops on, which
/// is the only reason anyone opens this section.
///
/// The total is the first row and is deliberately unlabelled: it is the sum of
/// the rows beneath it, and a `total:` label would compete with the lane names
/// for attention when the interesting number is almost always a lane.
///
/// Fenced rather than a markdown table because a table's pipes are re-laid-out
/// by the renderer and the alignment is lost — the thing this exists for.
pub fn cost_table(
    total: &Usage,
    total_models: &[String],
    lanes: &[(String, Usage, Vec<String>)],
) -> String {
    let mut rows: Vec<[String; 6]> = vec![row("", total, total_models)];

    for (name, usage, models) in lanes {
        // Same rule as `per_lane_costs`: a lane that spent nothing is noise.
        if usage.cost_usd <= 0.0 && usage.input_tokens == 0 && usage.embed_tokens == 0 {
            continue;
        }
        rows.push(row(&format!("{name}:"), usage, models));
    }

    // Nothing but a total means there is no breakdown to align, and a one-row
    // table reads worse than the sentence it replaced.
    if rows.len() < 2 {
        return format!("```\n{}\n```", cost_line(total, total_models).trim());
    }

    // The last column is never padded: trailing spaces are invisible and only
    // make the block wider than the terminal it is read in.
    let widths: [usize; 5] = std::array::from_fn(|column| {
        rows.iter()
            .map(|cells| cells[column].chars().count())
            .max()
            .unwrap_or(0)
    });

    let mut out = String::from("```\n");
    for cells in &rows {
        let mut line = String::new();
        for (column, cell) in cells.iter().enumerate().take(5) {
            let pad = widths[column].saturating_sub(cell.chars().count());
            line.push_str(cell);
            line.push_str(&" ".repeat(pad));
            // The input and output counts are one fact, so a slash joins them
            // and the interpunct separates the groups.
            line.push_str(match column {
                0 => " ",
                2 => " / ",
                _ => " \u{b7} ",
            });
        }
        line.push_str(&cells[5]);
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out.push_str("```");
    out
}

/// One row's cells: label, cost, input, output, cache, models.
fn row(label: &str, usage: &Usage, models: &[String]) -> [String; 6] {
    let cached = if usage.input_tokens > 0 {
        let rate = usage.cached_tokens as f64 / usage.input_tokens as f64 * 100.0;
        format!("{} cached ({rate:.0}%)", thousands(usage.cached_tokens))
    } else {
        // Not "0 cached (0%)": nothing was sent, so there was nothing to hit the
        // cache, and a zero here reads as a cache that missed.
        String::new()
    };

    let mut models = models.join(", ");
    if usage.embed_tokens > 0 {
        // Embedding spend rides in the model column rather than one of its own,
        // which would be empty on every row of a review that never indexed.
        let embedded = format!("{} embedded", thousands(usage.embed_tokens));
        models = if models.is_empty() {
            embedded
        } else {
            format!("{models} \u{b7} {embedded}")
        };
    }

    [
        label.to_string(),
        format!("${:.4}", usage.cost_usd),
        format!("{} in", thousands(usage.input_tokens)),
        format!("{} out", thousands(usage.output_tokens)),
        cached,
        models,
    ]
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
            identity: None,
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

    fn usage(cost_usd: f64, input: u64, output: u64, cached: u64) -> Usage {
        Usage {
            cost_usd,
            input_tokens: input,
            output_tokens: output,
            cached_tokens: cached,
            embed_tokens: 0,
        }
    }

    #[test]
    fn the_cost_line_reports_the_whole_token_breakdown() {
        let line = cost_line(
            &usage(0.1615, 49_169, 1_204, 40_000),
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
        let line = cost_line(&Usage::default(), &[]);
        assert!(!line.contains('%'), "{line}");
    }

    #[test]
    fn embedding_tokens_are_reported_apart_from_prompt_tokens() {
        let line = cost_line(
            &Usage {
                embed_tokens: 12_000,
                ..usage(0.05, 1_000, 10, 0)
            },
            &[],
        );
        assert!(line.contains("1,000 in"), "{line}");
        assert!(line.contains("12,000 embedded"), "{line}");
    }

    #[test]
    fn the_per_lane_breakdown_omits_lanes_that_spent_nothing() {
        let rendered = per_lane_costs(&[
            (
                "critique".into(),
                usage(0.30, 40_000, 900, 0),
                vec!["vendor/deep".into()],
            ),
            ("gate".into(), Usage::default(), vec![]),
        ]);
        assert!(rendered.contains("critique: $0.3000"), "{rendered}");
        assert!(!rendered.contains("gate"), "{rendered}");
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

#[cfg(test)]
mod cost_table_tests {
    use super::*;

    fn usage(cost: f64, input: u64, output: u64, cached: u64) -> Usage {
        Usage {
            cost_usd: cost,
            input_tokens: input,
            output_tokens: output,
            cached_tokens: cached,
            ..Default::default()
        }
    }

    fn sample() -> String {
        cost_table(
            &usage(0.2672, 139_215, 6_896, 13_824),
            &["minimax/minimax-m3".into(), "moonshotai/kimi-k3".into()],
            &[
                (
                    "critique".into(),
                    usage(0.0618, 19_733, 173, 0),
                    vec!["moonshotai/kimi-k3".into()],
                ),
                (
                    "security".into(),
                    usage(0.1827, 59_392, 2_375, 11_520),
                    vec!["moonshotai/kimi-k3".into()],
                ),
                (
                    "commits".into(),
                    usage(0.0010, 2_866, 132, 128),
                    vec!["minimax/minimax-m3".into()],
                ),
            ],
        )
    }

    #[test]
    fn every_column_lines_up() {
        // The whole point: a reader should be able to run an eye down the cost
        // column. If the separators drift, they cannot.
        let table = sample();
        let body: Vec<&str> = table
            .lines()
            .filter(|l| !l.starts_with("```") && !l.is_empty())
            .collect();

        let first = body[0].find('$').expect("a cost");
        for line in &body {
            assert_eq!(line.find('$'), Some(first), "{table}");
        }

        // And the separator after the cost column, which is what actually
        // proves the padding is per-column rather than accidental.
        let sep = body[0].find(" · ").expect("a separator");
        for line in &body {
            assert_eq!(line.find(" · "), Some(sep), "{table}");
        }
    }

    #[test]
    fn the_total_is_first_and_unlabelled() {
        let table = sample();
        let body: Vec<&str> = table.lines().filter(|l| !l.starts_with("```")).collect();
        assert!(body[0].trim_start().starts_with('$'), "{table}");
        assert!(body[1].starts_with("critique:"), "{table}");
    }

    #[test]
    fn it_is_fenced_so_the_renderer_cannot_reflow_it() {
        let table = sample();
        assert!(table.starts_with("```\n"), "{table}");
        assert!(table.ends_with("```"), "{table}");
    }

    #[test]
    fn no_row_carries_trailing_whitespace() {
        // Padding the last column would widen the block for nothing and show up
        // as ragged whitespace in a diff.
        for line in sample().lines() {
            assert_eq!(line, line.trim_end(), "{line:?}");
        }
    }

    #[test]
    fn a_lane_that_spent_nothing_is_left_out() {
        let table = cost_table(
            &usage(0.0618, 19_733, 173, 0),
            &["moonshotai/kimi-k3".into()],
            &[
                (
                    "critique".into(),
                    usage(0.0618, 19_733, 173, 0),
                    vec!["moonshotai/kimi-k3".into()],
                ),
                ("gate".into(), usage(0.0, 0, 0, 0), vec![]),
            ],
        );
        assert!(!table.contains("gate"), "{table}");
    }

    #[test]
    fn a_run_that_sent_nothing_reports_no_cache_rate() {
        // 0% would read as "the cache missed" rather than "nothing was sent".
        let table = cost_table(&usage(0.0, 0, 0, 0), &[], &[]);
        assert!(!table.contains('%'), "{table}");
    }

    #[test]
    fn a_single_lane_falls_back_to_one_line() {
        // A table of one row is worse than the sentence it replaced.
        let single = usage(0.01, 100, 10, 0);
        let table = cost_table(&single, &["m".into()], &[]);
        assert_eq!(table.lines().count(), 3, "{table}");
    }

    #[test]
    fn embedding_spend_appears_without_a_column_of_its_own() {
        let mut with_embed = usage(0.02, 100, 10, 0);
        with_embed.embed_tokens = 377_153;
        let table = cost_table(
            &with_embed,
            &["m".into()],
            &[
                ("critique".into(), usage(0.01, 50, 5, 0), vec!["m".into()]),
                ("security".into(), with_embed, vec!["m".into()]),
            ],
        );
        assert!(table.contains("377,153 embedded"), "{table}");
    }
}
