//! Lanes: one agent, one narrow job, one check run.
//!
//! A lane takes evidence and returns a [`LaneOutcome`]. It does **not** take a
//! [`ForgeWrite`](crate::ports::forge::ForgeWrite), so it cannot mutate a pull
//! request even by mistake — lanes propose, `src/apply` disposes. That is the
//! security boundary from `AGENTS.md`, enforced by the type system rather than
//! by discipline.

pub mod anchor;
pub mod commits;
pub mod coverage;
pub mod critique;
pub mod description;
pub mod e2e;
pub mod fanout;
pub mod grouping;
pub mod large_pr;
pub mod mechanical;
pub(crate) mod preparation;
pub mod security;
pub mod tests;
pub mod triage;

use std::collections::BTreeMap;

use async_trait::async_trait;

use crate::config::types::{Config, LaneId, Severity};
use crate::council::Reviewer;
use crate::error::Result;
use crate::evidence::diff::FileDiff;
use crate::findings::types::Finding;
use crate::flows::runner::{Answer, Asking};
use crate::forge::types::{CheckConclusion, Commit, PullRequest};
use crate::harness::schema::LaneResponse;
use crate::ports::model::Spend;
use crate::ports::tree::TreeReader;
use crate::scan::types::{Finding as ScanFinding, ScanKind};

/// Receives bounded yield counters when one review pass finishes.
///
/// Implementations aggregate by lane and pass number; they must not retain
/// group paths, prompts, or findings.
pub trait PassYieldObserver: Send + Sync {
    /// Record one group's candidate and surviving finding counts for a pass.
    fn record_pass_yield(&self, lane: LaneId, pass: u8, candidates: usize, survivors: usize);
}

/// Everything a lane is given.
pub struct LaneInput<'a> {
    /// The effective configuration.
    pub config: &'a Config,
    /// The pull request under review.
    pub pull_request: &'a PullRequest,
    /// The parsed diffs, one per changed file.
    pub diffs: &'a [FileDiff],
    /// Head-revision content of the changed files, keyed by path.
    ///
    /// Optional evidence: a run with a checkout fills it, a forge-only run
    /// leaves it empty. It exists so `src/position` can fall back to the whole
    /// file when a finding quotes context the diff did not include; with it
    /// empty that stage is simply skipped.
    pub file_contents: &'a BTreeMap<String, String>,
    /// Findings the deterministic scanners already produced, for the lanes that
    /// adjudicate rather than re-discover.
    pub scan_findings: &'a [ScanFinding],
    /// The commits in the pull request's range, for the `commits` lane.
    pub commits: &'a [Commit],
    /// Curated repository policy: the pinned knowledge documents in scope,
    /// written by operators through the admin API. Cacheable prefix material.
    pub repo_policy: Option<&'a str>,
    /// Rules the sandboxed extraction pass read out of the repository's own
    /// instruction files. **Untrusted** — the pull request's author wrote them
    /// — so they land in the prompt's volatile suffix, fenced and labelled.
    pub extracted_rules: &'a [String],
    /// The diff already reviewed at the last reviewed SHA, replayed verbatim so
    /// the prompt prefix stays cacheable. Empty on a first review.
    pub reviewed_evidence: &'a str,
    /// Titles of findings raised in earlier cycles.
    pub prior_findings: &'a [String],
    /// Code retrieved from the index for this pull request: what the change
    /// resembles, and what it reaches.
    ///
    /// **Volatile.** It is composed from this diff, so it belongs in the
    /// prompt's suffix and nowhere near the cacheable prefix — see
    /// `crate::harness::prompt`. Empty when retrieval is off, degraded, or
    /// found nothing, in which case the lane reviews the diff alone.
    pub retrieved_context: &'a str,
    /// Typed state of the index used for this review. The critique lane uses
    /// this to keep unsupported unchanged-code claims from becoming
    /// actionable when retrieval could not establish repository context.
    pub retrieval_status: crate::retrieve::RetrievalStatus,
    /// One sentence saying a value was masked out of `diffs` before this lane
    /// ever saw it, from `crate::evidence::redact::Redactions::note`. Empty
    /// when nothing was redacted. Volatile and placed right after the diff
    /// in the suffix — see `crate::harness::prompt::PromptInputs::redaction_note`
    /// — because it describes *this* diff and must never touch the cacheable
    /// prefix.
    pub redaction_note: &'a str,
    /// What the `e2e` lane needs beyond the diff: the harness at head, the
    /// check runs on it, and candidate coverage. Gathered by
    /// `lanes::e2e::evidence::gather` only when that lane is enabled; every
    /// other lane ignores it, and the `e2e` lane skips without it.
    pub e2e: Option<&'a e2e::evidence::Evidence>,
    /// The reviewed tree, for a reviewer that wants to check before it
    /// answers — see `crate::flows::lookup`. `None` reviews the diff alone,
    /// which every offline golden test does.
    pub tree: Option<&'a dyn TreeReader>,
    /// The code-graph neighbourhood already walked for this pull request's
    /// changed files, when a graph is configured — the same walk
    /// `crate::retrieve::expand` and `crate::app::review::change_map` read
    /// edges from. `None` degrades `crate::lanes::grouping` to its name
    /// heuristics alone, which is what every offline golden test does and
    /// what a forge-only review without a graph store does too.
    pub graph: Option<&'a crate::index::types::Neighbourhood>,
    /// Optional terminal-review telemetry sink for completed adaptive passes.
    /// Optional sink for completed adaptive-pass yield totals. Set to `None`
    /// when the caller does not collect terminal review timing.
    ///
    /// External lane callers can leave it unset with struct update syntax:
    ///
    /// ```
    /// use tinysweeper::lanes::LaneInput;
    ///
    /// fn without_timing<'a>(input: LaneInput<'a>) -> LaneInput<'a> {
    ///     LaneInput { pass_yield_observer: None, ..input }
    /// }
    /// ```
    pub pass_yield_observer: Option<&'a dyn PassYieldObserver>,
}

