//! Rendering a scorecard, and comparing two.
//!
//! Two properties, both about being diffable. Cases are sorted by id and every
//! float is rounded at render time, so re-running an unchanged corpus produces
//! a byte-identical report and `git diff` shows only what actually moved.
//!
//! # There is no single number
//!
//! A composite score can be improved by trading recall for precision, or either
//! for cost, and a reader cannot tell which happened. So the gate is a
//! conjunction and each term is on its own line. `eval report --baseline` says
//! PASS only when recall did not fall, forbidden hits did not rise, and nothing
//! went over budget.

use std::fmt::Write as _;

use crate::eval::types::{CaseScore, Scorecard};

/// How much a rate may fall before it counts as a regression.
///
/// Run-to-run variance is real — the provider routes where it likes and no
/// `seed` is honoured — so a gate with no tolerance fails on noise and teaches
/// people to re-run CI rather than to read it.
pub const EPSILON: f64 = 0.02;

/// The verdict of comparing a run against a baseline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Comparison {
    /// Nothing regressed.
    Pass,
    /// One or more terms of the conjunction failed.
    Fail(Vec<String>),
    /// The two runs are not comparable.
    Incomparable(String),
}

/// Compare `current` against `baseline`.
pub fn compare(current: &Scorecard, baseline: &Scorecard, allow_drift: bool) -> Comparison {
    if !allow_drift {
        if current.corpus_digest != baseline.corpus_digest {
            return Comparison::Incomparable(format!(
                "the corpus changed ({} → {}). Re-score the baseline, or pass \
                 --allow-config-drift and read the numbers knowing the labels moved.",
                baseline.corpus_digest, current.corpus_digest
            ));
        }
        if current.config_digest != baseline.config_digest {
            return Comparison::Incomparable(format!(
                "the configuration changed ({} → {}). A stricter gate finds fewer things \
                 without the reviewer having got worse, so this comparison would not mean \
                 what it looks like. Pass --allow-config-drift to make it anyway.",
                baseline.config_digest, current.config_digest
            ));
        }
    }

    let mut failures = Vec::new();

    if let (Some(now), Some(was)) = (current.recall(), baseline.recall())
        && now < was - EPSILON
    {
        failures.push(format!(
            "recall fell {:.1}% → {:.1}%",
            was * 100.0,
            now * 100.0
        ));
    }
    if current.forbidden_hits() > baseline.forbidden_hits() {
        failures.push(format!(
            "forbidden findings rose {} → {}",
            baseline.forbidden_hits(),
            current.forbidden_hits()
        ));
    }
    if current.clean_case_findings() > baseline.clean_case_findings() {
        failures.push(format!(
            "findings on clean pull requests rose {} → {}",
            baseline.clean_case_findings(),
            current.clean_case_findings()
        ));
    }
    if current.over_budget() > baseline.over_budget() {
        failures.push(format!(
            "cases over budget rose {} → {}",
            baseline.over_budget(),
            current.over_budget()
        ));
    }
    if current.errored() > baseline.errored() {
        failures.push(format!(
            "cases that failed to review rose {} → {}",
            baseline.errored(),
            current.errored()
        ));
    }

    if failures.is_empty() {
        Comparison::Pass
    } else {
        Comparison::Fail(failures)
    }
}

