//! `tinysweeper review` — the read-only half.
//!
//! Runs the deterministic scanners and the model lanes, and writes a
//! **proposal** to disk. It publishes nothing. `apply` does that, later, with a
//! token this process never holds.
//!
//! That split is the security boundary from `AGENTS.md` made concrete: this
//! module cannot construct a `ForgeWrite`, so no amount of confusion here can
//! result in something being posted.

use std::path::Path;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::config::types::{Config, LaneId, Severity};
use crate::error::{Error, Result};
use crate::evidence::diff::{FileDiff, parse_changed_files};
use crate::findings::types::Finding;
use crate::forge::types::{CheckConclusion, PullRequestContext, RepoId};
use crate::lanes::{Lane, LaneInput, LaneOutcome, critique::Critique};
use crate::ports::forge::ForgeRead;
use crate::ports::model::{Model, Usage};
use crate::scan;

/// What a review run concluded, ready for `apply` to publish.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Proposal {
    /// Schema version of this file.
    pub version: u32,
    /// The repository, as `owner/name`.
    pub repo: String,
    /// The pull request number.
    pub number: u64,
    /// The commit reviewed. `apply` refuses to publish against a different one.
    pub head_sha: String,
    /// One entry per lane that ran.
    pub lanes: Vec<LaneProposal>,
    /// Total model spend for the run.
    pub cost_usd: f64,
    /// Total tokens served from the provider's prompt cache.
    pub cached_tokens: u64,
}

/// One lane's verdict.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LaneProposal {
    /// The lane.
    pub lane: LaneId,
    /// The check-run name to publish under.
    pub check_name: String,
    /// The conclusion.
    pub conclusion: CheckConclusion,
    /// Summary line.
    pub summary: String,
    /// Findings that survived filtering.
    pub findings: Vec<Finding>,
}

impl Proposal {
    /// Whether any lane blocks the merge.
    pub fn blocked(&self) -> bool {
        self.lanes.iter().any(|l| l.conclusion.blocks())
    }

    /// Every finding across every lane.
    pub fn findings(&self) -> impl Iterator<Item = &Finding> {
        self.lanes.iter().flat_map(|l| l.findings.iter())
    }
}