impl<'a> LaneInput<'a> {
    /// How this lane's reviewers may follow up: questions when sub-agents are
    /// on, lookups when a tree was supplied and `[lookup]` allows them.
    pub fn asking(&self) -> Asking<'a> {
        Asking {
            subagent_model: self
                .config
                .council
                .subagents
                .then_some(self.config.models.flash.as_str()),
            tree: self.tree,
            lookup: Some(&self.config.lookup),
            seed: &[],
        }
    }

    /// [`Self::asking`], for a conversation about one file: the definitions
    /// its changed lines call into are fetched before the first turn.
    pub fn asking_about(&self, diff: &'a FileDiff) -> Asking<'a> {
        self.asking_about_group(std::slice::from_ref(diff))
    }

    /// [`Self::asking`], for a conversation about a group of related files:
    /// the definitions every file's changed lines call into are fetched
    /// before the first turn, so grouping a file with its test does not
    /// regress the single-file seeding win — see `docs/modules/lanes/lookup.md`.
    pub fn asking_about_group(&self, diffs: &'a [FileDiff]) -> Asking<'a> {
        Asking {
            seed: diffs,
            ..self.asking()
        }
    }

    /// Total lines this pull request added, across every file.
    pub fn additions(&self) -> usize {
        self.diffs.iter().map(FileDiff::additions).sum()
    }

    /// Whether any file has a reviewable diff at all.
    pub fn has_reviewable_content(&self) -> bool {
        self.preparation().has_reviewable_content()
    }

    /// Every changed path, for selecting the path rules that apply.
    pub fn changed_paths(&self) -> Vec<String> {
        self.preparation().changed_paths()
    }

    /// The scanner findings of the given kinds.
    ///
    /// Which lane adjudicates which kind is a partition, not an overlap: two
    /// lanes discussing the same scanner match is the double-reporting this
    /// whole design exists to avoid.
    pub fn scanner_findings_of(&self, kinds: &[ScanKind]) -> Vec<&ScanFinding> {
        self.preparation().scanner_findings_of(kinds)
    }

    /// Group `paths` deterministically under `bounds`, using this input's own
    /// diffs and graph neighbourhood — see `crate::lanes::grouping`.
    pub fn group(
        &self,
        paths: &[String],
        bounds: &crate::lanes::grouping::GroupBounds,
    ) -> Vec<crate::lanes::grouping::FileGroup> {
        self.preparation().group(paths, bounds)
    }

    /// Whether the pull request should be skipped as an unreviewed draft.
    pub fn skip_as_draft(&self) -> Option<LaneOutcome> {
        self.preparation().skip_as_draft()
    }

    /// Borrow the evidence needed to prepare a lane without model work.
    pub(crate) fn preparation(&self) -> crate::lanes::preparation::LanePreparationInput<'a> {
        crate::lanes::preparation::LanePreparationInput {
            config: self.config,
            pull_request: self.pull_request,
            diffs: self.diffs,
            scan_findings: self.scan_findings,
            reviewed_evidence: self.reviewed_evidence,
            graph: self.graph,
        }
    }
}