/// Render a scorecard as markdown, optionally against a baseline.
///
/// `allow_drift` reaches `compare` verbatim so the rendered verdict never
/// contradicts the exit status of `eval report --gate --allow-config-drift`.
pub fn markdown(card: &Scorecard, baseline: Option<&Scorecard>, allow_drift: bool) -> String {
    let mut out = String::new();

    out.push_str("# Review quality\n\n");
    let _ = writeln!(
        out,
        "corpus `{}` · config `{}` · {} case(s)",
        card.corpus_digest,
        card.config_digest,
        card.cases.len()
    );
    if card.loose_replays > 0 {
        // Loud, because the alternative is a table of numbers describing a
        // prompt nobody is running.
        let _ = writeln!(
            out,
            "\n> **{} answer(s) replayed by call order, not by content.** The corpus is stale \
             against these prompts; re-record before trusting anything below.",
            card.loose_replays
        );
    }

    out.push_str("\n## Totals\n\n");
    out.push_str("| metric | value |\n|---|---|\n");
    let _ = writeln!(out, "| recall | {} |", rate(card.recall()));
    let _ = writeln!(out, "| precision | {} |", rate(card.precision()));
    let _ = writeln!(out, "| F1 | {} |", rate(card.f1()));
    let _ = writeln!(
        out,
        "| found / expected | {} / {} |",
        card.true_positives(),
        card.true_positives() + card.missed()
    );
    let _ = writeln!(out, "| false positives | {} |", card.false_positives());
    let _ = writeln!(
        out,
        "| unscored (corpus has no opinion) | {} |",
        card.unscored()
    );
    let _ = writeln!(out, "| duplicates | {} |", card.duplicates());
    let _ = writeln!(out, "| forbidden | {} |", card.forbidden_hits());
    let _ = writeln!(
        out,
        "| findings on clean PRs | {} |",
        card.clean_case_findings()
    );
    let _ = writeln!(out, "| cost | ${:.4} |", card.cost_usd());
    let _ = writeln!(out, "| over budget | {} |", card.over_budget());
    if card.errored() > 0 {
        let _ = writeln!(out, "| **failed to review** | {} |", card.errored());
    }

    out.push_str("\n## Cases\n\n");
    out.push_str("| case | labels | found | missed | FP | unscored | forbidden | cost | wall |\n");
    out.push_str("|---|---|---|---|---|---|---|---|---|\n");
    for case in &card.cases {
        let _ = writeln!(
            out,
            "| `{}` | {} | {}/{} | {} | {} | {} | {} | ${:.4} | {:.1}s |",
            case.id,
            if case.exhaustive {
                "complete"
            } else {
                "partial"
            },
            case.true_positives,
            case.required(),
            case.missed.len(),
            case.false_positives,
            case.unscored,
            case.forbidden_hits.len(),
            case.cost_usd,
            case.wall_secs,
        );
    }

    let misses: Vec<&CaseScore> = card
        .cases
        .iter()
        .filter(|case| !case.missed.is_empty() || !case.forbidden_hits.is_empty())
        .collect();
    if !misses.is_empty() {
        out.push_str("\n## What it missed\n\n");
        for case in misses {
            let _ = writeln!(out, "**`{}`**", case.id);
            for id in &case.missed {
                let _ = writeln!(out, "- missed `{id}`");
            }
            for id in &case.forbidden_hits {
                let _ = writeln!(out, "- said the forbidden thing `{id}`");
            }
            out.push('\n');
        }
    }

    let noisy: Vec<&CaseScore> = card
        .cases
        .iter()
        // `exhaustive` matters: a partially-labelled case has no expectations
        // either, and calling its findings noise is the mistake the `unscored`
        // verdict exists to avoid.
        .filter(|case| case.exhaustive && case.required() == 0 && !case.judged.is_empty())
        .collect();
    if !noisy.is_empty() {
        out.push_str("\n## Noise on clean pull requests\n\n");
        for case in noisy {
            let _ = writeln!(out, "**`{}`**", case.id);
            for judged in &case.judged {
                // A finding demoted to the check-run summary has no line; print
                // the path alone rather than a `:0` that reads as a real anchor.
                let anchor = match judged.line {
                    Some(line) => format!("{}:{line}", judged.path),
                    None => judged.path.clone(),
                };
                let _ = writeln!(
                    out,
                    "- `{anchor}` {} — {}",
                    judged.title, judged.reason
                );
            }
            out.push('\n');
        }
    }

    if let Some(baseline) = baseline {
        out.push_str("\n## Against the baseline\n\n");
        match compare(card, baseline, false) {
            Comparison::Pass => out.push_str("**PASS** — nothing regressed.\n"),
            Comparison::Fail(reasons) => {
                out.push_str("**FAIL**\n\n");
                for reason in reasons {
                    let _ = writeln!(out, "- {reason}");
                }
            }
            Comparison::Incomparable(why) => {
                let _ = writeln!(out, "**NOT COMPARABLE** — {why}");
            }
        }
        out.push('\n');
        out.push_str("| metric | baseline | now |\n|---|---|---|\n");
        let _ = writeln!(
            out,
            "| recall | {} | {} |",
            rate(baseline.recall()),
            rate(card.recall())
        );
        let _ = writeln!(
            out,
            "| precision | {} | {} |",
            rate(baseline.precision()),
            rate(card.precision())
        );
        let _ = writeln!(
            out,
            "| forbidden | {} | {} |",
            baseline.forbidden_hits(),
            card.forbidden_hits()
        );
        let _ = writeln!(
            out,
            "| clean-PR findings | {} | {} |",
            baseline.clean_case_findings(),
            card.clean_case_findings()
        );
        let _ = writeln!(
            out,
            "| cost | ${:.4} | ${:.4} |",
            baseline.cost_usd(),
            card.cost_usd()
        );
    }

    out
}

/// A rate as a percentage, or `n/a` when the corpus asserts nothing.
fn rate(value: Option<f64>) -> String {
    match value {
        Some(value) => format!("{:.1}%", value * 100.0),
        None => "n/a".into(),
    }
}

#[cfg(test)]
#[path = "report_test.rs"]
mod tests;