/// Run the review.
pub async fn review(
    forge: &dyn ForgeRead,
    model: Arc<dyn Model>,
    config: &Config,
    repo: &RepoId,
    number: u64,
) -> Result<Proposal> {
    let context = forge.pull_request_context(repo, number).await?;
    let diffs = reviewable_diffs(config, &context)?;

    // Kill switches are checked before anything expensive, so a label really
    // does stop the bot rather than merely hiding its output.
    if let Some(label) = kill_switch(config, &context) {
        return Ok(Proposal {
            version: 1,
            repo: repo.to_string(),
            number,
            head_sha: context.pull_request.head_sha.clone(),
            lanes: vec![LaneProposal {
                lane: LaneId::Gate,
                check_name: LaneId::Gate.check_name(),
                conclusion: CheckConclusion::Neutral,
                summary: format!("Skipped: `{label}` is applied."),
                findings: vec![],
            }],
            cost_usd: 0.0,
            cached_tokens: 0,
        });
    }

    let scan_findings = run_scanners(config, &diffs, &context);

    let mut lanes = Vec::new();
    let mut usage = Usage::default();

    for lane_id in config.enabled_lanes() {
        let lane: Box<dyn Lane> = match lane_id {
            LaneId::Critique => Box::new(Critique::new(model.clone())),
            // The remaining lanes land in M4. Until then they report Neutral
            // rather than Success: claiming a lane passed when it never ran
            // would make requiring it in branch protection meaningless.
            _ => {
                lanes.push(LaneProposal {
                    lane: lane_id,
                    check_name: lane_id.check_name(),
                    conclusion: CheckConclusion::Neutral,
                    summary: "Not implemented yet.".into(),
                    findings: vec![],
                });
                continue;
            }
        };

        let outcome = lane
            .run(LaneInput {
                config,
                pull_request: &context.pull_request,
                diffs: &diffs,
                scan_findings: &scan_findings,
                repo_policy: repo_policy().as_deref(),
                reviewed_evidence: "",
                prior_findings: &[],
            })
            .await?;

        usage.add(outcome.usage);
        if usage.cost_usd > config.models.budget_usd_per_pr {
            return Err(Error::Budget {
                spent: usage.cost_usd,
                limit: config.models.budget_usd_per_pr,
            });
        }

        lanes.push(lane_proposal(config, lane_id, outcome));
    }

    // Scanner findings that no lane actually adjudicated still have to reach a
    // human. A committed private key must not vanish because the lane that
    // would have discussed it has not been written yet.
    //
    // The condition here was wrong, and tinysweeper caught it reviewing itself:
    // it keyed on whether the `commits` lane was *enabled* rather than whether
    // it had actually run. Under the default config `commits` is enabled and
    // unimplemented, so it contributed a Neutral placeholder and swallowed every
    // scanner finding — a committed secret would have passed silently, which is
    // precisely the failure this code exists to prevent. The unit test agreed
    // with the code because it disabled `commits` to reach the branch, so it
    // tested the shape of the implementation rather than the requirement.
    let unclaimed: Vec<Finding> = scan_findings
        .iter()
        .cloned()
        .map(Finding::from)
        .filter(|f| f.severity >= Severity::High)
        .collect();

    if !unclaimed.is_empty() {
        let adjudicated = lanes
            .iter()
            .any(|l| l.lane == LaneId::Commits && l.conclusion != CheckConclusion::Neutral);

        if !adjudicated {
            // Replace the placeholder rather than sitting beside it, so the
            // check run for `commits` reports the findings instead of claiming
            // there was nothing to do.
            lanes.retain(|l| l.lane != LaneId::Commits);
            lanes.push(LaneProposal {
                lane: LaneId::Commits,
                check_name: LaneId::Commits.check_name(),
                conclusion: CheckConclusion::Failure,
                summary: format!(
                    "{} finding(s) from the deterministic scanners.",
                    unclaimed.len()
                ),
                findings: unclaimed,
            });
        }
    }

    lanes.push(gate(&lanes));

    Ok(Proposal {
        version: 1,
        repo: repo.to_string(),
        number,
        head_sha: context.pull_request.head_sha.clone(),
        lanes,
        cost_usd: usage.cost_usd,
        cached_tokens: usage.cached_tokens,
    })
}

/// Write a proposal to disk for `apply` to pick up.
pub fn write_proposal(proposal: &Proposal, path: &Path) -> Result<()> {
    let json = serde_json::to_string_pretty(proposal)?;
    std::fs::write(path, json).map_err(|err| Error::path(path, err))
}

/// Read a proposal written by `review`.
pub fn read_proposal(path: &Path) -> Result<Proposal> {
    let text = std::fs::read_to_string(path).map_err(|err| Error::path(path, err))?;
    serde_json::from_str(&text).map_err(Error::from)
}

fn lane_proposal(config: &Config, lane: LaneId, outcome: LaneOutcome) -> LaneProposal {
    let gate = config.severity_gate();
    let minimum = config.review.confidence_min;

    // `RawFinding::into_finding` scrubs a finding's own title and body, but
    // the lane summary is free text the model wrote, and a model asked to
    // review a diff containing a credential routinely repeats it back. This
    // is the last stop before that text is serialized into the proposal,
    // printed to CI logs, and rendered into a check-run summary.
    let outcome_summary = scan::secrets::scrub(&outcome.summary);

    // The conclusion is decided from every confidence-qualified finding,
    // before the posting gate below hides some of them from view. A
    // `severity_gate` configured above a lane's `fail_on` (e.g. gate=high,
    // fail_on=medium) must still fail the lane on a medium finding even
    // though that finding will not become a visible comment — otherwise the
    // posting gate silently doubles as a pass/fail gate nobody asked for.
    let confidence_qualified: Vec<&Finding> = outcome
        .findings
        .iter()
        .filter(|f| f.confidence >= minimum)
        .collect();
    let conclusion = if outcome.skipped.is_some() {
        CheckConclusion::Neutral
    } else if confidence_qualified
        .iter()
        .any(|f| f.severity >= config.fail_on(lane))
    {
        CheckConclusion::Failure
    } else {
        CheckConclusion::Success
    };

    let mut findings: Vec<Finding> = outcome
        .findings
        .into_iter()
        .filter(|f| f.meets(gate, minimum))
        .collect();

    // Most severe first, so the cap keeps what matters when it bites.
    findings.sort_by(|a, b| {
        b.severity
            .cmp(&a.severity)
            .then(b.confidence.total_cmp(&a.confidence))
    });

    let over_cap = findings.len().saturating_sub(config.review.max_comments);
    findings.truncate(config.review.max_comments);

    let summary = if over_cap > 0 {
        format!("{outcome_summary} (+{over_cap} more not shown)")
    } else {
        outcome_summary
    };

    LaneProposal {
        lane,
        check_name: lane.check_name(),
        conclusion,
        summary,
        findings,
    }
}