/// What a lane does with a finding it cannot anchor to a changed line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Anchoring {
    /// Drop it. For lanes whose subject matter is the code itself.
    Strict,
    /// Keep it without a line. For lanes whose subject matter — a commit
    /// message, a missing description — has no line to point at.
    Demote,
}

/// What a lane concluded.
#[derive(Debug, Clone, Default)]
pub struct LaneOutcome {
    /// One or two sentences for the check-run summary.
    pub summary: String,
    /// The model findings were reviewed without usable repository context.
    ///
    /// Only host lane code sets this bit; it is not part of any model schema.
    /// The application keeps such findings visible as labeled, non-blocking
    /// notes rather than treating their confidence as a repository-wide
    /// verdict.
    pub context_degraded: bool,
    /// Whether this lane produced a completed result, including deterministic
    /// policy checks that do not make a model call.
    pub completed: bool,
    /// The findings, before the shared filtering pipeline runs.
    pub findings: Vec<Finding>,
    /// Titles of earlier findings this revision fixed.
    pub resolved: Vec<String>,
    /// What the model calls cost, and which models actually answered.
    pub spend: Spend,
    /// Set when the lane did not apply to this pull request at all.
    pub skipped: Option<String>,
    /// Check runs this lane is still waiting on before it can conclude.
    ///
    /// Only the `e2e` lane sets it. A lane with something pending concludes
    /// `Neutral` rather than `Success` — a verdict on work that has not
    /// finished is the verdict branch protection must not see — and the
    /// server settles it when the named checks complete.
    pub pending: Vec<String>,
    /// What the lane was asked about and got no answer on.
    ///
    /// Paths for a per-file lane; the lane's own name for a whole-pull-request
    /// lane whose reviewer could not be consulted. Distinct from `skipped`,
    /// and the distinction decides a verdict: a lane with nothing to look at
    /// has nothing to object to, but a lane whose model never answered has
    /// nothing to *vouch for* either, and an approval is a claim about the
    /// change. A review that consulted no model once approved a pull request
    /// with "found nothing blocking · $0.0000 · 0 in / 0 out".
    pub unanswered: Vec<String>,
    /// Planned groups omitted by host-side execution policy, with their reason.
    pub omissions: Vec<fanout::OmittedGroup>,
    /// Per-file review coverage, when this lane fans out over changed files.
    pub coverage: Option<FileCoverage>,
}

/// Paths a per-file lane considered, split by whether it received a response.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FileCoverage {
    /// Files this lane reviewed.
    pub reviewed: Vec<String>,
    /// Files this lane could not review.
    pub unanswered: Vec<String>,
}

