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
use crate::evidence::diff::parse_file_patch;
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

/// Finding `n`, quoting changed line `5(n mod 30)` — so file line
/// `5(n mod 30) + 2`. A later push's findings (`n` offset by 100) sit on the
/// same lines under new titles and rules, so they are new findings rather
/// than repeats. Every one clears the default gate (high, 0.75).
fn finding(n: usize, severity: &str, confidence: f64) -> Value {
    let k = (n % 30) * 5;
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
/// falsifier confirming everything. The first `criticals` findings of the
/// *last* pass are critical, so a per-pass or first-come cap would miss them.
fn three_passes(offset: usize, criticals: usize) -> MockModel {
    let pass = |index: usize| {
        let findings: Vec<Value> = (0..PER_PASS)
            .map(|i| {
                let n = offset + index * PER_PASS + i;
                if index == 2 && i < criticals {
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

    let proposal = review(&forge, Arc::new(three_passes(0, 2)), &config, &repo(), 7)
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

    let first = review(&forge, Arc::new(three_passes(0, 2)), &config, &repo(), 7)
        .await
        .expect("reviews");
    crate::app::apply::apply(&forge, &forge, &config, &first, None)
        .await
        .expect("applies");

    // A new push, thirty brand-new findings. Five conversations are already
    // open, so none of them is posted inline — all thirty are listed.
    forge.push(7, "sha-two", vec![large_file()]);
    let second = review(&forge, Arc::new(three_passes(100, 0)), &config, &repo(), 7)
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
    let first = review(&forge, Arc::new(three_passes(0, 2)), &config, &repo(), 7)
        .await
        .expect("reviews");
    crate::app::apply::apply(&forge, &forge, &config, &first, None)
        .await
        .expect("applies");

    // All five conversations are on the forge. Two were resolved by a
    // maintainer; the other three are still open and keep their slots.
    let threads: Vec<ReviewThread> = posted(&forge)
        .into_iter()
        .enumerate()
        .map(|(index, comment)| ReviewThread {
            id: format!("thread-{index}"),
            is_resolved: index < 2,
            is_outdated: false,
            comments: vec![ThreadComment {
                author: "tinysweeper[bot]".into(),
                body: comment.body,
                bot: true,
                maintainer: false,
            }],
            resolved_by_has_write_access: index < 2,
        })
        .collect();
    assert_eq!(threads.len(), 5, "every posted conversation is listed");
    let forge = {
        forge.push(7, "sha-two", vec![large_file()]);
        forge.with_review_threads(7, threads)
    };

    let second = review(&forge, Arc::new(three_passes(100, 0)), &config, &repo(), 7)
        .await
        .expect("reviews");
    assert_eq!(second.findings().count(), 2);
}

#[tokio::test]
async fn a_critical_finding_is_posted_even_when_the_budget_is_spent() {
    // Five conversations open, one new critical: it gets its own thread. A
    // critical bug demoted to a summary line because older nits are still
    // open is the one outcome the budget must never produce.
    let config = config();
    let forge = forge();
    let first = review(&forge, Arc::new(three_passes(0, 0)), &config, &repo(), 7)
        .await
        .expect("reviews");
    crate::app::apply::apply(&forge, &forge, &config, &first, None)
        .await
        .expect("applies");
    assert_eq!(posted(&forge).len(), 5);

    forge.push(7, "sha-two", vec![large_file()]);
    let second = review(&forge, Arc::new(three_passes(100, 1)), &config, &repo(), 7)
        .await
        .expect("reviews");
    let inline: Vec<&Finding> = second.findings().collect();
    assert_eq!(inline.len(), 1, "{inline:#?}");
    assert_eq!(inline[0].severity, Severity::Critical);

    crate::app::apply::apply(&forge, &forge, &config, &second, None)
        .await
        .expect("applies");
    assert_eq!(posted(&forge).len(), 6);

    // Still deduplicated, and still spending the budget: the same critical on
    // the next push is not posted again, and nothing else fits beside it.
    forge.push(7, "sha-three", vec![large_file()]);
    let third = review(&forge, Arc::new(three_passes(100, 1)), &config, &repo(), 7)
        .await
        .expect("reviews");
    assert_eq!(third.findings().count(), 0);
}

/// A lane carrying `findings` and nothing else, for exercising the cap alone.
fn lane_of(findings: Vec<Finding>) -> LaneProposal {
    LaneProposal {
        lane: LaneId::Critique,
        check_name: LaneId::Critique.check_name(),
        conclusion: CheckConclusion::Failure,
        summary: "Reviewed.".into(),
        findings,
        noted: Vec::new(),
        resolved: vec![],
        pending: vec![],
        deduped: 0,
        highest_severity: Some(Severity::Critical),
        usage: Default::default(),
        models: vec![],
        unanswered: vec![],
        overflow: vec![],
    }
}

/// A finding on `src/lib.rs` at `line` (`None` for a file-level finding).
fn proposal_finding(
    title: &str,
    severity: Severity,
    line: Option<u64>,
    confidence: f64,
) -> Finding {
    Finding {
        lane: LaneId::Critique,
        severity,
        confidence,
        path: "src/lib.rs".into(),
        line,
        end_line: None,
        rule: "rule".into(),
        title: title.into(),
        body: "body".into(),
        suggestion: None,
        applicable: None,
        late: false,
        identity: None,
        aliases: vec![],
        grouped: false,
        review_pass: 1,
        corroboration: 1,
    }
}

#[test]
fn the_bypass_is_for_critical_only() {
    let mut lanes = vec![lane_of(vec![
        proposal_finding("critical one", Severity::Critical, Some(1), 0.9),
        proposal_finding("critical two", Severity::Critical, Some(20), 0.9),
        proposal_finding("high", Severity::High, Some(40), 0.9),
    ])];
    cap_proposal_findings(&mut lanes, 1, &|_| true);

    let kept: Vec<&str> = lanes[0].findings.iter().map(|f| f.title.as_str()).collect();
    assert_eq!(kept, vec!["critical one", "critical two"]);
    assert_eq!(lanes[0].overflow.len(), 1);
    assert_eq!(lanes[0].overflow[0].title, "high");
}

#[test]
fn a_finding_that_cannot_anchor_inline_does_not_spend_the_budget() {
    // The file-level finding ranks first on confidence, but `inline_comments`
    // would never post it. It must not take the one slot: the strongest
    // anchored finding does, and the file-level one stays where it was
    // reported rather than being moved to the hub's overflow list.
    let mut lanes = vec![lane_of(vec![
        proposal_finding("file level", Severity::High, None, 0.99),
        proposal_finding("anchored strong", Severity::High, Some(10), 0.9),
        proposal_finding("anchored weak", Severity::High, Some(30), 0.8),
    ])];
    cap_proposal_findings(&mut lanes, 1, &|finding| finding.line.is_some());

    let kept: Vec<&str> = lanes[0].findings.iter().map(|f| f.title.as_str()).collect();
    assert_eq!(kept, vec!["file level", "anchored strong"]);
    let over: Vec<&str> = lanes[0].overflow.iter().map(|f| f.title.as_str()).collect();
    assert_eq!(over, vec!["anchored weak"]);
}

#[test]
fn only_a_line_inside_the_live_diff_is_anchorable() {
    // The same rule `apply::inline_comments` applies before posting: a line
    // outside every hunk is never a conversation, so it never spends a slot.
    let diffs = [parse_file_patch(
        "src/lib.rs",
        "@@ -1,2 +1,3 @@\n fn main() {\n+    let x = 1;\n }\n",
    )];
    let inside = proposal_finding("inside", Severity::High, Some(2), 0.9);
    let outside = proposal_finding("outside", Severity::High, Some(50), 0.9);
    let unanchored = proposal_finding("none", Severity::High, None, 0.9);

    assert!(inline_anchor_within(&inside, &diffs));
    assert!(!inline_anchor_within(&outside, &diffs));
    assert!(!inline_anchor_within(&unanchored, &diffs));
}
