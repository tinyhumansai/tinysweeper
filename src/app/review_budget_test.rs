//! Golden tests for the per-pull-request inline-comment budget.
//!
//! The incident: tinyskills#24 collected 115 top-level findings and a 72-line
//! test-only pull request collected 11, because `review.max_comments` was
//! spent afresh on every review cycle and its overflow was simply dropped.
//! These pin the replacement: one budget across lanes, passes and cycles, a
//! global ranking, and an overflow that is listed rather than lost.

use std::sync::Arc;

use serde_json::{Value, json};

use super::*;
use crate::forge::types::{ChangedFile, FileStatus, PullRequest, ReviewThread, ThreadComment};
use crate::forge::{MockForge, MockState};
use crate::harness::mock::MockModel;

/// Changed lines in the fixture: enough for every finding to sit more than
/// `LINE_TOLERANCE` away from every other, so none are merged as repeats.
const LINES: usize = 150;
/// Findings each review pass reports.
const PER_PASS: usize = 10;

/// The defaults, narrowed to one lane at full adaptive depth, with every
/// side-channel model call turned off so the canned queue is the whole run.
fn config() -> Config {
    let mut config: Config = crate::config::DEFAULTS
        .parse::<toml::Table>()
        .unwrap()
        .try_into()
        .unwrap();
    config.review.lanes = vec!["critique".into()];
    config.review.passes = 3;
    config.summary.enabled = false;
    config.wireframe.enabled = false;
    config.overview.enabled = false;
    config
}

fn repo() -> RepoId {
    RepoId::parse("tinyhumansai/tinysweeper").unwrap()
}

fn large_file() -> ChangedFile {
    let mut patch = format!("@@ -1,2 +1,{} @@\n fn main() {{\n", LINES + 2);
    for i in 0..LINES {
        patch.push_str(&format!("+    let x{i} = {i};\n"));
    }
    patch.push_str(" }\n");
    ChangedFile {
        path: "src/large.rs".into(),
        status: FileStatus::Modified,
        patch: Some(patch),
        ..ChangedFile::default()
    }
}

fn forge() -> MockForge {
    let mut state = MockState::default();
    state.pull_requests.insert(
        7,
        PullRequest {
            number: 7,
            title: "feat: a large change".into(),
            body: "A large change.".into(),
            head_sha: "sha-one".into(),
            ..PullRequest::default()
        },
    );
    state.files.insert(7, vec![large_file()]);
    MockForge::with_state(state)
}

/// Finding `n` of the whole review, quoting changed line `5n` — so file line
/// `5n + 2`. Every one clears the default gate (high, 0.75).
fn finding(n: usize, severity: &str, confidence: f64) -> Value {
    let k = n * 5;
    json!({
        "path": "src/large.rs",
        "existing_code": format!("let x{k} = {k};"),
        "rule": format!("rule-{n}"),
        "title": format!("Finding {n}"),
        "body": "detail.",
        "severity": severity,
        "confidence": confidence
    })
}

/// Three passes of ten qualifying findings, each pass followed by the
/// falsifier confirming everything. The two most severe findings arrive in
/// the *last* pass, so a per-pass or first-come cap would miss them.
fn three_passes(offset: usize) -> MockModel {
    let pass = |index: usize| {
        let findings: Vec<Value> = (0..PER_PASS)
            .map(|i| {
                let n = offset + index * PER_PASS + i;
                if index == 2 && i < 2 {
                    finding(n, "critical", 0.8)
                } else {
                    finding(n, "high", 0.76 + (i as f64) / 100.0)
                }
            })
            .collect();
        json!({"summary": "Several problems.", "findings": findings})
    };
    MockModel::new()
        .then(pass(0))
        .then(json!({"incorrect": []}))
        .then(pass(1))
        .then(json!({"incorrect": []}))
        .then(pass(2))
        .then(json!({"incorrect": []}))
}

fn posted(forge: &MockForge) -> Vec<crate::forge::types::ReviewComment> {
    forge
        .writes()
        .into_iter()
        .filter_map(|write| match write {
            crate::forge::Write::Review { comments, .. } => Some(comments),
            _ => None,
        })
        .flatten()
        .collect()
}