impl LaneOutcome {
    /// A lane that had nothing to do.
    pub fn skipped(reason: impl Into<String>) -> Self {
        let reason = reason.into();
        Self {
            summary: reason.clone(),
            skipped: Some(reason),
            ..Self::default()
        }
    }

    /// Add paths this lane can prove it reviewed without a model call.
    ///
    /// Used for unchanged replayed files and deterministic mechanical
    /// verification. A path with a current unanswered result stays uncovered;
    /// an earlier proof cannot hide a failed attempt on this run.
    pub fn include_reviewed_paths(&mut self, paths: impl IntoIterator<Item = String>) {
        for path in paths {
            let coverage = self.coverage.get_or_insert_with(FileCoverage::default);
            if !coverage.unanswered.contains(&path) && !coverage.reviewed.contains(&path) {
                coverage.reviewed.push(path);
            }
        }
    }

    /// A whole-pull-request lane whose reviewer could not be consulted.
    ///
    /// Neutral like a skip — no verdict is the truth — but it names itself
    /// as unanswered, so the proposal cannot read the silence as clean.
    pub fn unanswered(lane: LaneId, spend: Spend) -> Self {
        Self {
            summary: "No reviewer produced a usable response.".into(),
            spend,
            skipped: Some(
                "No reviewer produced a usable response; see the reviewer errors in the log."
                    .into(),
            ),
            unanswered: vec![lane.check_name()],
            ..Self::default()
        }
    }

    /// Turn a parsed model response into an outcome, applying `anchoring`.
    ///
    /// Every lane shares this so the anchoring rule cannot drift between them:
    /// a lane that quietly kept mis-anchored findings would post comments on
    /// code its pull request never touched, and nothing downstream re-checks.
    pub fn from_response(
        lane: LaneId,
        parsed: LaneResponse,
        diffs: &[FileDiff],
        anchoring: Anchoring,
        spend: Spend,
    ) -> Self {
        let mut findings = Vec::new();
        let mut discarded = 0usize;
        for raw in parsed.findings {
            // Structured responses anchor with a quoted snippet, not a line
            // number. Resolve that quote before strict anchoring so schema-
            // conforming findings are not all discarded as line-less.
            let range = raw.existing_code.as_deref().and_then(|snippet| {
                diffs
                    .iter()
                    .find(|diff| diff.path == raw.path)
                    .and_then(|diff| crate::position::locate(snippet, Some(diff), None).range())
            });
            let mut finding = raw.into_finding(lane);
            if let Some((start, end)) = range {
                finding.line = Some(start);
                finding.end_line = (end > start).then_some(end);
            }
            match anchoring {
                Anchoring::Strict if !anchor::anchored_in_diff(&finding, diffs) => {
                    // Not an error: models do this routinely. Dropped quietly
                    // and counted, so the count can surface in the summary if
                    // it ever gets large enough to mean something.
                    discarded += 1;
                }
                Anchoring::Strict => findings.push(finding),
                Anchoring::Demote => findings.push(anchor::demote_unanchored(finding, diffs)),
            }
        }

        let summary = if discarded > 0 {
            format!(
                "{} ({discarded} finding{} discarded for not matching a changed line)",
                parsed.summary.trim(),
                if discarded == 1 { "" } else { "s" }
            )
        } else {
            parsed.summary.trim().to_string()
        };

        Self {
            summary,
            context_degraded: false,
            completed: true,
            findings,
            resolved: parsed.resolved,
            spend,
            skipped: None,
            pending: Vec::new(),
            unanswered: Vec::new(),
            omissions: Vec::new(),
            coverage: None,
        }
    }