/// The deterministic aggregate every other lane feeds.
fn gate(lanes: &[LaneProposal]) -> LaneProposal {
    let blocking: Vec<&LaneProposal> = lanes.iter().filter(|l| l.conclusion.blocks()).collect();

    let (conclusion, summary) = if blocking.is_empty() {
        (CheckConclusion::Success, "All lanes passed.".to_string())
    } else {
        (
            CheckConclusion::Failure,
            format!(
                "Blocked by {}.",
                blocking
                    .iter()
                    .map(|l| l.lane.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        )
    };

    LaneProposal {
        lane: LaneId::Gate,
        check_name: LaneId::Gate.check_name(),
        conclusion,
        summary,
        findings: vec![],
    }
}

fn kill_switch(config: &Config, context: &PullRequestContext) -> Option<String> {
    let labels = &context.pull_request.labels;
    for candidate in [&config.labels.human_review, &config.labels.manual_only] {
        if !candidate.is_empty() && labels.iter().any(|l| l == candidate) {
            return Some(candidate.clone());
        }
    }
    None
}

fn reviewable_diffs(config: &Config, context: &PullRequestContext) -> Result<Vec<FileDiff>> {
    let mut builder = globset::GlobSetBuilder::new();
    for pattern in &config.paths.ignore {
        builder.add(
            globset::Glob::new(pattern)
                .map_err(|err| Error::config(format!("invalid ignore glob `{pattern}`: {err}")))?,
        );
    }
    let ignored = builder
        .build()
        .map_err(|err| Error::config(format!("could not build the ignore set: {err}")))?;

    let kept: Vec<_> = context
        .files
        .iter()
        .filter(|f| !ignored.is_match(&f.path))
        .cloned()
        .collect();

    Ok(parse_changed_files(&kept))
}

fn run_scanners(
    config: &Config,
    diffs: &[FileDiff],
    context: &PullRequestContext,
) -> Vec<scan::types::Finding> {
    let max_blob = config
        .lane(LaneId::Commits)
        .and_then(|l| l.max_blob_bytes)
        .unwrap_or(1024 * 1024);

    let mut findings = Vec::new();
    for diff in diffs {
        findings.extend(scan::secrets::scan_added_lines(
            &diff.path,
            diff.added_lines(),
        ));
        findings.extend(scan::workflows::scan_added_lines(
            &diff.path,
            diff.added_lines(),
        ));
    }
    findings.extend(scan::blobs::scan_files(&context.files, max_blob));
    findings
}

/// Repository policy for the prompt: this repository's own `AGENTS.md`.
///
/// Read from the checkout rather than the API — the workflow already has the
/// tree, and reading a file is cheaper and more reliable than another request.
fn repo_policy() -> Option<String> {
    for candidate in ["AGENTS.md", "CLAUDE.md", ".github/AGENTS.md"] {
        if let Ok(text) = std::fs::read_to_string(candidate) {
            // Only the first part: the whole file would crowd out the diff, and
            // conventions live near the top.
            return Some(text.chars().take(6000).collect());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::forge::types::{ChangedFile, FileStatus, PullRequest};
    use crate::forge::{MockForge, MockState};
    use crate::harness::mock::MockModel;
    use serde_json::json;
    use std::collections::BTreeMap;

    fn config() -> Config {
        crate::config::DEFAULTS
            .parse::<toml::Table>()
            .unwrap()
            .try_into()
            .unwrap()
    }

    fn repo() -> RepoId {
        RepoId::parse("tinyhumansai/tinysweeper").unwrap()
    }

    fn forge_with(files: Vec<ChangedFile>, labels: Vec<String>) -> MockForge {
        let mut state = MockState::default();
        state.pull_requests.insert(
            7,
            PullRequest {
                number: 7,
                title: "feat: something".into(),
                head_sha: "abc123".into(),
                labels,
                ..PullRequest::default()
            },
        );
        state.files.insert(7, files);
        MockForge::with_state(state)
    }

    fn rust_file() -> ChangedFile {
        ChangedFile {
            path: "src/main.rs".into(),
            status: FileStatus::Modified,
            patch: Some("@@ -1,2 +1,3 @@\n fn main() {\n+    let x = items[i];\n }\n".into()),
            ..ChangedFile::default()
        }
    }

    #[tokio::test]
    async fn a_clean_review_produces_a_passing_gate() {
        let forge = forge_with(vec![rust_file()], vec![]);
        let proposal = review(&forge, Arc::new(MockModel::silent()), &config(), &repo(), 7)
            .await
            .expect("reviews");

        let gate = proposal
            .lanes
            .iter()
            .find(|l| l.lane == LaneId::Gate)
            .expect("gate present");
        assert_eq!(gate.conclusion, CheckConclusion::Success);
        assert!(!proposal.blocked());
    }

    #[tokio::test]
    async fn a_high_severity_finding_blocks_the_gate() {
        let model = MockModel::always(json!({
            "summary": "Unchecked index.",
            "findings": [{
                "path": "src/main.rs", "line": 2,
                "rule": "unchecked-index",
                "title": "Guard the index", "body": "…",
                "severity": "high", "confidence": 0.9
            }]
        }));
        let forge = forge_with(vec![rust_file()], vec![]);
        let proposal = review(&forge, Arc::new(model), &config(), &repo(), 7)
            .await
            .expect("reviews");

        assert!(proposal.blocked());
        let gate = proposal
            .lanes
            .iter()
            .find(|l| l.lane == LaneId::Gate)
            .unwrap();
        assert!(gate.summary.contains("critique"), "{}", gate.summary);
    }

    #[tokio::test]
    async fn a_kill_switch_label_stops_the_run_before_any_model_call() {
        let model = MockModel::new();
        let forge = forge_with(
            vec![rust_file()],
            vec!["tinysweeper:human-review".to_string()],
        );

        let proposal = review(&forge, Arc::new(model.clone()), &config(), &repo(), 7)
            .await
            .expect("reviews");

        assert_eq!(
            model.calls(),
            0,
            "a kill switch must stop work, not hide output"
        );
        assert!(!proposal.blocked());
        assert!(proposal.lanes[0].summary.contains("human-review"));
    }

    #[tokio::test]
    async fn ignored_paths_never_reach_a_lane() {
        let lockfile = ChangedFile {
            path: "Cargo.lock".into(),
            status: FileStatus::Modified,
            patch: Some("@@ -1 +1,2 @@\n a\n+b\n".into()),
            ..ChangedFile::default()
        };
        let model = MockModel::new();
        let forge = forge_with(vec![lockfile], vec![]);

        let proposal = review(&forge, Arc::new(model.clone()), &config(), &repo(), 7)
            .await
            .expect("reviews");

        assert_eq!(model.calls(), 0, "only ignored files changed");
        assert!(!proposal.blocked());
    }

    fn file_with_committed_secret() -> ChangedFile {
        let key = format!("{}{}", "AKIA", "IOSFODNN7EXAMPLE");
        ChangedFile {
            path: "src/config.rs".into(),
            status: FileStatus::Modified,
            patch: Some(format!("@@ -1 +1,2 @@\n a\n+const K: &str = \"{key}\";\n")),
            ..ChangedFile::default()
        }
    }

    #[tokio::test]
    async fn a_committed_secret_fails_under_the_default_configuration() {
        // The regression test for the bug tinysweeper found in itself. The
        // original test disabled the `commits` lane to reach the branch it was
        // exercising, which meant it agreed with the implementation instead of
        // checking the requirement — and under the shipped defaults a committed
        // key passed silently.
        let forge = forge_with(vec![file_with_committed_secret()], vec![]);
        let proposal = review(&forge, Arc::new(MockModel::silent()), &config(), &repo(), 7)
            .await
            .expect("reviews");

        assert!(proposal.blocked(), "{:#?}", proposal.lanes);
        let commits = proposal
            .lanes
            .iter()
            .find(|l| l.lane == LaneId::Commits)
            .expect("the commits lane reports it");
        assert_eq!(commits.conclusion, CheckConclusion::Failure);
        assert_eq!(commits.findings.len(), 1);

        let rendered = serde_json::to_string(&proposal).unwrap();
        assert!(!rendered.contains("IOSFODNN7EXAMPLE"), "value leaked");
    }

    #[tokio::test]
    async fn a_file_with_no_patch_does_not_suppress_the_others() {
        // The critique lane claimed, reviewing this repository, that
        // `reviewable_diffs` drops every file whenever a pull request touches a
        // submodule or anything else with no inline patch. Verifying rather
        // than taking a model's word for it.
        let submodule = ChangedFile {
            path: "vendor/something".into(),
            status: FileStatus::Modified,
            patch: None,
            ..ChangedFile::default()
        };
        let model = MockModel::always(json!({
            "summary": "Reviewed.",
            "findings": [{
                "path": "src/main.rs", "line": 2,
                "rule": "unchecked-index", "title": "Guard the index",
                "body": "…", "severity": "high", "confidence": 0.9
            }]
        }));
        let forge = forge_with(vec![submodule, rust_file()], vec![]);
        let proposal = review(&forge, Arc::new(model), &config(), &repo(), 7)
            .await
            .expect("reviews");

        assert_eq!(
            proposal.findings().count(),
            1,
            "the patchless file must not suppress review of the rest"
        );
    }

    #[tokio::test]
    async fn only_one_commits_lane_is_ever_reported() {
        // The placeholder must be replaced, not accompanied: two check runs of
        // the same name is a confusing way to fail.
        let forge = forge_with(vec![file_with_committed_secret()], vec![]);
        let proposal = review(&forge, Arc::new(MockModel::silent()), &config(), &repo(), 7)
            .await
            .expect("reviews");

        assert_eq!(
            proposal
                .lanes
                .iter()
                .filter(|l| l.lane == LaneId::Commits)
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn a_committed_secret_fails_even_with_the_commits_lane_disabled() {
        let mut config = config();
        config.review.lanes = vec!["critique".into()];

        let forge = forge_with(vec![file_with_committed_secret()], vec![]);
        let proposal = review(&forge, Arc::new(MockModel::silent()), &config, &repo(), 7)
            .await
            .expect("reviews");

        assert!(proposal.blocked(), "{:#?}", proposal.lanes);
        let rendered = serde_json::to_string(&proposal).unwrap();
        assert!(!rendered.contains("IOSFODNN7EXAMPLE"), "value leaked");
    }

    #[tokio::test]
    async fn findings_below_the_gate_are_dropped_before_they_become_comments() {
        let model = MockModel::always(json!({
            "summary": "A nit.",
            "findings": [{
                "path": "src/main.rs", "line": 2,
                "rule": "style", "title": "Rename this", "body": "…",
                "severity": "low", "confidence": 0.9
            }]
        }));
        let forge = forge_with(vec![rust_file()], vec![]);
        let proposal = review(&forge, Arc::new(model), &config(), &repo(), 7)
            .await
            .expect("reviews");

        assert_eq!(proposal.findings().count(), 0, "severity gate is medium");
    }

    #[tokio::test]
    async fn a_credential_quoted_back_in_the_lane_summary_is_scrubbed() {
        // `RawFinding::into_finding` scrubs a finding's title and body, but a
        // model that quotes a secret into its free-text summary bypassed
        // that entirely — the value would reach the proposal, the CLI log,
        // and the published check-run summary untouched.
        let key = format!("{}{}", "AKIA", "IOSFODNN7EXAMPLE");
        let model = MockModel::always(json!({
            "summary": format!("Found a hardcoded key: {key}"),
            "findings": []
        }));
        let forge = forge_with(vec![rust_file()], vec![]);
        let proposal = review(&forge, Arc::new(model), &config(), &repo(), 7)
            .await
            .expect("reviews");

        let critique = proposal
            .lanes
            .iter()
            .find(|l| l.lane == LaneId::Critique)
            .unwrap();
        assert!(
            !critique.summary.contains("IOSFODNN7EXAMPLE"),
            "{}",
            critique.summary
        );
        let rendered = serde_json::to_string(&proposal).unwrap();
        assert!(!rendered.contains("IOSFODNN7EXAMPLE"), "value leaked");
    }

    #[tokio::test]
    async fn a_finding_below_the_posting_gate_can_still_fail_the_lane() {
        // A model reviewer, reviewing this repository, pointed out that
        // `severity_gate` (what gets posted) and `fail_on` (what fails the
        // check) are independent knobs, but the conclusion was computed from
        // the already-gated findings. With `severity_gate = high` and
        // `fail_on = medium`, a medium finding was hidden from the posted
        // comments *and* silently swallowed by the pass/fail decision, even
        // though the configuration says a medium finding must fail the lane.
        let mut config = config();
        config.review.severity_gate = "high".into();
        config.lanes.insert(
            "critique".into(),
            crate::config::types::Lane {
                fail_on: Some("medium".into()),
                ..Default::default()
            },
        );

        let model = MockModel::always(json!({
            "summary": "A medium issue.",
            "findings": [{
                "path": "src/main.rs", "line": 2,
                "rule": "style", "title": "Worth a look", "body": "…",
                "severity": "medium", "confidence": 0.9
            }]
        }));
        let forge = forge_with(vec![rust_file()], vec![]);
        let proposal = review(&forge, Arc::new(model), &config, &repo(), 7)
            .await
            .expect("reviews");

        assert!(
            proposal.blocked(),
            "a medium finding must fail the lane when fail_on=medium, \
             even though severity_gate=high keeps it off the posted comments"
        );
        let critique = proposal
            .lanes
            .iter()
            .find(|l| l.lane == LaneId::Critique)
            .unwrap();
        assert_eq!(critique.conclusion, CheckConclusion::Failure);
        assert!(
            critique.findings.is_empty(),
            "the posting gate still hides it: {:#?}",
            critique.findings
        );
    }

    #[tokio::test]
    async fn a_lane_that_has_not_been_written_is_neutral_not_successful() {
        let forge = forge_with(vec![rust_file()], vec![]);
        let proposal = review(&forge, Arc::new(MockModel::silent()), &config(), &repo(), 7)
            .await
            .expect("reviews");

        let security = proposal
            .lanes
            .iter()
            .find(|l| l.lane == LaneId::Security)
            .expect("present");
        assert_eq!(security.conclusion, CheckConclusion::Neutral);
    }

    #[test]
    fn a_proposal_round_trips_through_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("findings.json");
        let proposal = Proposal {
            version: 1,
            repo: "tinyhumansai/tinysweeper".into(),
            number: 7,
            head_sha: "abc123".into(),
            lanes: vec![],
            cost_usd: 0.02,
            cached_tokens: 900,
        };

        write_proposal(&proposal, &path).expect("writes");
        let read = read_proposal(&path).expect("reads");
        assert_eq!(read.head_sha, "abc123");
        assert_eq!(read.cached_tokens, 900);
    }

    #[tokio::test]
    async fn the_gate_is_always_present_even_when_nothing_ran() {
        let forge = forge_with(vec![], vec![]);
        let proposal = review(&forge, Arc::new(MockModel::silent()), &config(), &repo(), 7)
            .await
            .expect("reviews");

        assert!(proposal.lanes.iter().any(|l| l.lane == LaneId::Gate));
    }

    #[allow(dead_code)]
    fn unused(_: BTreeMap<u64, u64>) {}
}