#[tokio::test]
async fn thirty_findings_over_three_passes_post_the_budget_and_list_the_rest() {
    let config = config();
    assert_eq!(config.review.max_comments, 5, "the shipped budget");
    let forge = forge();

    let proposal = review(&forge, Arc::new(three_passes(0)), &config, &repo(), 7)
        .await
        .expect("reviews");

    let inline: Vec<&Finding> = proposal.findings().collect();
    let overflow: Vec<&Finding> = proposal
        .lanes
        .iter()
        .flat_map(|lane| lane.overflow.iter())
        .collect();
    assert_eq!(inline.len(), 5, "{inline:#?}");
    assert_eq!(inline.len() + overflow.len(), 30, "nothing is lost");

    // Ranked globally: both criticals, from the third pass, made the cut,
    // then the most confident highs.
    let mut kept: Vec<&str> = inline.iter().map(|f| f.title.as_str()).collect();
    kept.sort_unstable();
    assert_eq!(
        kept,
        vec![
            "Finding 19",
            "Finding 20",
            "Finding 21",
            "Finding 29",
            "Finding 9"
        ]
    );
    assert!(
        proposal.blocked(),
        "the cap hides comments, not the verdict"
    );

    // The overflow is named in the hub, by title and location.
    let hub = crate::summary::render(&config, &proposal);
    for finding in &overflow {
        let line = finding.line.expect("placed");
        // Rendered through the hub's Markdown escaping, hence `\.`.
        let entry = format!("{} (`src/large\\.rs:{line}`)", finding.title);
        assert!(hub.contains(&entry), "missing {entry}:\n{hub}");
    }

    crate::app::apply::apply(&forge, &forge, &config, &proposal, None)
        .await
        .expect("applies");
    assert_eq!(posted(&forge).len(), 5);
}

#[tokio::test]
async fn open_findings_from_an_earlier_push_spend_the_same_budget() {
    let config = config();
    let forge = forge();

    let first = review(&forge, Arc::new(three_passes(0)), &config, &repo(), 7)
        .await
        .expect("reviews");
    crate::app::apply::apply(&forge, &forge, &config, &first, None)
        .await
        .expect("applies");

    // A new push, thirty brand-new findings. Five conversations are already
    // open, so none of them is posted inline — all thirty are listed.
    forge.push(7, "sha-two", vec![large_file()]);
    let second = review(&forge, Arc::new(three_passes(100)), &config, &repo(), 7)
        .await
        .expect("reviews");
    assert_eq!(second.findings().count(), 0);
    assert_eq!(
        second
            .lanes
            .iter()
            .map(|lane| lane.overflow.len())
            .sum::<usize>(),
        30
    );
}

#[tokio::test]
async fn a_resolved_conversation_frees_its_slot() {
    let config = config();
    let forge = forge();
    let first = review(&forge, Arc::new(three_passes(0)), &config, &repo(), 7)
        .await
        .expect("reviews");
    crate::app::apply::apply(&forge, &forge, &config, &first, None)
        .await
        .expect("applies");

    // Two of the five threads were resolved by a maintainer.
    let resolved: Vec<ReviewThread> = posted(&forge)
        .into_iter()
        .take(2)
        .enumerate()
        .map(|(index, comment)| ReviewThread {
            id: format!("thread-{index}"),
            is_resolved: true,
            is_outdated: false,
            comments: vec![ThreadComment {
                author: "tinysweeper[bot]".into(),
                body: comment.body,
                bot: true,
                maintainer: false,
            }],
            resolved_by_has_write_access: true,
        })
        .collect();
    let forge = {
        forge.push(7, "sha-two", vec![large_file()]);
        forge.with_review_threads(7, resolved)
    };

    let second = review(&forge, Arc::new(three_passes(100)), &config, &repo(), 7)
        .await
        .expect("reviews");
    assert_eq!(second.findings().count(), 2);
}