    /// The check-run conclusion for this outcome.
    ///
    /// `fail_on` comes from the lane's config. A skipped lane is `Neutral`
    /// rather than `Success`: claiming success for work that never happened
    /// would make branch protection meaningless.
    pub fn conclusion(&self, fail_on: Severity) -> CheckConclusion {
        if self.skipped.is_some() {
            return CheckConclusion::Neutral;
        }
        if self.findings.iter().any(|f| f.severity >= fail_on) {
            return CheckConclusion::Failure;
        }
        if !self.unanswered.is_empty() {
            return CheckConclusion::Neutral;
        }
        if !self.pending.is_empty() {
            return CheckConclusion::Neutral;
        }
        CheckConclusion::Success
    }
}

/// A whole-pull-request lane whose reviewer work stopped at a host budget
/// boundary. The changed paths remain explicitly uncovered; this does not
/// claim per-file coverage for a lane that reasons about the whole pull request.
pub(crate) fn stopped_by_execution_policy(
    lane: LaneId,
    paths: Vec<String>,
    spend: Spend,
    refusal: crate::ports::model_execution::ModelExecutionRefusal,
) -> Option<LaneOutcome> {
    use crate::lanes::fanout::{OmissionReason, OmittedGroup};
    use crate::ports::model_execution::ModelExecutionRefusal;

    let (reason, message) = match refusal {
        ModelExecutionRefusal::CallLimit => (
            OmissionReason::CallLimit,
            "Review stopped at the shared physical-call limit.",
        ),
        ModelExecutionRefusal::SoftDeadline => (
            OmissionReason::SoftDeadline,
            "Review stopped at the original soft deadline.",
        ),
        ModelExecutionRefusal::DrainDeadline => (
            OmissionReason::DrainDeadline,
            "Review stopped at the original drain deadline.",
        ),
        ModelExecutionRefusal::Cancelled => return None,
    };

    Some(LaneOutcome {
        summary: message.into(),
        spend,
        skipped: Some(message.into()),
        unanswered: vec![lane.check_name()],
        omissions: if paths.is_empty() {
            Vec::new()
        } else {
            vec![OmittedGroup { paths, reason }]
        },
        ..LaneOutcome::default()
    })
}

/// One response that was both received and valid for its lane's schema.
///
/// Failed calls and malformed responses are kept out of the returned list, so
/// the caller can decide whether an empty list fails one file or skips a
/// whole-pull-request lane.
pub struct ReviewerResponse {
    /// The configured reviewer that produced the response.
    pub id: String,
    /// The model selected after any fallback.
    pub model: String,
    /// The lane-shaped response, before anchoring or lane-specific placement.
    pub response: LaneResponse,
    /// What was read from the repository for this reviewer, if anything.
    pub looked_up: String,
}

/// Decode every usable council response, consistently across lanes.
///
/// A member failure never discards its peers. A malformed response is treated
/// like a failed member even for a solo reviewer; callers retain the policy
/// decision for the no-usable-response case and account for charged work.
pub fn reviewer_responses(
    lane: LaneId,
    reviewers: &[Reviewer<'_>],
    answers: &[Answer],
) -> Result<Vec<ReviewerResponse>> {
    let mut responses = Vec::with_capacity(reviewers.len());
    let mut execution_refusal = None;
    for (reviewer, answer) in reviewers.iter().zip(answers) {
        let Some(value) = answer.value.clone() else {
            if let Some(refusal) = answer.execution_refusal {
                execution_refusal = Some((
                    refusal,
                    answer.execution_usage.clone(),
                    answer.execution_accounting_complete.unwrap_or(false),
                ));
            }
            tracing::warn!(
                agent = reviewer.id,
                err = answer.error.as_deref().unwrap_or("no answer"),
                "a reviewer returned an unusable response"
            );
            continue;
        };

        let response = match crate::harness::schema::parse(lane, value) {
            Ok(response) => response,
            Err(err) => {
                tracing::warn!(agent = reviewer.id, %err, "a reviewer returned an unusable response");
                continue;
            }
        };

        responses.push(ReviewerResponse {
            id: reviewer.id.to_string(),
            model: answer.model.clone(),
            response,
            looked_up: answer.looked_up.clone(),
        });
    }
    if responses.is_empty()
        && let Some((refusal, usage, accounting_complete)) = execution_refusal
    {
        return Err(crate::error::Error::ModelExecutionRefused {
            refusal,
            usage,
            accounting_complete,
        });
    }
    Ok(responses)
}

/// Anchor and merge successful reviewer responses into one lane outcome.
///
/// This intentionally leaves the empty-response decision to its caller: a
/// per-file lane must surface an error, while a whole-pull-request lane must
/// return Neutral rather than claim a successful review.
pub fn aggregate_reviewer_responses(
    lane: LaneId,
    responses: Vec<ReviewerResponse>,
    diffs: &[FileDiff],
    anchoring: Anchoring,
    corroboration: bool,
) -> Option<LaneOutcome> {
    let mut per_reviewer = Vec::with_capacity(responses.len());
    let mut first = None;
    let mut spend = Spend::default();

    for response in responses {
        spend.note(&response.model);
        let anchored =
            LaneOutcome::from_response(lane, response.response, diffs, anchoring, Spend::default());
        per_reviewer.push(anchored.findings.clone());
        if first.is_none() {
            first = Some(anchored);
        }
    }

    let mut outcome = first?;
    outcome.findings = if corroboration {
        crate::council::merge(per_reviewer)
    } else {
        per_reviewer.into_iter().flatten().collect()
    };
    outcome.spend = spend;
    Some(outcome)
}

/// One reviewing lane.
#[async_trait]
pub trait Lane: Send + Sync {
    /// Which lane this is.
    fn id(&self) -> LaneId;

    /// Run it.
    async fn run(&self, input: LaneInput<'_>) -> Result<LaneOutcome>;
}

// Named for what it covers rather than `tests`: `lanes::tests` is already the
// tests *lane*, and the two module names would collide.
#[cfg(test)]
mod outcome_tests {
    use super::*;

    fn finding(severity: Severity) -> Finding {
        Finding {
            lane: LaneId::Critique,
            severity,
            confidence: 1.0,
            path: "src/lib.rs".into(),
            line: Some(1),
            end_line: None,
            rule: "r".into(),
            title: "t".into(),
            body: "b".into(),
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
    fn a_clean_lane_succeeds() {
        let outcome = LaneOutcome {
            summary: "Nothing to report.".into(),
            ..LaneOutcome::default()
        };
        assert_eq!(outcome.conclusion(Severity::High), CheckConclusion::Success);
    }

    #[test]
    fn a_finding_below_the_bar_does_not_fail_the_check() {
        let outcome = LaneOutcome {
            findings: vec![finding(Severity::Medium)],
            ..LaneOutcome::default()
        };
        assert_eq!(outcome.conclusion(Severity::High), CheckConclusion::Success);
    }

    #[test]
    fn a_finding_at_or_above_the_bar_fails_it() {
        let outcome = LaneOutcome {
            findings: vec![finding(Severity::High)],
            ..LaneOutcome::default()
        };
        assert_eq!(outcome.conclusion(Severity::High), CheckConclusion::Failure);
        assert_eq!(
            LaneOutcome {
                findings: vec![finding(Severity::Critical)],
                ..LaneOutcome::default()
            }
            .conclusion(Severity::High),
            CheckConclusion::Failure
        );
    }

    #[test]
    fn a_skipped_lane_is_neutral_not_successful() {
        // Reporting success for work that never happened would make a required
        // check meaningless.
        let outcome = LaneOutcome::skipped("no reviewable content");
        assert_eq!(outcome.conclusion(Severity::High), CheckConclusion::Neutral);
        assert!(!outcome.conclusion(Severity::High).blocks());
    }

    #[test]
    fn an_outcome_with_unanswered_files_is_neutral() {
        let outcome = LaneOutcome {
            unanswered: vec!["src/unread.rs".into()],
            ..LaneOutcome::default()
        };
        assert_eq!(outcome.conclusion(Severity::High), CheckConclusion::Neutral);
    }
}
