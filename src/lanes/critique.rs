//! The `critique` lane: correctness of the diff.
//!
//! The first lane, and the template for the rest. Its shape is the point:
//!
//! 1. Decide whether there is anything to review at all, before spending a
//!    token.
//! 2. Build the prompt in cache-friendly layers.
//! 3. Ask for structured output; refuse to parse prose.
//! 4. Place every finding by the code it quoted, not by a number it guessed.
//! 5. Drop the findings the diff disproves.
//!
//! Steps 4 and 5 are the noise control, and they pull in opposite directions on
//! purpose. Step 4 (`src/position`) exists because the old rule — the model
//! emits a line number, and anything outside the diff is dropped — threw away
//! good findings for bad arithmetic. Step 5 (`src/falsify`) exists because
//! keeping more findings is only an improvement if the wrong ones still go, and
//! it removes only what the diff *disproves*, never what it merely cannot
//! confirm.
//!
//! A finding that cannot be placed is no longer dropped. It loses its line and
//! is rendered into the check-run summary instead of posted inline, which is
//! the honest outcome: the review found something and could not say exactly
//! where.
//!
//! The lane fans out **one conversation per changed file**, like `security`.
//! It reviewed the whole pull request in a single call until a 31-file change
//! landed with two real correctness bugs in it and this lane reported one
//! hallucination: the failure `fanout`'s own module doc predicts, where the
//! first few files are read closely and the rest are an afterthought. The
//! subject here is one file's correctness, so nothing is lost by the split —
//! unlike `tests`, whose subject is the relationship *between* files and which
//! is deliberately still one conversation.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;

use crate::config::types::{Config, LaneId};
use crate::council;
use crate::error::Result;
use crate::evidence::diff::FileDiff;
use crate::evidence::replay;
use crate::falsify::{Falsifier, Rejection};
use crate::findings::types::Finding;
use crate::flows::panel::Call;
use crate::flows::runner;
use crate::harness::prompt::{self, PromptInputs};
use crate::harness::schema::{self, RawFinding};
use crate::lanes::fanout::{FileReview, per_unit};
use crate::lanes::grouping::{FileGroup, GroupBounds};
use crate::lanes::mechanical;
use crate::lanes::preparation::LanePreparationInput;
use crate::lanes::{Lane, LaneInput, LaneOutcome, reviewer_responses};
use crate::ports::model::{Model, Spend, Usage};
use crate::position::{PositionRequest, Positioner, Resolution, Unanchored};

/// Confidence ceiling for critique model findings when retrieval is degraded.
const DEGRADED_CONTEXT_CONFIDENCE_CAP: f64 = 0.7;

/// The correctness lane.
pub struct Critique {
    model: Arc<dyn Model>,
}

impl Critique {
    /// Build the lane over `model`.
    pub fn new(model: Arc<dyn Model>) -> Self {
        Self { model }
    }
}

/// The model-free part of preparing one critique lane.
pub(crate) struct CritiquePreparation {
    /// An early draft/content skip, when the lane has no candidate work.
    pub(crate) skipped: Option<LaneOutcome>,
    /// Groups in the exact order the existing critique runner reviews them.
    pub(crate) groups: Vec<FileGroup>,
    /// Paths covered without a current model response.
    pub(crate) covered_without_model: Vec<String>,
    /// A deterministic substitution proved by the changed lines, if any.
    pub(crate) mechanical: Option<mechanical::Substitution>,
    /// Whether every fresh path was verified mechanically.
    pub(crate) paths_empty: bool,
}

/// Prepare critique groups without constructing prompts or calling a model.
pub(crate) fn prepare(input: LanePreparationInput<'_>) -> CritiquePreparation {
    if !input.has_reviewable_content() {
        return CritiquePreparation {
            skipped: Some(LaneOutcome::skipped(
                "No added or modified lines to review.",
            )),
            groups: Vec::new(),
            covered_without_model: Vec::new(),
            mechanical: None,
            paths_empty: true,
        };
    }

    if let Some(skipped) = input.skip_as_draft() {
        return CritiquePreparation {
            skipped: Some(skipped),
            groups: Vec::new(),
            covered_without_model: Vec::new(),
            mechanical: None,
            paths_empty: true,
        };
    }

    let fresh: Vec<&FileDiff> = replay::unreviewed(input.reviewed_evidence, input.diffs)
        .into_iter()
        .filter(|diff| !diff.changed_lines.is_empty())
        .collect();
    let mut covered_without_model: Vec<String> = input
        .diffs
        .iter()
        .filter(|diff| {
            !diff.changed_lines.is_empty() && !fresh.iter().any(|fresh| fresh.path == diff.path)
        })
        .map(|diff| diff.path.clone())
        .collect();

    // A mechanical rename is verified, not read. Keep one verified file as a
    // sample: uniformity proves the same substitution, not that it is harmless.
    let mechanical = mechanical::detect(&fresh);
    let sample = mechanical
        .as_ref()
        .and_then(|substitution| substitution.verified.first().cloned());
    let paths: Vec<String> = fresh
        .iter()
        .filter(|diff| {
            mechanical.as_ref().is_none_or(|substitution| {
                !substitution.verified.contains(&diff.path) || Some(&diff.path) == sample.as_ref()
            })
        })
        .map(|diff| diff.path.clone())
        .collect();

    if let Some(substitution) = &mechanical {
        covered_without_model.extend(
            substitution
                .verified
                .iter()
                .filter(|path| Some(*path) != sample.as_ref())
                .cloned(),
        );
    }

    let groups = if input.config.grouping.enabled {
        input.group(
            &paths,
            &GroupBounds {
                max_files: input.config.grouping.max_files,
                max_hunk_chars: input.config.grouping.max_hunk_chars,
            },
        )
    } else {
        paths
            .iter()
            .map(|path| FileGroup {
                label: path.clone(),
                paths: vec![path.clone()],
            })
            .collect()
    };

    CritiquePreparation {
        skipped: None,
        groups,
        covered_without_model,
        mechanical,
        paths_empty: paths.is_empty(),
    }
}

#[async_trait]
impl Lane for Critique {
    fn id(&self) -> LaneId {
        LaneId::Critique
    }

    async fn run(&self, input: LaneInput<'_>) -> Result<LaneOutcome> {
        let prepared = prepare(input.preparation());
        self.run_prepared(input, prepared).await
    }
}

impl Critique {
    /// Review one planner-selected group, retaining its exact normal lane path.
    pub(crate) async fn review_selected_group(
        llm: Arc<crate::flows::caps::ModelCapability>,
        input: &LaneInput<'_>,
        group: &FileGroup,
    ) -> Result<FileReview> {
        let changed_paths = input.changed_paths();
        let cross_file_evidence = replay::CrossFileEvidenceIndex::new(input.diffs);
        let group_diffs: Vec<FileDiff> = group
            .paths
            .iter()
            .filter_map(|path| input.diffs.iter().find(|diff| &diff.path == path).cloned())
            .collect();
        let review = review_group(
            llm.clone(),
            input,
            &changed_paths,
            &group.paths,
            &group_diffs,
            &cross_file_evidence,
        )
        .await?;
        Ok(review)
    }

    /// Apply lane-wide postprocessing to selected group results.
    pub(crate) fn finish_selected(
        input: &LaneInput<'_>,
        prepared: CritiquePreparation,
        mut outcome: LaneOutcome,
        llm: Arc<crate::flows::caps::ModelCapability>,
    ) -> Result<LaneOutcome> {
        if let Some(skipped) = prepared.skipped {
            return Ok(skipped);
        }
        outcome.context_degraded = input.retrieval_status.is_degraded();
        outcome.spend.merge(llm.spend());
        outcome.include_reviewed_paths(prepared.covered_without_model);
        if let Some(substitution) = &prepared.mechanical {
            outcome.summary = format!(
                "{} {}",
                outcome.summary.trim(),
                mechanical::note(substitution)
            );
            if prepared.paths_empty {
                outcome.skipped = None;
            }
        }
        Ok(outcome)
    }

    /// Run the exact model-free candidate preparation already used by planning.
    pub(crate) async fn run_prepared(
        &self,
        input: LaneInput<'_>,
        prepared: CritiquePreparation,
    ) -> Result<LaneOutcome> {
        self.run_prepared_with_execution(input, prepared, None)
            .await
    }

    /// Run the prepared lane under its original review-wide stop window.
    pub(crate) async fn run_prepared_with_execution(
        &self,
        input: LaneInput<'_>,
        prepared: CritiquePreparation,
        execution: Option<Arc<crate::app::review_execution::ReviewExecution>>,
    ) -> Result<LaneOutcome> {
        if let Some(skipped) = prepared.skipped {
            return Ok(skipped);
        }
        let changed_paths = input.changed_paths();
        let groups = prepared.groups;
        let covered_without_model = prepared.covered_without_model;
        let mechanical = prepared.mechanical;
        let paths_empty = prepared.paths_empty;

        // One capability for the whole lane, so the pull-request budget is
        // enforced across every file and every reviewer at once. That is what
        // lets the groups run concurrently: this lane reviewed them one at a
        // time only because spend is known after a call returns, and there was
        // nowhere else to check it.
        let llm = runner::reviewer_lane_llm(
            self.model.clone(),
            input.config,
            input.config.models.budget_usd_per_pr,
        );
        // Links between changed files are indexed once, then each group's
        // falsifier gets only its own diff and mechanically related excerpts.
        let cross_file_evidence = replay::CrossFileEvidenceIndex::new(input.diffs);

        let review = |group: crate::lanes::grouping::FileGroup| {
            let llm = llm.clone();
            let input = &input;
            let changed_paths = &changed_paths;
            let cross_file_evidence = &cross_file_evidence;
            async move {
                let group_diffs: Vec<FileDiff> = group
                    .paths
                    .iter()
                    .filter_map(|path| input.diffs.iter().find(|d| &d.path == path).cloned())
                    .collect();
                review_group(
                    llm,
                    input,
                    changed_paths,
                    &group.paths,
                    &group_diffs,
                    cross_file_evidence,
                )
                .await
            }
        };
        let outcome = if let Some(execution) = execution {
            crate::lanes::fanout::per_unit_bounded(
                &groups,
                |group| group.label.clone(),
                |group| group.paths.clone(),
                review,
                execution,
            )
            .await
        } else {
            per_unit(
                &groups,
                |group| group.label.clone(),
                |group| group.paths.clone(),
                review,
            )
            .await
        };

        // The graph's own calls are tallied inside the capability, which is the
        // only object every one of them passes through. Folded in once here
        // rather than per file: `llm` is shared across the fan-out, so adding
        // it per file would multiply the bill by the file count.
        let mut outcome = outcome.into_outcome();
        outcome.context_degraded = input.retrieval_status.is_degraded();
        outcome.include_reviewed_paths(covered_without_model);
        outcome.spend.merge(llm.spend());
        if let Some(sub) = &mechanical {
            outcome.summary = format!("{} {}", outcome.summary.trim(), mechanical::note(sub));
            // Every file was the rename: a real verdict, reached without a
            // model, and the outcome must say so rather than read as skipped.
            if paths_empty {
                outcome.skipped = None;
            }
        }
        Ok(outcome)
    }
}

/// Review one group of related changed files, in a conversation that knows
/// about no file outside it.
///
/// `group_paths` and `group_diffs` are the same files in the same order;
/// kept apart because a finding is placed against the one `FileDiff` whose
/// path it names (see [`place`]), while the prompt layer wants the plain
/// path list. A group of one file is the pre-grouping case, byte-identical to
/// it: one path, one diff, the same isolation clause text.
///
/// Positioning (step 4) and falsification (step 5) both run here rather than
/// once over the folded result, because both want *the evidence this
/// conversation was shown* and that is now this group's diffs.
/// Falsification is also free for the common case of nothing to report:
/// `Falsifier::filter` makes no call when there is nothing to filter, so the
/// number of falsify calls is the number of groups that actually produced a
/// finding.
async fn review_group(
    llm: std::sync::Arc<crate::flows::caps::ModelCapability>,
    input: &LaneInput<'_>,
    changed_paths: &[String],
    group_paths: &[String],
    group_diffs: &[FileDiff],
    cross_file_evidence: &replay::CrossFileEvidenceIndex<'_>,
) -> Result<FileReview> {
    let config: &Config = input.config;
    let evidence = replay::render(group_diffs);
    // Findings are confined to this group's paths, but contradictions can
    // live in a mechanically related PR file (for example, a module added
    // beside its declaration). The shared index keeps this bounded without
    // replaying the complete PR diff for every group.
    let falsifier_evidence = cross_file_evidence.render_for(group_diffs);
    let reviewers = council::reviewers(config, LaneId::Critique);
    let redaction_note = redaction_note(&evidence);

    // Every reviewer at once, as one concurrent round. `ask_all` returns one answer per
    // reviewer in the order asked, and reports a reviewer it could not reach
    // rather than failing the council for it.
    let calls: Vec<Call> = reviewers
        .iter()
        .map(|reviewer| {
            let inputs = prompt_inputs(
                input,
                changed_paths,
                group_paths,
                &evidence,
                reviewer,
                &redaction_note,
            );
            let built = prompt::build(&inputs);
            Call {
                id: reviewer.id.to_string(),
                model: reviewer.model.to_string(),
                system: built.prefix().to_string(),
                prompt: built.suffix().to_string(),
                schema_name: "tinysweeper_critique".into(),
            }
        })
        .collect();

    let round_one = runner::ask_all_accounted(
        llm.clone(),
        LaneId::Critique,
        &calls,
        &schema::json_schema(),
        input.asking_about_group(group_diffs),
    )
    .await?;
    let round_one_usage = round_one.usage;
    let round_one_elapsed = round_one.elapsed;
    let answers = round_one.answers;

    let responses = reviewer_responses(LaneId::Critique, &reviewers, &answers)?;
    let mut spend = Spend::default();
    let mut per_reviewer: Vec<Vec<Finding>> = Vec::with_capacity(reviewers.len());
    let mut summary = String::new();
    let mut resolved: Vec<String> = Vec::new();
    let mut unanchored = 0usize;
    let mut discarded = 0usize;
    // What the reviewers read, for the falsifier: a finding about a callee's
    // contract cannot be judged against the diff alone, and was being
    // rejected on the strength of the diff's own comment about it.
    let mut looked_up = String::new();

    for response in responses {
        spend.note(&response.model);
        looked_up.push_str(&response.looked_up);

        let asked = match place(
            llm.clone(),
            input,
            group_diffs,
            &evidence,
            response.response,
        )
        .await
        {
            Ok(asked) => asked,
            Err(err) if reviewers.len() > 1 => {
                tracing::warn!(agent = response.id, %err, "a council reviewer failed");
                continue;
            }
            Err(err) => return Err(err),
        };

        spend.merge(asked.spend);
        unanchored += asked.unanchored;
        discarded += asked.discarded;
        // The first reviewer's prose, taken whole. Blending N summaries would
        // author text no reviewer wrote, which is the objection `src/falsify`
        // raises to a filter that can return findings of its own.
        if summary.is_empty() {
            summary = asked.summary;
            resolved = asked.resolved;
        }
        per_reviewer.push(asked.findings);
    }

    if per_reviewer.is_empty() {
        return Err(crate::error::Error::lane(
            "critique",
            format!("every reviewer failed on {}", group_paths.join(" + ")),
        ));
    }

    // Corroboration is a separate switch from the council itself, so the merge
    // can be measured before a second agent is what is being judged. Off, the
    // findings are concatenated exactly as the reviewers produced them.
    let findings: Vec<Finding> = if config.council.corroboration {
        council::merge(per_reviewer)
    } else {
        per_reviewer.into_iter().flatten().collect()
    };
    let round_one_candidates = findings.len();

    // Step 5, once over the merged set rather than once per reviewer. The
    // filter can only reject, so more inputs in one pass is identical semantics
    // at a fraction of the calls.
    let filtered = Falsifier::new(llm.model().as_ref(), config)
        .filter_with(LaneId::Critique, findings, &falsifier_evidence, &looked_up)
        .await;
    spend.merge(filtered.spend);

    let mut findings = filtered.findings;
    let round_one_survivors = findings.len();
    let mut execution_refusal = filtered.execution_refusal;
    let mut rejected = filtered.rejected;
    let mut capped_for_degraded_context = 0usize;
    let mut metrics = crate::lanes::coverage::Metrics::start(
        round_one_usage,
        round_one_elapsed,
        round_one_candidates,
        round_one_survivors,
    );
    if execution_refusal.is_none()
        && let Some(observer) = input.pass_yield_observer
    {
        observer.record_pass_yield(
            LaneId::Critique,
            1,
            round_one_candidates,
            round_one_survivors,
        );
    }

    // Candidate yield and deterministic high-risk files trigger an extra look;
    // unrelated large files in the pull request do not affect this group.
    let mut added_by_coverage = 0usize;
    if execution_refusal.is_none()
        && config.review.passes > 1
        && (round_one_candidates > 0 || group_diffs.iter().any(crate::lanes::triage::high_risk))
    {
        // Fed cumulatively: the second coverage pass (pass 3) is told about
        // everything round one *and* the first coverage pass found, so it
        // does not rediscover the first pass's own additions.
        let mut confirmed = findings.clone();

        for pass_index in 1..config.review.passes {
            let reviewer = &reviewers[0];
            let confirmed_lines = crate::lanes::coverage::confirmed_lines(&confirmed);
            let base_inputs = prompt_inputs(
                input,
                changed_paths,
                group_paths,
                &evidence,
                reviewer,
                &redaction_note,
            );
            let built = prompt::build(&PromptInputs {
                confirmed_this_round: &confirmed_lines,
                coverage_pass: true,
                ..base_inputs
            });

            let coverage = match crate::lanes::coverage::coverage_pass(
                llm.clone(),
                LaneId::Critique,
                reviewer,
                &built,
                &schema::json_schema(),
                "tinysweeper_critique",
                input.asking_about_group(group_diffs),
            )
            .await
            {
                Ok(coverage) => coverage,
                Err(err) => {
                    let refusal = match &err {
                        crate::error::Error::ModelExecutionRefused { refusal, .. } => {
                            Some(*refusal)
                        }
                        _ => None,
                    };
                    if refusal
                        == Some(crate::ports::model_execution::ModelExecutionRefusal::Cancelled)
                    {
                        return Err(err);
                    }
                    if let Some(refusal) = refusal {
                        execution_refusal = Some(refusal);
                        tracing::info!(?refusal, "adaptive review pass stopped by host policy");
                    } else {
                        tracing::warn!(%err, "an adaptive review pass failed");
                    }
                    // No per-call usage is observable when the round itself
                    // fails to return an outcome; count the attempt without
                    // attributing concurrent groups' shared lane spend to it.
                    metrics.record(Usage::default(), Duration::ZERO, 0, 0);
                    metrics.stop(if refusal.is_some() {
                        crate::lanes::coverage::StopReason::Refused
                    } else {
                        crate::lanes::coverage::StopReason::Failed
                    });
                    break;
                }
            };
            let coverage_usage = coverage.usage;
            let coverage_elapsed = coverage.elapsed;
            spend.merge(coverage.spend);
            looked_up.push_str(&coverage.looked_up);

            if let Some(refusal) = coverage.execution_refusal {
                execution_refusal = Some(refusal);
                metrics.record(coverage_usage, coverage_elapsed, 0, 0);
                metrics.stop(crate::lanes::coverage::StopReason::Refused);
                break;
            }

            // No answer this round: nothing new to place, and a round that
            // could not be reached is not evidence a further one would fare
            // better, so stop rather than pay for another.
            let Some(response) = coverage.response else {
                metrics.record(coverage_usage, coverage_elapsed, 0, 0);
                metrics.stop(crate::lanes::coverage::StopReason::Failed);
                break;
            };

            // A coverage response that quotes more than `place` can relocate
            // within its budget fails placement even though parsing already
            // succeeded, which the malformed-response handling above does not
            // cover. This pass is optional on top of round one, so a failure
            // here is no additional coverage result, not a reason to discard
            // every finding round one already produced and falsified.
            let asked = match place(llm.clone(), input, group_diffs, &evidence, response).await {
                Ok(asked) => asked,
                Err(err) => {
                    tracing::warn!(%err, "a coverage pass failed to place its findings");
                    metrics.record(coverage_usage, coverage_elapsed, 0, 0);
                    metrics.stop(crate::lanes::coverage::StopReason::PlacementFailure);
                    break;
                }
            };
            spend.merge(asked.spend);
            unanchored += asked.unanchored;
            discarded += asked.discarded;

            // Drop anything that is really a round-one finding said again.
            // `corroborates` catches the common paraphrase on the same lines;
            // the fingerprint catches an exact repeat the reviewer quoted
            // differently. Both are computed here rather than trusted from
            // the model, which has no channel to report "this is the same
            // one" and no reason to be honest about it if it did.
            let had_proposals = !asked.findings.is_empty();
            let new_findings: Vec<Finding> = asked
                .findings
                .into_iter()
                .filter(|candidate| {
                    let candidate_fp = candidate.fingerprint(
                        &crate::findings::anchor::anchor_context(candidate, group_diffs),
                    );
                    !confirmed.iter().any(|prior| {
                        council::agree::corroborates(candidate, prior)
                            || candidate_fp
                                == prior.fingerprint(&crate::findings::anchor::anchor_context(
                                    prior,
                                    group_diffs,
                                ))
                    })
                })
                .collect();

            // Nothing new: a further pass over the same evidence would not
            // find more either, so stop rather than pay for one.
            if new_findings.is_empty() {
                if let Some(observer) = input.pass_yield_observer {
                    observer.record_pass_yield(LaneId::Critique, pass_index + 1, 0, 0);
                }
                metrics.record(coverage_usage, coverage_elapsed, 0, 0);
                metrics.stop(if had_proposals {
                    crate::lanes::coverage::StopReason::Duplicate
                } else {
                    crate::lanes::coverage::StopReason::Empty
                });
                break;
            }

            // Falsify only what this pass added — free when empty, and it
            // never re-judges what round one's own pass already kept.
            let new_candidates = new_findings.len();
            let mut new_filtered = Falsifier::new(llm.model().as_ref(), config)
                .filter_with(
                    LaneId::Critique,
                    new_findings,
                    &falsifier_evidence,
                    &looked_up,
                )
                .await;
            for finding in &mut new_filtered.findings {
                finding.review_pass = pass_index + 1;
            }
            spend.merge(new_filtered.spend);
            rejected.extend(new_filtered.rejected);
            if let Some(refusal) = new_filtered.execution_refusal {
                execution_refusal = Some(refusal);
                let survivors = new_filtered.findings.len();
                findings.extend(new_filtered.findings);
                metrics.record(coverage_usage, coverage_elapsed, new_candidates, survivors);
                metrics.stop(crate::lanes::coverage::StopReason::Failed);
                break;
            }

            let survivors = new_filtered.findings.len();
            if let Some(observer) = input.pass_yield_observer {
                observer.record_pass_yield(
                    LaneId::Critique,
                    pass_index + 1,
                    new_candidates,
                    survivors,
                );
            }

            if new_filtered.findings.is_empty() {
                metrics.record(coverage_usage, coverage_elapsed, new_candidates, 0);
                metrics.stop(crate::lanes::coverage::StopReason::NonSurviving);
                break;
            }

            metrics.record(
                coverage_usage,
                coverage_elapsed,
                new_candidates,
                new_filtered.findings.len(),
            );
            added_by_coverage += new_filtered.findings.len();
            confirmed.extend(new_filtered.findings.clone());
            findings.extend(new_filtered.findings);
        }
    }
    // Emit one yield record for every reviewed group, including one-pass and
    // adaptively ineligible groups, so zero yield remains observable.
    metrics.emit(LaneId::Critique, group_paths);

    // The application's typed context-degraded path keeps these findings as
    // non-blocking structured notes. A Likely cap makes that caveat visible
    // in their confidence badge without allowing unsupported repository-wide
    // claims to drive a check verdict or inline comment. Scanners have their
    // own evidence and are outside this model-finding gate.
    if input.retrieval_status.is_degraded() {
        capped_for_degraded_context = findings.len();
        for finding in &mut findings {
            finding.confidence = finding.confidence.min(DEGRADED_CONTEXT_CONFIDENCE_CAP);
        }
    }

    Ok(FileReview {
        summary: summarise(
            summary.trim(),
            unanchored,
            discarded,
            &rejected,
            &findings,
            added_by_coverage,
            capped_for_degraded_context,
        ),
        findings,
        resolved,
        spend,
        execution_refusal,
    })
}

/// What one reviewer said about one group.
struct Asked {
    summary: String,
    resolved: Vec<String>,
    findings: Vec<Finding>,
    spend: Spend,
    unanchored: usize,
    discarded: usize,
}

/// Build one reviewer's prompt inputs for one group.
///
/// Round one and adaptive passes both start from this complete context, then
/// adaptive review changes only its coverage-specific fields. Keeping the
/// shared layers here prevents later passes from silently losing policy or
/// evidence when a new prompt input is added.
fn prompt_inputs<'a>(
    input: &'a LaneInput<'_>,
    changed_paths: &'a [String],
    group_paths: &'a [String],
    evidence: &'a str,
    reviewer: &council::Reviewer<'_>,
    redaction_note: &'a str,
) -> PromptInputs<'a> {
    let config: &Config = input.config;
    PromptInputs {
        repo_policy: input.repo_policy,
        extracted_rules: input.extracted_rules,
        prior_findings: input.prior_findings,
        new_evidence: evidence,
        // Every path the pull request touched, not just this group's.
        // `path_instructions` always selects repository overrides from
        // `changed_paths`, never from `focus_paths` below: a path-specific
        // rule for a file outside this group is still a rule about a file the
        // pull request touched, and a grouped conversation must see it even
        // though it may only report findings inside its own group.
        changed_paths,
        focus_paths: group_paths,
        persona: reviewer.persona,
        retrieved_context: input.retrieved_context,
        retrieval_status: Some(&input.retrieval_status),
        redaction_note,
        ..PromptInputs::new(LaneId::Critique, config)
    }
}

/// Explain the redaction markers present in one group's rendered evidence.
fn redaction_note(evidence: &str) -> String {
    // Critique prompts see only this group's rendered diffs. Derive the note
    // from that evidence so a clean group is never told about a credential
    // removed from a different group's file. Only complete markers emitted by
    // `scan::redact` count; diff text may contain the prefix literally.
    let redacted = evidence
        .split("<redacted, ")
        .skip(1)
        .filter(|suffix| {
            suffix.split_once(" chars>").is_some_and(|(count, _)| {
                !count.is_empty() && count.bytes().all(|b| b.is_ascii_digit())
            })
        })
        .count();
    if redacted == 0 {
        "".to_string()
    } else {
        let value = if redacted == 1 { "value" } else { "values" };
        format!(
            "{redacted} credential {value} were removed from this diff before you saw it and appear as `<redacted, N chars>`; the lines are real, only the values are gone — never ask for or guess them."
        )
    }
}

/// Place what one reviewer said against the group file it names.
///
/// Resolved against the `FileDiff` in `group_diffs` whose path equals the
/// finding's own `path` — never the first file of the group. A path outside
/// the group is discarded exactly like a file the pull request never touched:
/// the isolation clause told this conversation it owns only these files, and
/// honouring a finding about anything else is what `focus_paths` exists to
/// prevent (see `harness::prompt::isolation_clause`).
async fn place(
    llm: std::sync::Arc<crate::flows::caps::ModelCapability>,
    input: &LaneInput<'_>,
    group_diffs: &[FileDiff],
    evidence: &str,
    parsed: schema::LaneResponse,
) -> Result<Asked> {
    let config: &Config = input.config;

    // The call's own cost is already tallied inside the capability; what is
    // counted here is only what *placement* adds, which is a relocation call
    // per finding the quote could not anchor.
    let mut spend = Spend::default();
    let model = llm.model().as_ref();
    let positioner = Positioner::new(model, config);
    let mut findings = Vec::new();
    let mut unanchored = 0usize;
    let mut discarded = 0usize;

    for raw in parsed.findings {
        // Resolved against the group file whose path it names. A path outside
        // the group — including one this pull request touched, in a different
        // conversation — is dropped exactly as a whole-diff lane would drop a
        // path it never touched. That is stricter than "the pull request
        // touched this somewhere", and it has to be: N reviewers each
        // reporting the same cross-file problem is what `focus_paths` exists
        // to prevent, and honouring an off-group finding here would undo it.
        let Some(diff) = group_diffs.iter().find(|d| d.path == raw.path) else {
            discarded += 1;
            continue;
        };

        // Budget check: relocation can make one model call per unresolvable
        // finding, so enforce the limit inside the loop before escalating to
        // stage 3. Do not wait until the lane finishes.
        if spend.cost_usd() > config.models.budget_usd_per_pr {
            return Err(crate::error::Error::Budget {
                spent: spend.cost_usd(),
                limit: config.models.budget_usd_per_pr,
                usage: None,
            });
        }

        let comment = format!("{}\n\n{}", raw.title, raw.body);
        let snippet = raw.existing_code.clone().unwrap_or_default();
        let resolution = positioner
            .resolve(
                PositionRequest {
                    snippet: &snippet,
                    diff: Some(diff),
                    file: input.file_contents.get(&raw.path).map(String::as_str),
                    comment: &comment,
                    rendered_diff: evidence,
                },
                &mut spend,
            )
            .await;

        let range = postable_range(&raw, diff, resolution);
        if range.is_none() {
            unanchored += 1;
        }

        let mut finding = raw.into_finding(LaneId::Critique);
        finding.line = range.map(|(start, _)| start);
        finding.end_line = range.and_then(|(start, end)| (end > start).then_some(end));

        // Postability is wider than the changed-line set, so a finding can
        // now land on a context line the pull request never touched. That
        // is a deliberate widening, but it must not be a silent one: the
        // noise rule is "introduced by this pull request", and a reader has
        // to be able to tell when a finding is not. Marking it `late` is
        // what puts the pre-existing badge on it in the summary.
        if let Some((start, end)) = range
            && !diff.touches_range(start, end)
        {
            finding.late = true;
        }

        findings.push(finding);
    }

    // Falsification is deliberately *not* here: it runs once over the merged
    // set in `review_group`, because a reject-only filter given more inputs in
    // one pass has identical semantics at a fraction of the calls.
    Ok(Asked {
        summary: parsed.summary,
        resolved: parsed.resolved,
        findings,
        spend,
        unanchored,
        discarded,
    })
}

/// The head-revision range a finding may be posted against, if any.
///
/// One rule: the range has to be inside a hunk, because that is exactly what
/// GitHub will accept an inline comment on. That is deliberately wider than
/// *the lines this pull request changed* — a finding that quotes a context
/// line inside the hunk is about the change too, and the quotation is evidence
/// the model really did read that line rather than guess at it. It is also
/// deliberately narrower than *anywhere in the file*: a finding that resolved
/// through the whole-file fallback to code the diff never showed cannot be
/// posted inline, so it goes in the summary rather than being thrown away.
fn postable_range(raw: &RawFinding, diff: &FileDiff, resolution: Resolution) -> Option<(u64, u64)> {
    let (start, end) = match resolution {
        Resolution::Anchored(anchor) => (anchor.start, anchor.end),
        // The migration path: a model still answering with the old schema gets
        // its line number honoured, because there is no quotation to place and
        // its number is better than nothing.
        Resolution::Unanchored(Unanchored::NoSnippet) => {
            let line = raw.line?;
            (line, raw.end_line.unwrap_or(line))
        }
        Resolution::Unanchored(Unanchored::NoMatch) => return None,
    };

    diff.within_hunk(start, end).then_some((start, end))
}

/// Fold the bookkeeping into the model's own summary.
///
/// Every count here is a finding that did not become an inline comment. They
/// are stated rather than hidden: a filter nobody can see the effect of is a
/// filter nobody can tell is broken.
///
/// The original prose predates falsification and cannot be trusted once any
/// finding is disproven, even when another finding survives. Replace it with
/// the surviving titles, or the rejection reasons when none survive. This
/// preserves observations without asking the model to rewrite its verdict.
///
/// Adaptive coverage is also newer than the original prose. Its additions are
/// counted explicitly, so a group initially called clean cannot silently keep
/// declaring itself clean after a coverage pass adds a finding.
fn summarise(
    summary: &str,
    unanchored: usize,
    discarded: usize,
    rejected: &[Rejection],
    kept: &[Finding],
    added_by_coverage: usize,
    capped_for_degraded_context: usize,
) -> String {
    let summary = if capped_for_degraded_context > 0 {
        let titles = kept
            .iter()
            .map(|finding| finding.title.trim())
            .collect::<Vec<_>>()
            .join("; ");
        if titles.is_empty() {
            "No findings were reported. Retrieval was degraded, so model confidence was capped at Likely."
                .to_string()
        } else {
            format!(
                "Critique findings were capped at Likely confidence because retrieval was degraded: {titles}."
            )
        }
    } else if kept.is_empty() && !rejected.is_empty() {
        let reasons: Vec<String> = rejected
            .iter()
            .map(|item| format!("{} — {}", item.title.trim(), item.reason.trim()))
            .collect();
        format!(
            "Nothing to report. {} finding{} raised and dropped as disproved by the diff: {}.",
            rejected.len(),
            plural(rejected.len()),
            reasons.join("; ")
        )
    } else if rejected.is_empty() {
        summary.to_string()
    } else {
        let titles = kept
            .iter()
            .map(|finding| finding.title.trim())
            .collect::<Vec<_>>()
            .join("; ");
        if titles.is_empty() {
            "Nothing to report.".to_string()
        } else {
            format!("Surviving findings: {titles}.")
        }
    };
    let rejected = rejected.len();
    let mut notes = Vec::new();
    if unanchored > 0 {
        notes.push(format!(
            "{unanchored} finding{} could not be anchored to a line",
            plural(unanchored)
        ));
    }
    if discarded > 0 {
        notes.push(format!(
            "{discarded} finding{} discarded for naming a file this pull request did not change",
            plural(discarded)
        ));
    }
    if rejected > 0 {
        notes.push(format!(
            "{rejected} finding{} dropped as disproved by the diff",
            plural(rejected)
        ));
    }
    if added_by_coverage > 0 {
        notes.push(format!(
            "{added_by_coverage} finding{} added by a second pass",
            plural(added_by_coverage)
        ));
    }
    if capped_for_degraded_context > 0 {
        notes.push(format!(
            "{capped_for_degraded_context} model finding{} retained at no more than Likely confidence because retrieval was degraded",
            plural(capped_for_degraded_context)
        ));
    }

    if notes.is_empty() {
        return summary;
    }
    format!("{summary} ({})", notes.join("; "))
}

fn plural(count: usize) -> &'static str {
    if count == 1 { "" } else { "s" }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::types::{Config, PathInstruction, Severity};
    use crate::evidence::diff::parse_file_patch;
    use crate::forge::types::CheckConclusion;
    use crate::forge::types::PullRequest;
    use crate::harness::mock::MockModel;
    use crate::ports::tree::{Found, Lookup};
    use serde_json::json;
    use std::collections::BTreeMap;

    #[derive(Clone)]
    struct MetricsLogWriter(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for MetricsLogWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[derive(Debug)]
    struct CountingTree {
        inner: crate::ports::tree::MockTree,
        lookups: std::sync::Mutex<Vec<Lookup>>,
    }

    #[async_trait::async_trait]
    impl crate::ports::tree::TreeReader for CountingTree {
        async fn lookup(&self, lookup: &Lookup) -> crate::error::Result<Found> {
            self.lookups
                .lock()
                .expect("lookup calls")
                .push(lookup.clone());
            self.inner.lookup(lookup).await
        }

        fn describe(&self) -> String {
            "counting mock tree".into()
        }
    }

    fn config() -> Config {
        let mut config: Config = crate::config::DEFAULTS
            .parse::<toml::Table>()
            .unwrap()
            .try_into()
            .unwrap();
        // Most tests exercise one review turn. Adaptive-pass tests opt in
        // explicitly through `config_with_passes`.
        config.review.passes = 1;
        config
    }

    const PATCH: &str =
        "@@ -1,3 +1,5 @@\n fn main() {\n+    let x = items[i];\n+    println!(\"{x}\");\n }\n";

    /// The head revision of the same file. Lines 6–8 are outside every hunk,
    /// so only the whole-file fallback can reach them.
    const FILE: &str = "\
fn main() {
    let x = items[i];
    println!(\"{x}\");
}

fn helper() {
    let cfg = load();
}
";

    fn diffs() -> Vec<FileDiff> {
        vec![parse_file_patch("src/main.rs", PATCH)]
    }

    fn pull_request() -> PullRequest {
        PullRequest {
            number: 7,
            title: "feat: index items".into(),
            head_sha: "abc123".into(),
            ..PullRequest::default()
        }
    }

    async fn run_with(model: MockModel, config: &Config, diffs: &[FileDiff]) -> LaneOutcome {
        run_with_files(model, config, diffs, &BTreeMap::new()).await
    }

    async fn run_with_files(
        model: MockModel,
        config: &Config,
        diffs: &[FileDiff],
        file_contents: &BTreeMap<String, String>,
    ) -> LaneOutcome {
        run_with_context(
            model,
            config,
            diffs,
            file_contents,
            crate::retrieve::RetrievalStatus::Off,
        )
        .await
    }

    async fn run_with_context(
        model: MockModel,
        config: &Config,
        diffs: &[FileDiff],
        file_contents: &BTreeMap<String, String>,
        retrieval_status: crate::retrieve::RetrievalStatus,
    ) -> LaneOutcome {
        run_with_context_and_tree(model, config, diffs, file_contents, retrieval_status, None).await
    }

    async fn run_with_context_and_tree(
        model: MockModel,
        config: &Config,
        diffs: &[FileDiff],
        file_contents: &BTreeMap<String, String>,
        retrieval_status: crate::retrieve::RetrievalStatus,
        tree: Option<&dyn crate::ports::tree::TreeReader>,
    ) -> LaneOutcome {
        let pr = pull_request();
        Critique::new(Arc::new(model))
            .run(LaneInput {
                config,
                pull_request: &pr,
                diffs,
                file_contents,
                scan_findings: &[],
                commits: &[],
                repo_policy: None,
                extracted_rules: &[],
                reviewed_evidence: "",
                prior_findings: &[],
                retrieval_status,
                retrieved_context: "",
                redaction_note: "",
                e2e: None,
                tree,
                graph: None,
                pass_yield_observer: None,
            })
            .await
            .expect("lane runs")
    }

    async fn run_with_prior(
        model: MockModel,
        config: &Config,
        diffs: &[FileDiff],
        prior: &[String],
    ) {
        let pr = pull_request();
        Critique::new(Arc::new(model))
            .run(LaneInput {
                config,
                pull_request: &pr,
                diffs,
                file_contents: &BTreeMap::new(),
                scan_findings: &[],
                commits: &[],
                repo_policy: None,
                extracted_rules: &[],
                reviewed_evidence: "",
                prior_findings: prior,
                retrieval_status: crate::retrieve::RetrievalStatus::Off,
                retrieved_context: "",
                redaction_note: "",
                e2e: None,
                tree: None,
                graph: None,
                pass_yield_observer: None,
            })
            .await
            .expect("lane runs");
    }

    /// A finding anchored the way the schema now asks for: by quotation.
    fn finding_quoting(snippet: &str) -> serde_json::Value {
        json!({
            "path": "src/main.rs",
            "existing_code": snippet,
            "rule": "unchecked-index",
            "title": "Guard the index before dereferencing",
            "body": "`i` is never bounds-checked.",
            "severity": "high",
            "confidence": 0.9
        })
    }

    #[tokio::test]
    async fn a_degraded_index_keeps_unsupported_unchanged_code_at_likely_confidence() {
        let config = config();
        let diffs = vec![parse_file_patch(
            "src/main.rs",
            "@@ -1,2 +1,3 @@\n fn main() {\n+    let ready = true;\n }\n",
        )];
        let response = json!({
            "summary": "The old branch is broken.",
            "findings": [{
                "path": "src/main.rs",
                "existing_code": "fn main() {",
                "rule": "bad-caller",
                "title": "Handle the old caller",
                "body": "The unchanged caller uses an incompatible contract.",
                "severity": "critical",
                "confidence": 0.99
            }]
        });
        let model = MockModel::new()
            .then(response)
            .then(json!({"incorrect": []}));

        let outcome = run_with_context(
            model,
            &config,
            &diffs,
            &BTreeMap::new(),
            crate::retrieve::RetrievalStatus::Cold,
        )
        .await;

        assert_eq!(outcome.findings.len(), 1, "{outcome:?}");
        assert_eq!(outcome.findings[0].severity, Severity::Critical);
        assert_eq!(
            outcome.findings[0].confidence,
            DEGRADED_CONTEXT_CONFIDENCE_CAP
        );
        assert!(outcome.summary.contains("capped at Likely confidence"));
        assert!(!outcome.summary.contains("old branch is broken"));
    }

    #[tokio::test]
    async fn disputed_prior_finding_reaches_the_next_model_request_as_disputed() {
        let config = config();
        let model = MockModel::new().then(json!({
            "summary": "Nothing to report.",
            "findings": []
        }));
        let handle = model.clone();
        let prior = [
            "[disputed by maintainer; verify against current-head evidence] high — Close the socket on the error path"
                .to_string(),
        ];

        run_with_prior(model, &config, &diffs(), &prior).await;

        let request = handle.requests().pop().expect("critique model request");
        let prompt = request
            .messages
            .iter()
            .map(|message| message.content.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(prompt.contains("[disputed by maintainer"));
        assert!(prompt.contains("the earlier claim is not evidence"));
        assert!(prompt.contains("independently supports the concern"));
    }

    #[tokio::test]
    async fn degraded_retrieval_keeps_findings_grounded_on_changed_lines() {
        let config = config();
        let diffs = vec![parse_file_patch(
            "src/main.rs",
            "@@ -1,2 +1,3 @@\n fn main() {\n+    let ready = true;\n }\n",
        )];
        let model = MockModel::new()
            .then(json!({
                "summary": "The new value is unused.",
                "findings": [{
                    "path": "src/main.rs",
                    "existing_code": "    let ready = true;",
                    "rule": "unused-value",
                    "title": "Use the ready value",
                    "body": "The changed value is never read.",
                    "severity": "high",
                    "confidence": 0.9
                }]
            }))
            .then(json!({"incorrect": []}));

        let outcome = run_with_context(
            model,
            &config,
            &diffs,
            &BTreeMap::new(),
            crate::retrieve::RetrievalStatus::Cold,
        )
        .await;

        assert_eq!(outcome.findings.len(), 1, "{outcome:?}");
        assert!(!outcome.findings[0].late);
        assert_eq!(
            outcome.findings[0].confidence,
            DEGRADED_CONTEXT_CONFIDENCE_CAP
        );
    }

    #[tokio::test]
    async fn cold_retrieval_caps_changed_signature_claims_about_unavailable_callers() {
        let config = config();
        let diffs = vec![parse_file_patch(
            "src/parser.rs",
            "@@ -1,1 +1,1 @@\n-pub fn parse() -> i32 { 1 }\n+pub fn parse(input: i32) -> i32 { input }\n",
        )];
        let lookup = Lookup::Read {
            path: "src/parser.rs".into(),
            start: Some(1),
            end: Some(1),
        };
        let caller_search = Lookup::Search {
            pattern: "parse(".into(),
            glob: None,
        };
        let tree = crate::ports::tree::MockTree::from_recorded(
            [
                (
                    lookup.key(),
                    Found::Unavailable {
                        reason: "cold index".into(),
                    },
                ),
                (
                    caller_search.key(),
                    Found::Unavailable {
                        reason: "cold index cannot search callers".into(),
                    },
                ),
            ]
            .into_iter()
            .collect(),
        );
        let model = MockModel::new()
            .then(json!({
                "summary": "An unchanged caller still uses the old signature.",
                "findings": [{
                    "path": "src/parser.rs",
                    "existing_code": "pub fn parse(input: i32) -> i32 { input }",
                    "rule": "stale-caller",
                    "title": "Update callers of parse",
                    "body": "An unchanged caller still invokes parse with the old argument shape.",
                    "severity": "critical",
                    "confidence": 0.99
                }]
            }))
            .then(json!({"incorrect": []}));

        let outcome = run_with_context_and_tree(
            model,
            &config,
            &diffs,
            &BTreeMap::new(),
            crate::retrieve::RetrievalStatus::Cold,
            Some(&tree),
        )
        .await;

        assert_eq!(
            outcome.findings.len(),
            1,
            "the finding stays visible: {outcome:?}"
        );
        assert_eq!(
            outcome.findings[0].confidence,
            DEGRADED_CONTEXT_CONFIDENCE_CAP
        );
        assert_eq!(outcome.findings[0].severity, Severity::Critical);
    }

    #[tokio::test]
    async fn degraded_retrieval_caps_readable_unchanged_anchor_without_caller_evidence() {
        let config = config();
        let diffs = vec![parse_file_patch(
            "src/main.rs",
            "@@ -1,2 +1,3 @@\n fn main() {\n+    helper();\n }\n",
        )];
        let anchor_read = Lookup::Read {
            path: "src/main.rs".into(),
            start: Some(1),
            end: Some(1),
        };
        let caller_search = Lookup::Search {
            pattern: "fn helper(".into(),
            glob: None,
        };
        let tree = crate::ports::tree::MockTree::from_recorded(
            [
                (
                    anchor_read.key(),
                    Found::Text {
                        text: "1 fn main() {".into(),
                        start: 1,
                        end: 1,
                        total: 3,
                    },
                ),
                (
                    caller_search.key(),
                    Found::Unavailable {
                        reason: "cold index cannot search callers".into(),
                    },
                ),
            ]
            .into_iter()
            .collect(),
        );
        let model = MockModel::new()
            .then(json!({
                "summary": "The existing call needs a guard.",
                "findings": [{
                    "path": "src/main.rs",
                    "existing_code": "fn main() {",
                    "rule": "unsafe-caller",
                    "title": "Guard the existing caller",
                    "body": "The unchanged caller assumes an invalid contract.",
                    "severity": "high",
                    "confidence": 0.9
                }]
            }))
            .then(json!({"incorrect": []}));
        let model_handle = model.clone();

        let outcome = run_with_context_and_tree(
            model,
            &config,
            &diffs,
            &BTreeMap::new(),
            crate::retrieve::RetrievalStatus::Cold,
            Some(&tree),
        )
        .await;

        assert_eq!(outcome.findings.len(), 1, "{outcome:?}");
        assert!(outcome.findings[0].late);
        assert_eq!(
            outcome.findings[0].confidence,
            DEGRADED_CONTEXT_CONFIDENCE_CAP
        );
        assert_eq!(outcome.findings[0].severity, Severity::High);
        let request = model_handle
            .requests()
            .into_iter()
            .find(|request| request.schema_name == "tinysweeper_critique")
            .expect("the critique request");
        let prompt = request
            .messages
            .iter()
            .map(|message| message.content.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            prompt.contains("cold index cannot search callers"),
            "{prompt}"
        );
    }

    #[tokio::test]
    async fn a_degraded_claim_about_an_existing_declared_module_is_still_capped() {
        let config = config();
        let diffs = vec![parse_file_patch(
            "src/lib.rs",
            "@@ -1,1 +1,2 @@\n fn existing() {}\n+pub mod foo;\n",
        )];
        let tree =
            crate::ports::tree::MockTree::from_files([("src/foo/mod.rs", "pub fn foo() {}\n")]);
        let model = MockModel::new()
            .then(json!({
                "summary": "The declared module is missing.",
                "findings": [{
                    "path": "src/lib.rs",
                    "existing_code": "pub mod foo;",
                    "rule": "missing-module",
                    "title": "Add the missing foo module",
                    "body": "The module file is absent.",
                    "severity": "critical",
                    "confidence": 0.99
                }]
            }))
            .then(json!({"incorrect": []}));

        let outcome = run_with_context_and_tree(
            model,
            &config,
            &diffs,
            &BTreeMap::new(),
            crate::retrieve::RetrievalStatus::Cold,
            Some(&tree),
        )
        .await;

        assert_eq!(outcome.findings.len(), 1, "{outcome:?}");
        assert_eq!(outcome.findings[0].severity, Severity::Critical);
        assert_eq!(
            outcome.findings[0].confidence,
            DEGRADED_CONTEXT_CONFIDENCE_CAP
        );
    }

    #[tokio::test]
    async fn disabled_lookups_do_not_trigger_post_review_tree_reads() {
        let mut config = config();
        config.lookup.enabled = false;
        let diffs = vec![parse_file_patch(
            "src/lib.rs",
            "@@ -1,1 +1,2 @@\n fn existing() {}\n+pub mod foo;\n",
        )];
        let tree = CountingTree {
            inner: crate::ports::tree::MockTree::from_files([(
                "src/foo/mod.rs",
                "pub fn foo() {}\n",
            )]),
            lookups: std::sync::Mutex::new(Vec::new()),
        };
        let model = MockModel::new()
            .then(json!({
                "summary": "The declared module is missing.",
                "findings": [{
                    "path": "src/lib.rs",
                    "existing_code": "pub mod foo;",
                    "rule": "missing-module",
                    "title": "Add the missing foo module",
                    "body": "The module file is absent.",
                    "severity": "critical",
                    "confidence": 0.99
                }]
            }))
            .then(json!({"incorrect": []}));

        let outcome = run_with_context_and_tree(
            model,
            &config,
            &diffs,
            &BTreeMap::new(),
            crate::retrieve::RetrievalStatus::Cold,
            Some(&tree),
        )
        .await;

        assert_eq!(outcome.findings.len(), 1, "{outcome:?}");
        let lookups = tree.lookups.lock().expect("lookup calls");
        assert!(
            lookups.is_empty(),
            "disabled lookup policy must prevent post-review reads: {lookups:?}"
        );
        assert_eq!(
            outcome.findings[0].confidence,
            DEGRADED_CONTEXT_CONFIDENCE_CAP
        );
    }

    /// A finding in the pre-positioning shape, still accepted so a proposal
    /// written by an older version keeps working.
    fn finding_at(line: u64) -> serde_json::Value {
        json!({
            "path": "src/main.rs",
            "line": line,
            "rule": "unchecked-index",
            "title": "Guard the index before dereferencing",
            "body": "`i` is never bounds-checked.",
            "severity": "high",
            "confidence": 0.9
        })
    }

    #[tokio::test]
    async fn a_finding_on_a_changed_line_survives() {
        let model = MockModel::new().then(json!({
            "summary": "Adds an unchecked index.",
            "findings": [finding_at(2)]
        }));
        let outcome = run_with(model, &config(), &diffs()).await;

        assert_eq!(outcome.findings.len(), 1);
        assert_eq!(outcome.findings[0].severity, Severity::High);
        assert_eq!(outcome.findings[0].lane, LaneId::Critique);
    }

    #[tokio::test]
    async fn a_quoted_snippet_is_what_places_the_finding() {
        // The model quotes the code with the indentation it felt like using and
        // never names a line. Line 2 is where that code actually is.
        let model = MockModel::new().then(json!({
            "summary": "Adds an unchecked index.",
            "findings": [finding_quoting("let x = items[i];")]
        }));
        let outcome = run_with(model, &config(), &diffs()).await;

        assert_eq!(outcome.findings.len(), 1);
        assert_eq!(outcome.findings[0].line, Some(2));
    }

    #[tokio::test]
    async fn a_leaked_diff_marker_in_the_quote_does_not_lose_the_finding() {
        let model = MockModel::new().then(json!({
            "summary": "…",
            "findings": [finding_quoting("+    let x = items[i];")]
        }));
        let outcome = run_with(model, &config(), &diffs()).await;

        assert_eq!(outcome.findings[0].line, Some(2));
    }

    #[tokio::test]
    async fn a_multi_line_quote_becomes_a_range() {
        let model = MockModel::new().then(json!({
            "summary": "…",
            "findings": [finding_quoting("let x = items[i];\n\nprintln!(\"{x}\");")]
        }));
        let outcome = run_with(model, &config(), &diffs()).await;

        assert_eq!(outcome.findings[0].line, Some(2));
        assert_eq!(outcome.findings[0].end_line, Some(3));
    }

    #[tokio::test]
    async fn a_finding_that_resolves_outside_every_hunk_survives_without_a_line() {
        // Real finding, quoted from real code, but the diff never showed that
        // code — GitHub would reject the inline comment. It goes in the summary
        // rather than being deleted, which is what the old line-number filter
        // did to it.
        let mut files = BTreeMap::new();
        files.insert("src/main.rs".to_string(), FILE.to_string());
        let model = MockModel::new().then(json!({
            "summary": "…",
            "findings": [finding_quoting("    let cfg = load();")]
        }));

        let outcome = run_with_files(model, &config(), &diffs(), &files).await;

        assert_eq!(outcome.findings.len(), 1, "not deleted");
        assert_eq!(outcome.findings[0].line, None, "not postable inline");
        assert!(
            outcome.summary.contains("1 finding could not be anchored"),
            "{}",
            outcome.summary
        );
    }

    #[tokio::test]
    async fn a_quote_that_matches_nothing_leaves_the_finding_unanchored() {
        let model = MockModel::new().then(json!({
            "summary": "…",
            "findings": [finding_quoting("let y = somewhere_else();")]
        }));
        let outcome = run_with(model, &config(), &diffs()).await;

        assert_eq!(outcome.findings.len(), 1);
        assert_eq!(outcome.findings[0].line, None);
    }

    #[tokio::test]
    async fn a_hopeless_quote_is_recovered_by_the_relocation_call() {
        let model = MockModel::new()
            .then(json!({
                "summary": "…",
                "findings": [finding_quoting("the loop that indexes without checking")]
            }))
            .then(json!({"existing_code": "    let x = items[i];"}))
            .then(json!({"incorrect": []}));

        let outcome = run_with(model, &config(), &diffs()).await;

        assert_eq!(outcome.findings[0].line, Some(2));
    }

    #[tokio::test]
    async fn the_falsification_pass_drops_what_the_diff_disproves() {
        let model = MockModel::new()
            .then(json!({
                "summary": "…",
                "findings": [finding_quoting("let x = items[i];")]
            }))
            .then(json!({
                "incorrect": [{"index": 1, "reason": "the diff bounds-checks `i` above"}]
            }));

        let outcome = run_with(model, &config(), &diffs()).await;

        assert!(outcome.findings.is_empty());
        assert!(
            outcome
                .summary
                .contains("1 finding raised and dropped as disproved"),
            "{}",
            outcome.summary
        );
        assert!(
            outcome.summary.contains("the diff bounds-checks `i` above"),
            "the rejection reason replaces the prose it disproved: {}",
            outcome.summary
        );
    }

    #[derive(Debug, Default)]
    struct AddedModuleModel {
        requests: std::sync::Mutex<Vec<crate::ports::model::ModelRequest>>,
    }

    #[async_trait::async_trait]
    impl crate::ports::model::Model for AddedModuleModel {
        async fn complete(
            &self,
            request: crate::ports::model::ModelRequest,
        ) -> crate::error::Result<crate::ports::model::ModelResponse> {
            use crate::ports::model::{ModelResponse, Usage};

            let prompt = request
                .messages
                .iter()
                .map(|message| message.content.as_str())
                .collect::<Vec<_>>()
                .join("\n");
            let value = if request.schema_name.contains("falsify") {
                if prompt.contains("src/foo.rs") && prompt.contains("pub fn foo()") {
                    json!({"incorrect": [{"index": 1, "reason": "the pull request adds src/foo.rs with the module implementation"}]})
                } else {
                    json!({"incorrect": []})
                }
            } else if request.schema_name.contains("critique")
                && prompt.contains("The file is `src/lib.rs`")
            {
                json!({
                    "summary": "The module file is missing.",
                    "findings": [{
                        "path": "src/lib.rs",
                        "existing_code": "pub mod foo;",
                        "rule": "missing-module",
                        "title": "Add the missing foo module",
                        "body": "The declared module has no source file.",
                        "severity": "critical",
                        "confidence": 0.99
                    }]
                })
            } else {
                json!({"summary": "Nothing to report.", "findings": []})
            };
            self.requests
                .lock()
                .expect("record requests")
                .push(request.clone());
            Ok(ModelResponse {
                value,
                model: request.model,
                usage: Usage::default(),
            })
        }
    }

    #[tokio::test]
    async fn an_added_module_file_falsifies_a_missing_module_claim_across_groups() {
        let mut config = config();
        config.grouping.enabled = false;
        let pr = pull_request();
        let diffs = vec![
            parse_file_patch(
                "src/lib.rs",
                "@@ -1,1 +1,2 @@\n fn existing() {}\n+pub mod foo;\n",
            ),
            parse_file_patch("src/foo.rs", "@@ -0,0 +1,1 @@\n+pub fn foo() {}\n"),
            parse_file_patch(
                "docs/unrelated.md",
                &format!("@@ -0,0 +1,1 @@\n+{}\n", "unrelated context ".repeat(4_000)),
            ),
        ];
        let tree = crate::ports::tree::MockTree::from_files([("src/foo.rs", "pub fn foo() {}\n")]);
        let model = Arc::new(AddedModuleModel::default());

        let outcome = Critique::new(model.clone())
            .run(LaneInput {
                config: &config,
                pull_request: &pr,
                diffs: &diffs,
                file_contents: &BTreeMap::new(),
                scan_findings: &[],
                commits: &[],
                repo_policy: None,
                extracted_rules: &[],
                reviewed_evidence: "",
                prior_findings: &[],
                retrieval_status: crate::retrieve::RetrievalStatus::Off,
                retrieved_context: "",
                redaction_note: "",
                e2e: None,
                tree: Some(&tree),
                graph: None,
                pass_yield_observer: None,
            })
            .await
            .expect("lane runs");

        assert!(outcome.findings.is_empty(), "{outcome:?}");
        let requests = model.requests.lock().expect("recorded requests");
        let falsifier = requests
            .iter()
            .find(|request| request.schema_name.contains("falsify"))
            .expect("the false finding reaches falsification");
        let falsifier_prompt = falsifier
            .messages
            .iter()
            .map(|message| message.content.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            falsifier_prompt.contains("src/foo.rs") && falsifier_prompt.contains("pub fn foo()"),
            "falsification must see the added file even though another group owns it: {falsifier_prompt}"
        );
        assert!(
            !falsifier_prompt.contains("unrelated context"),
            "cross-file falsification must not replay unrelated PR files: {falsifier_prompt}"
        );
        assert!(
            falsifier_prompt.len() < 20_000,
            "cross-file evidence must have a deterministic byte ceiling: {} bytes",
            falsifier_prompt.len()
        );
        assert!(
            requests.iter().any(|request| {
                request.schema_name.contains("critique")
                    && request
                        .messages
                        .iter()
                        .any(|message| message.content.contains("pub fn foo()"))
            }),
            "the module declaration's seed must read the new file from the head tree"
        );
    }

    #[tokio::test]
    async fn a_degraded_missing_module_claim_is_capped_without_a_successful_head_read() {
        let mut config = config();
        config.grouping.enabled = false;
        let pr = pull_request();
        let diffs = vec![parse_file_patch(
            "src/lib.rs",
            "@@ -1,1 +1,2 @@\n fn existing() {}\n+pub mod foo;\n",
        )];
        let module_rs = Lookup::Read {
            path: "src/foo.rs".into(),
            start: None,
            end: None,
        };
        let module_mod_rs = Lookup::Read {
            path: "src/foo/mod.rs".into(),
            start: None,
            end: None,
        };
        let tree = crate::ports::tree::MockTree::from_recorded(
            [
                (module_rs.key(), Found::NotFound),
                (
                    module_mod_rs.key(),
                    Found::Unavailable {
                        reason: "the index is cold".into(),
                    },
                ),
            ]
            .into_iter()
            .collect(),
        );
        let model = Arc::new(AddedModuleModel::default());

        let outcome = Critique::new(model)
            .run(LaneInput {
                config: &config,
                pull_request: &pr,
                diffs: &diffs,
                file_contents: &BTreeMap::new(),
                scan_findings: &[],
                commits: &[],
                repo_policy: None,
                extracted_rules: &[],
                reviewed_evidence: "",
                prior_findings: &[],
                retrieval_status: crate::retrieve::RetrievalStatus::Cold,
                retrieved_context: "",
                redaction_note: "",
                e2e: None,
                tree: Some(&tree),
                graph: None,
                pass_yield_observer: None,
            })
            .await
            .expect("lane runs");

        assert_eq!(outcome.findings.len(), 1, "{outcome:?}");
        assert!(
            outcome.findings[0].confidence <= DEGRADED_CONTEXT_CONFIDENCE_CAP,
            "a changed `pub mod` anchor does not prove the external module exists: {outcome:?}"
        );
        assert!(outcome.summary.contains("capped at Likely confidence"));
    }

    /// Keep the fixture large enough to exercise multi-pass prompt handling.
    const LARGE_LINES: usize = 40;

    /// A synthetic patch whose group is large enough for a coverage pass to
    /// run at all — `diffs()` is deliberately two lines, so every coverage
    /// pass test needs its own, bigger fixture.
    fn large_patch() -> String {
        let mut patch = String::from("@@ -1,2 +1,42 @@\n fn main() {\n");
        for i in 0..LARGE_LINES {
            patch.push_str(&format!("+    let x{i} = {i};\n"));
        }
        patch.push_str(" }\n");
        patch
    }

    fn large_diffs() -> Vec<FileDiff> {
        vec![parse_file_patch("src/large.rs", &large_patch())]
    }

    /// A finding quoting one of `large_diffs`'s added lines, named and titled
    /// by the caller so round one and a coverage pass can be told apart.
    fn finding_named(title: &str, index: usize) -> serde_json::Value {
        finding_named_with_rule(title, index, "unchecked-index")
    }

    /// [`finding_named`], with its own rule id — for a corroboration test that
    /// must not also match on [`Finding::fingerprint`], which hashes the rule.
    fn finding_named_with_rule(title: &str, index: usize, rule: &str) -> serde_json::Value {
        json!({
            "path": "src/large.rs",
            "existing_code": format!("let x{index} = {index};"),
            "rule": rule,
            "title": title,
            "body": "detail.",
            "severity": "high",
            "confidence": 0.9
        })
    }

    fn config_with_passes(passes: u8) -> Config {
        let mut config = config();
        config.review.passes = passes;
        config
    }

    /// A finding on `large_diffs`'s file whose quote matches nothing there,
    /// forcing `place` to spend a relocation call on it — the shape a
    /// coverage response takes when it names findings the relocation budget
    /// cannot all afford.
    fn finding_hopeless(title: &str, quote: &str) -> serde_json::Value {
        json!({
            "path": "src/large.rs",
            "existing_code": quote,
            "rule": "unchecked-index",
            "title": title,
            "body": "detail.",
            "severity": "high",
            "confidence": 0.9
        })
    }

    #[tokio::test]
    async fn a_coverage_pass_is_not_run_below_the_line_threshold() {
        // `diffs()` is two lines, well under the threshold — passes = 2 must
        // not build a second prompt over it.
        let model = MockModel::new().then(json!({
            "summary": "Nothing to report.",
            "findings": []
        }));
        let handle = model.clone();

        run_with(model, &config_with_passes(2), &diffs()).await;

        assert_eq!(handle.calls(), 1, "no coverage call should have been made");
    }

    #[tokio::test]
    async fn a_coverage_pass_runs_once_more_above_the_threshold() {
        let model = MockModel::new()
            .then(json!({
                "summary": "…",
                "findings": [finding_named("Guard the first index", 3)]
            }))
            .then(json!({"incorrect": []}))
            .then(json!({"summary": "…", "findings": []}));
        let handle = model.clone();

        run_with(model, &config_with_passes(2), &large_diffs()).await;

        assert_eq!(
            handle.calls(),
            3,
            "round one's review, round one's falsify, and the coverage pass"
        );
        let coverage_request = handle
            .requests()
            .last()
            .expect("the coverage pass made a request")
            .messages
            .iter()
            .map(|m| m.content.clone())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(coverage_request.contains("## What you already found"));
        assert!(coverage_request.contains("Guard the first index"));
    }

    #[tokio::test]
    async fn a_low_risk_group_without_candidates_uses_one_reviewer_call() {
        let model = MockModel::new().then(json!({
            "summary": "Nothing to report.",
            "findings": []
        }));
        let handle = model.clone();

        run_with(model, &config_with_passes(2), &large_diffs()).await;

        assert_eq!(
            handle.calls(),
            1,
            "an empty first-pass result on an ordinary group must not buy another look"
        );
    }

    #[tokio::test]
    async fn a_high_risk_group_without_candidates_gets_an_adaptive_pass() {
        let model = MockModel::new()
            .then(json!({"summary": "Nothing to report.", "findings": []}))
            .then(json!({"summary": "Nothing to report.", "findings": []}));
        let handle = model.clone();
        let auth_diff = vec![parse_file_patch("src/auth.rs", PATCH)];

        run_with(model, &config_with_passes(2), &auth_diff).await;

        assert_eq!(
            handle.calls(),
            2,
            "a sensitive path keeps a second look even when the first found no candidate"
        );
    }

    #[tokio::test]
    async fn a_small_group_with_a_candidate_gets_an_adaptive_pass() {
        let model = MockModel::new()
            .then(json!({
                "summary": "Unchecked index.",
                "findings": [{
                    "path": "src/main.rs",
                    "existing_code": "let x = items[i];",
                    "rule": "unchecked-index",
                    "title": "Guard the index",
                    "body": "The index may be out of bounds.",
                    "severity": "high",
                    "confidence": 0.9
                }]
            }))
            .then(json!({"incorrect": []}))
            .then(json!({"summary": "No further findings.", "findings": []}));
        let handle = model.clone();

        run_with(model, &config_with_passes(2), &diffs()).await;

        assert_eq!(
            handle.calls(),
            3,
            "a small group's first-pass candidate should earn one further review"
        );
    }

    #[tokio::test]
    async fn a_small_candidate_rejected_by_falsification_still_gets_a_second_review() {
        let model = MockModel::new()
            .then(json!({
                "summary": "Unchecked index.",
                "findings": [{
                    "path": "src/main.rs",
                    "existing_code": "let x = items[i];",
                    "rule": "unchecked-index",
                    "title": "Guard the index",
                    "body": "The index may be out of bounds.",
                    "severity": "high",
                    "confidence": 0.9
                }]
            }))
            .then(json!({"incorrect": [{"index": 1, "reason": "The diff proves i is in bounds."}]}))
            .then(json!({"summary": "No further findings.", "findings": []}));
        let handle = model.clone();
        let config = config_with_passes(2);
        let pr = pull_request();
        let diffs = diffs();
        let timing = crate::review_timing::ReviewTiming::new("owner/repo", 7, "Normal");
        let outcome = Critique::new(Arc::new(model))
            .run(LaneInput {
                config: &config,
                pull_request: &pr,
                diffs: &diffs,
                file_contents: &BTreeMap::new(),
                scan_findings: &[],
                commits: &[],
                repo_policy: None,
                extracted_rules: &[],
                reviewed_evidence: "",
                prior_findings: &[],
                retrieval_status: crate::retrieve::RetrievalStatus::Off,
                retrieved_context: "",
                redaction_note: "",
                e2e: None,
                tree: None,
                graph: None,
                pass_yield_observer: Some(&timing),
            })
            .await
            .expect("lane runs");

        assert!(
            outcome.findings.is_empty(),
            "falsification rejected the candidate"
        );
        let requests = handle.requests();
        assert_eq!(requests.len(), 3);
        assert_eq!(
            requests
                .iter()
                .map(|request| request.schema_name.as_str())
                .collect::<Vec<_>>(),
            [
                "tinysweeper_critique",
                "tinysweeper_falsify",
                "tinysweeper_critique"
            ],
            "the final request is a second reviewer call, even though no finding survived"
        );
        let yields = timing.snapshot().pass_yields;
        assert_eq!(yields["critique:1"].groups, 1);
        assert_eq!(yields["critique:1"].candidates, 1);
        assert_eq!(yields["critique:1"].survivors, 0);
        assert_eq!(yields["critique:2"].groups, 1);
        assert_eq!(yields["critique:2"].candidates, 0);
        assert_eq!(yields["critique:2"].survivors, 0);
    }

    #[tokio::test]
    async fn critique_coverage_and_falsify_requests_use_their_custom_efforts() {
        let model = MockModel::new()
            .then(json!({
                "summary": "Unchecked index.",
                "findings": [finding_named("Guard the first index", 3)]
            }))
            .then(json!({"incorrect": []}))
            .then(json!({"summary": "No additional findings.", "findings": []}));
        let handle = model.clone();
        let mut config = config_with_passes(2);
        config.models.reasoning_effort = "high".into();
        config.models.reviewer_reasoning_effort = "low".into();
        config.models.falsify_reasoning_effort = "medium".into();
        config.models.summary_reasoning_effort = "off".into();

        run_with(model, &config, &large_diffs()).await;

        let requests = handle.requests();
        assert_eq!(requests.len(), 3);
        assert_eq!(requests[0].schema_name, "tinysweeper_critique");
        assert_eq!(requests[0].reasoning_effort.as_deref(), Some("low"));
        assert_eq!(requests[1].schema_name, "tinysweeper_falsify");
        assert_eq!(requests[1].reasoning_effort.as_deref(), Some("medium"));
        assert_eq!(requests[2].schema_name, "tinysweeper_critique");
        assert_eq!(requests[2].reasoning_effort.as_deref(), Some("low"));
    }

    #[tokio::test]
    async fn blank_global_effort_disables_reviewer_and_falsifier_requests() {
        let mut role_failures = Vec::new();
        for global in ["", "   "] {
            let model = MockModel::new()
                .then(json!({
                    "summary": "Unchecked index.",
                    "findings": [finding_named("Guard the first index", 3)]
                }))
                .then(json!({"incorrect": []}))
                .then(json!({"summary": "No additional findings.", "findings": []}));
            let handle = model.clone();
            let mut config = config_with_passes(2);
            config.models.reasoning_effort = global.into();

            run_with(model, &config, &large_diffs()).await;

            let requests = handle.requests();
            assert_eq!(requests.len(), 3);
            assert_eq!(requests[0].schema_name, "tinysweeper_critique");
            assert_eq!(requests[1].schema_name, "tinysweeper_falsify");
            assert_eq!(requests[2].schema_name, "tinysweeper_critique");
            for request in &requests {
                if request.reasoning_effort.as_deref() != Some("off") {
                    role_failures.push(format!(
                        "global {global:?}, schema {}: got {:?}",
                        request.schema_name, request.reasoning_effort
                    ));
                }
            }
        }
        assert!(role_failures.is_empty(), "{role_failures:#?}");
    }

    #[tokio::test]
    async fn one_pass_empty_groups_emit_zero_yield_metrics() {
        use tracing::instrument::WithSubscriber;

        let model = MockModel::new().then(json!({
            "summary": "Nothing to report.",
            "findings": []
        }));
        let handle = model.clone();
        let bytes = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let writer_bytes = bytes.clone();
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_writer(move || MetricsLogWriter(writer_bytes.clone()))
            .finish();

        run_with(model, &config_with_passes(1), &diffs())
            .with_subscriber(subscriber)
            .await;

        assert_eq!(handle.calls(), 1);
        let line = String::from_utf8(bytes.lock().unwrap().clone()).unwrap();
        assert_eq!(line.matches("adaptive review passes").count(), 1, "{line}");
        assert!(line.contains("passes_attempted=1"), "{line}");
        assert!(line.contains("candidate_findings_per_pass=[0]"), "{line}");
        assert!(line.contains("surviving_findings_per_pass=[0]"), "{line}");
    }

    #[tokio::test]
    async fn adaptive_requests_keep_path_rules_and_redaction_guidance() {
        let mut config = config_with_passes(2);
        config.path_instructions = vec![
            PathInstruction {
                glob: "src/auth.rs".into(),
                instructions: "Check every index against the collection length.".into(),
                rules: None,
                lanes: vec![LaneId::Critique],
                merge: false,
            },
            PathInstruction {
                glob: "docs/**".into(),
                instructions: "This unrelated rule must stay out of the request.".into(),
                rules: None,
                lanes: vec![LaneId::Critique],
                merge: false,
            },
        ];
        let patch = large_patch().replace(
            "+    let x0 = 0;",
            "+    let x0 = \"<redacted, 12 chars>\";",
        );
        let diffs = vec![parse_file_patch("src/auth.rs", &patch)];
        let model = MockModel::new()
            .then(json!({"summary": "Nothing to report.", "findings": []}))
            .then(json!({"summary": "Nothing further.", "findings": []}));
        let handle = model.clone();

        run_with(model, &config, &diffs).await;

        let requests = handle.requests();
        assert_eq!(requests.len(), 2);
        for request in &requests {
            let prompt = request
                .messages
                .iter()
                .map(|message| message.content.as_str())
                .collect::<Vec<_>>()
                .join("\n");
            assert!(
                prompt.contains("Check every index against the collection length."),
                "{prompt}"
            );
            assert!(
                !prompt.contains("This unrelated rule must stay out of the request."),
                "{prompt}"
            );
            assert!(prompt.contains("never ask for or guess them"), "{prompt}");
        }
    }

    #[tokio::test]
    async fn a_coverage_pass_finding_that_corroborates_round_one_is_dropped() {
        let model = MockModel::new()
            .then(json!({
                "summary": "…",
                "findings": [finding_named("Guard the first index", 3)]
            }))
            .then(json!({"incorrect": []}))
            .then(json!({
                "summary": "…",
                // One line over, a different rule id and a different-sounding
                // title: neither the fingerprint nor the wording matches, but
                // the anchored range overlaps within `agree::LINE_TOLERANCE`,
                // which is what `corroborates` — not the fingerprint check —
                // has to catch.
                "findings": [finding_named_with_rule(
                    "Bounds-check x4 before use",
                    4,
                    "missing-bounds-check"
                )]
            }));

        let outcome = run_with(model, &config_with_passes(2), &large_diffs()).await;

        assert_eq!(outcome.findings.len(), 1);
        assert_eq!(outcome.findings[0].title, "Guard the first index");
    }

    #[tokio::test]
    async fn a_coverage_pass_finding_on_a_new_line_survives_and_is_falsified() {
        let model = MockModel::new()
            .then(json!({
                "summary": "…",
                "findings": [finding_named("Guard the first index", 3)]
            }))
            .then(json!({"incorrect": []}))
            .then(json!({
                "summary": "…",
                "findings": [finding_named("Guard the second index", 30)]
            }))
            .then(json!({
                "incorrect": [{"index": 1, "reason": "x30 is never dereferenced"}]
            }));

        let outcome = run_with(model, &config_with_passes(2), &large_diffs()).await;

        assert_eq!(
            outcome.findings.len(),
            1,
            "the disproved coverage-pass finding must not survive"
        );
        assert_eq!(outcome.findings[0].title, "Guard the first index");
    }

    #[tokio::test]
    async fn a_coverage_finding_updates_a_clean_round_one_summary() {
        // Round one's own prose is frozen before the coverage pass ever runs.
        // If round one found nothing and the coverage pass then adds a
        // surviving finding, the summary must say so rather than keep
        // reading "Nothing to report." while `findings` says otherwise.
        let mut coverage_finding = finding_named("Guard the second index", 30);
        coverage_finding["path"] = json!("src/auth.rs");
        let model = MockModel::new()
            .then(json!({"summary": "Nothing to report.", "findings": []}))
            .then(json!({
                "summary": "…",
                "findings": [coverage_finding]
            }))
            .then(json!({"incorrect": []}));

        let mut diff = large_diffs();
        diff[0].path = "src/auth.rs".into();
        let outcome = run_with(model, &config_with_passes(2), &diff).await;

        assert_eq!(outcome.findings.len(), 1, "{:#?}", outcome.findings);
        assert!(
            outcome.summary.contains("1 finding added by a second pass"),
            "{}",
            outcome.summary
        );
    }

    #[tokio::test]
    async fn passes_1_never_makes_a_second_call() {
        let model = MockModel::new().then(json!({
            "summary": "Nothing to report.",
            "findings": []
        }));
        let handle = model.clone();

        // Exercise the operator's explicit one-pass setting; production ships
        // with a maximum adaptive depth of three.
        run_with(model, &config(), &large_diffs()).await;

        assert_eq!(handle.calls(), 1);
    }

    #[tokio::test]
    async fn a_coverage_pass_with_zero_new_findings_makes_no_extra_falsify_call() {
        let model = MockModel::new()
            .then(json!({
                "summary": "…",
                "findings": [finding_named("Guard the first index", 3)]
            }))
            .then(json!({"incorrect": []}))
            .then(json!({"summary": "Nothing further.", "findings": []}));
        let handle = model.clone();

        run_with(model, &config_with_passes(2), &large_diffs()).await;

        assert_eq!(
            handle.calls(),
            3,
            "an empty coverage answer must not falsify anything"
        );
    }

    #[tokio::test]
    async fn a_third_pass_is_skipped_when_the_second_added_nothing() {
        let model = MockModel::new()
            .then(json!({
                "summary": "…",
                "findings": [finding_named("Guard the first index", 3)]
            }))
            .then(json!({"incorrect": []}))
            .then(json!({"summary": "Nothing further.", "findings": []}));
        let handle = model.clone();

        run_with(model, &config_with_passes(3), &large_diffs()).await;

        assert_eq!(
            handle.calls(),
            3,
            "the second coverage pass added nothing, so a third must not run"
        );
    }

    #[tokio::test]
    async fn a_new_second_pass_finding_unlocks_the_third_pass() {
        let model = MockModel::new()
            .then(json!({
                "summary": "…",
                "findings": [finding_named("Guard the first index", 3)]
            }))
            .then(json!({"incorrect": []}))
            .then(json!({
                "summary": "…",
                "findings": [finding_named("Guard the second index", 20)]
            }))
            .then(json!({"incorrect": []}))
            .then(json!({"summary": "Nothing further.", "findings": []}));
        let handle = model.clone();

        let outcome = run_with(model, &config_with_passes(3), &large_diffs()).await;

        assert_eq!(outcome.findings.len(), 2, "{:#?}", outcome.findings);
        assert_eq!(
            outcome
                .findings
                .iter()
                .map(|finding| finding.review_pass)
                .collect::<Vec<_>>(),
            vec![1, 2]
        );
        assert_eq!(
            handle.calls(),
            5,
            "review/falsify, second-pass review/falsify, then pass three"
        );
        let requests = handle.requests();
        let third_request = requests.last().unwrap();
        let prompt = third_request
            .messages
            .iter()
            .map(|message| message.content.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(prompt.contains("Guard the first index"), "{prompt}");
        assert!(prompt.contains("Guard the second index"), "{prompt}");
    }

    #[tokio::test]
    async fn a_malformed_third_pass_keeps_findings_from_both_earlier_passes() {
        let model = MockModel::new()
            .then(json!({
                "summary": "…",
                "findings": [finding_named("Guard the first index", 3)]
            }))
            .then(json!({"incorrect": []}))
            .then(json!({
                "summary": "…",
                "findings": [finding_named("Guard the second index", 20)]
            }))
            .then(json!({"incorrect": []}))
            .then(json!({"summary": "…", "findings": [{"rule": "broken"}]}));

        let outcome = run_with(model, &config_with_passes(3), &large_diffs()).await;

        assert_eq!(outcome.findings.len(), 2, "{:#?}", outcome.findings);
    }

    #[tokio::test]
    async fn a_failed_coverage_pass_keeps_round_ones_findings() {
        let model = MockModel::new()
            .then(json!({
                "summary": "…",
                "findings": [finding_named("Guard the first index", 3)]
            }))
            .then(json!({"incorrect": []}))
            .then_error("provider unavailable");
        let handle = model.clone();

        let outcome = run_with(model, &config_with_passes(3), &large_diffs()).await;

        assert_eq!(outcome.findings.len(), 1, "{:#?}", outcome.findings);
        assert_eq!(handle.calls(), 3, "failure must stop before pass three");
    }

    #[tokio::test]
    async fn an_adaptive_lookup_refusal_keeps_its_paid_usage_and_elapsed_time() {
        let (handle, model, execution) = crate::lanes::coverage::test_support::limited_model(
            json!({"summary": "Nothing to report.", "findings": []}),
            json!({
                "summary": "I need to inspect the called helper.",
                "findings": [],
                "lookups": [{
                    "kind": "read",
                    "path": "src/defs.rs",
                    "start": 1,
                    "end": 1,
                    "why": "Check the helper contract before concluding."
                }]
            }),
        );
        let tree = crate::lanes::coverage::test_support::DelayedTree::new(
            std::time::Duration::from_millis(15),
        );
        let config = config_with_passes(2);
        let mut diffs = large_diffs();
        diffs[0].path = "src/auth.rs".into();
        let pr = pull_request();
        let file_contents = BTreeMap::new();

        let (outcome, trace) = crate::lanes::coverage::test_support::capture_trace(async {
            let lane = Critique::new(model);
            let input = LaneInput {
                config: &config,
                pull_request: &pr,
                diffs: &diffs,
                file_contents: &file_contents,
                scan_findings: &[],
                commits: &[],
                repo_policy: None,
                extracted_rules: &[],
                reviewed_evidence: "",
                prior_findings: &[],
                retrieval_status: crate::retrieve::RetrievalStatus::Off,
                retrieved_context: "",
                redaction_note: "",
                e2e: None,
                tree: Some(&tree),
                graph: None,
                pass_yield_observer: None,
            };
            let prepared = prepare(input.preparation());
            lane.run_prepared_with_execution(input, prepared, Some(execution))
                .await
                .expect("a typed adaptive refusal is a partial lane result")
        })
        .await;

        assert_eq!(
            handle.calls(),
            2,
            "the third model call is refused before dispatch"
        );
        assert_eq!(outcome.spend.usage.input_tokens, 8);
        assert_eq!(outcome.spend.usage.output_tokens, 4);
        assert!((outcome.spend.usage.cost_usd - 0.25).abs() < f64::EPSILON);
        assert!(outcome.unanswered.contains(&"src/auth.rs".to_string()));
        assert!(outcome.omissions.iter().any(|omission| {
            omission.paths.contains(&"src/auth.rs".to_string())
                && omission.reason == crate::lanes::fanout::OmissionReason::CallLimit
        }));
        let event = trace
            .lines()
            .find(|line| line.contains("adaptive review passes"))
            .expect("adaptive pass accounting is emitted");
        assert!(event.contains("stop_reason=\"refused\""), "{event}");
        assert!(event.contains("input_tokens=8"), "{event}");
        assert!(event.contains("output_tokens=4"), "{event}");
        assert!(event.contains("cost_usd=0.25"), "{event}");
        let elapsed_ms = event
            .split("model_elapsed_ms=")
            .nth(1)
            .and_then(|value| value.split_whitespace().next())
            .and_then(|value| value.parse::<u64>().ok())
            .expect("adaptive elapsed field is present");
        assert!(elapsed_ms >= 10, "the accounted lookup took 15ms: {event}");
    }

    #[tokio::test]
    async fn a_coverage_placement_failure_preserves_round_one_findings() {
        // A coverage response with more unresolvable quotes than the
        // relocation budget affords must not lose round one's own, already
        // falsified finding — placement failing on this optional pass is no
        // additional coverage result, not a reason to fail the whole group.
        //
        // Round one's review call and the coverage pass's own review call are
        // routed through the lane capability, so they alone count against
        // `budget_usd_per_pr` (0.02 for both). Relocation calls go straight to
        // the model port and are bounded only by `place`'s own tally, so the
        // budget is set just above what round one and the coverage review
        // spend, and three hopeless quotes are enough to cross it on the
        // fourth relocation attempt.
        let mut config = config_with_passes(2);
        config.models.budget_usd_per_pr = 0.025;

        let model = MockModel::new()
            .with_usage(crate::ports::model::Usage {
                cost_usd: 0.01,
                ..crate::ports::model::Usage::default()
            })
            .then(json!({
                "summary": "…",
                "findings": [finding_named("Guard the first index", 3)]
            }))
            .then(json!({"incorrect": []}))
            .then(json!({
                "summary": "…",
                "findings": [
                    finding_hopeless("Guard the second index", "a snippet nowhere in the diff"),
                    finding_hopeless("Guard the third index", "another snippet nowhere in it"),
                    finding_hopeless("Guard the fourth index", "yet another absent snippet"),
                    finding_hopeless("Guard the fifth index", "and one more absent snippet"),
                ]
            }))
            .then(json!({"existing_code": "let x0 = 0;"}))
            .then(json!({"existing_code": "let x1 = 1;"}))
            .then(json!({"existing_code": "let x2 = 2;"}));
        let handle = model.clone();

        let outcome = run_with(model, &config, &large_diffs()).await;

        assert_eq!(
            outcome.findings.len(),
            1,
            "round one's finding must survive a coverage placement failure"
        );
        assert_eq!(outcome.findings[0].title, "Guard the first index");
        assert_eq!(
            handle.calls(),
            6,
            "round one's review and falsify, the coverage review, and the \
             three relocation calls the budget afforded before the fourth \
             finding tripped it"
        );
    }

    /// The bug this lane shipped: a check run whose summary asserted a bug,
    /// reported no findings, concluded success and approved the pull request.
    /// The model's prose is written before falsification runs, so once the
    /// filter empties the finding list the prose is describing nothing.
    #[tokio::test]
    async fn a_summary_never_asserts_a_bug_the_falsifier_removed() {
        let model = MockModel::new()
            .then(json!({
                "summary": "One real bug: the coverage edge is never stored.",
                "findings": [finding_quoting("let x = items[i];")]
            }))
            .then(json!({
                "incorrect": [{"index": 1, "reason": "the diff stores it two lines above"}]
            }));

        let outcome = run_with(model, &config(), &diffs()).await;

        assert!(outcome.findings.is_empty());
        assert!(
            !outcome.summary.contains("One real bug"),
            "a lane reporting nothing must not narrate something: {}",
            outcome.summary
        );
        assert_eq!(
            outcome.conclusion(Severity::High),
            CheckConclusion::Success,
            "the verdict was already clean; it is the summary that had to agree with it"
        );
    }

    #[tokio::test]
    async fn a_partial_rejection_never_leaves_the_removed_claim_in_summary_prose() {
        let mut false_claim = finding_quoting("let x = items[i];");
        false_claim["title"] = json!("The budget limit cannot be exhausted");
        false_claim["body"] = json!("The budget assertion is unreachable.");
        let survivor = finding_quoting("println!(\"{x}\");");
        let model = MockModel::new()
            .then(json!({
                "summary": "The budget assertion is unreachable and the index may panic.",
                "findings": [false_claim, survivor]
            }))
            .then(json!({"incorrect": [{"index": 1,
                "reason": "The final output bound is four times the base bound, so the assertion is reachable."}]}));
        let handle = model.clone();
        let outcome = run_with(model, &config(), &diffs()).await;
        assert_eq!(outcome.findings.len(), 1);
        assert_eq!(
            handle.calls(),
            2,
            "Use the existing review and falsifier calls"
        );
        assert!(
            !outcome.summary.contains("budget assertion is unreachable"),
            "A surviving finding cannot authenticate disproven original prose: {}",
            outcome.summary
        );
        assert!(
            outcome.summary.contains(&outcome.findings[0].title),
            "Replacement prose should name the actual surviving observation: {}",
            outcome.summary
        );
    }

    #[tokio::test]
    async fn coverage_survivors_do_not_restore_disproven_initial_summary_prose() {
        let model = MockModel::new()
            .then(json!({"summary": "The budget assertion is unreachable.",
                "findings": [finding_named("False budget claim", 3)]}))
            .then(
                json!({"incorrect": [{"index": 1, "reason": "The bound is multiplied by four."}]}),
            )
            .then(json!({"summary": "A separate genuine bug.",
                "findings": [finding_named("Guard the surviving index", 30)]}))
            .then(json!({"incorrect": []}));
        let handle = model.clone();
        let outcome = run_with(model, &config_with_passes(2), &large_diffs()).await;
        assert_eq!(outcome.findings.len(), 1);
        assert_eq!(handle.calls(), 4);
        assert!(
            !outcome.summary.contains("budget assertion is unreachable"),
            "{}",
            outcome.summary
        );
        assert!(
            outcome.summary.contains("Guard the surviving index"),
            "{}",
            outcome.summary
        );
    }

    #[tokio::test]
    async fn a_broken_falsification_pass_never_deletes_a_review() {
        let model = MockModel::new()
            .then(json!({
                "summary": "…",
                "findings": [finding_quoting("let x = items[i];")]
            }))
            .then_error("upstream exploded");

        let outcome = run_with(model, &config(), &diffs()).await;

        assert_eq!(outcome.findings.len(), 1, "failed open");
        assert!(
            !outcome.summary.contains("disproved"),
            "{}",
            outcome.summary
        );
    }

    #[tokio::test]
    async fn a_finding_quoting_a_line_the_model_did_not_change_is_still_postable() {
        // Line 1 is context inside the hunk. The model quoted it, so it read
        // it, and GitHub will take a comment there.
        let model = MockModel::new().then(json!({
            "summary": "…",
            "findings": [finding_quoting("fn main() {")]
        }));
        let outcome = run_with(model, &config(), &diffs()).await;

        assert_eq!(outcome.findings[0].line, Some(1));
    }

    #[tokio::test]
    async fn a_legacy_response_with_a_line_and_no_quote_still_anchors() {
        // Migration: a proposal or a fine-tune still answering with the old
        // schema keeps working, because its number is better than nothing.
        let model = MockModel::new().then(json!({
            "summary": "…",
            "findings": [finding_at(2)]
        }));
        let outcome = run_with(model, &config(), &diffs()).await;

        assert_eq!(outcome.findings[0].line, Some(2));
    }

    #[tokio::test]
    async fn a_legacy_line_outside_every_hunk_is_not_trusted() {
        let model = MockModel::new().then(json!({
            "summary": "…",
            "findings": [finding_at(99)]
        }));
        let outcome = run_with(model, &config(), &diffs()).await;

        assert_eq!(outcome.findings[0].line, None);
    }

    #[tokio::test]
    async fn a_finding_in_a_file_the_pull_request_never_touched_is_discarded() {
        let model = MockModel::new().then(json!({
            "summary": "…",
            "findings": [{
                "path": "src/elsewhere.rs",
                "existing_code": "let x = items[i];",
                "rule": "r", "title": "t", "body": "b",
                "severity": "high", "confidence": 0.9
            }]
        }));
        let outcome = run_with(model, &config(), &diffs()).await;
        assert!(outcome.findings.is_empty());
        assert!(
            outcome.summary.contains("did not change"),
            "{}",
            outcome.summary
        );
    }

    #[tokio::test]
    async fn a_late_finding_may_sit_on_unchanged_lines_of_a_touched_file() {
        let mut late = finding_at(1);
        late["late"] = json!(true);
        let model = MockModel::new().then(json!({"summary": "…", "findings": [late]}));

        let outcome = run_with(model, &config(), &diffs()).await;
        assert_eq!(outcome.findings.len(), 1);
        assert!(outcome.findings[0].late);
    }

    #[tokio::test]
    async fn a_finding_quoting_a_context_line_is_marked_pre_existing() {
        // Postability is the hunk, which is wider than the lines this pull
        // request changed. A finding that lands on a context line is therefore
        // about code the author did not touch, and the reader has to be able to
        // tell — the model did not say `late`, the diff did.
        let model = MockModel::new().then(json!({
            "summary": "…",
            "findings": [finding_quoting("fn main() {")]
        }));

        let outcome = run_with(model, &config(), &diffs()).await;

        assert_eq!(outcome.findings.len(), 1);
        assert_eq!(outcome.findings[0].line, Some(1), "the context line");
        assert!(
            outcome.findings[0].late,
            "an untouched line must carry the pre-existing badge"
        );
    }

    #[tokio::test]
    async fn a_finding_quoting_an_added_line_is_not_marked_pre_existing() {
        let model = MockModel::new().then(json!({
            "summary": "…",
            "findings": [finding_quoting("    let x = items[i];")]
        }));

        let outcome = run_with(model, &config(), &diffs()).await;

        assert_eq!(outcome.findings.len(), 1);
        assert!(
            !outcome.findings[0].late,
            "this pull request introduced the line"
        );
    }

    #[tokio::test]
    async fn an_empty_review_is_reported_as_such() {
        let outcome = run_with(MockModel::silent(), &config(), &diffs()).await;

        assert!(outcome.findings.is_empty());
        assert_eq!(outcome.summary, "Nothing to report.");
        assert!(
            outcome.skipped.is_none(),
            "silence is not the same as skipping"
        );
    }

    #[tokio::test]
    async fn a_pull_request_with_nothing_to_review_never_calls_the_model() {
        let model = MockModel::new();
        let outcome = run_with(model.clone(), &config(), &[]).await;

        assert_eq!(model.calls(), 0, "spent money on a foregone conclusion");
        assert!(outcome.skipped.is_some());
    }

    #[tokio::test]
    async fn a_draft_is_skipped_unless_the_repository_opts_in() {
        let config = config();
        let model = MockModel::silent();
        let pr = PullRequest {
            draft: true,
            ..pull_request()
        };
        let diffs = diffs();

        let outcome = Critique::new(Arc::new(model.clone()))
            .run(LaneInput {
                config: &config,
                pull_request: &pr,
                diffs: &diffs,
                file_contents: &BTreeMap::new(),
                scan_findings: &[],
                commits: &[],
                repo_policy: None,
                extracted_rules: &[],
                reviewed_evidence: "",
                prior_findings: &[],
                retrieval_status: crate::retrieve::RetrievalStatus::Off,
                retrieved_context: "",
                redaction_note: "",
                e2e: None,
                tree: None,
                graph: None,
                pass_yield_observer: None,
            })
            .await
            .expect("runs");

        assert!(outcome.skipped.is_some());
        assert_eq!(model.calls(), 0);
    }

    #[tokio::test]
    async fn the_prompt_carries_line_numbers_so_anchors_are_read_not_counted() {
        let model = MockModel::silent();
        run_with(model.clone(), &config(), &diffs()).await;

        let prompt = model.last_prompt().expect("recorded");
        assert!(prompt.contains("2 +    let x = items[i];"), "{prompt}");
    }

    #[tokio::test]
    async fn the_configured_tier_is_the_model_actually_called() {
        let mut config = config();
        config.models.deep = "some/deep-model".into();
        let model = MockModel::silent();
        run_with(model.clone(), &config, &diffs()).await;

        assert_eq!(model.requests()[0].model, "some/deep-model");
    }

    /// A per-file lane can do better than replaying an already-reviewed file
    /// into a cacheable prefix: it can not send it at all. The cheapest call is
    /// the one not made, and a cache prefix only pays when the provider honours
    /// it.
    #[tokio::test]
    async fn a_file_reviewed_before_and_unchanged_since_is_not_reviewed_again() {
        let config = config();
        let model = MockModel::silent();
        let pr = pull_request();

        // The earlier cycle reviewed `src/earlier.rs`; this push adds
        // `src/main.rs`. Only the new file is worth a call.
        let earlier = parse_file_patch("src/earlier.rs", "@@ -1,1 +1,2 @@\n a\n+earlier\n");
        let reviewed = replay::render(std::slice::from_ref(&earlier));
        let diffs = vec![earlier, parse_file_patch("src/main.rs", PATCH)];

        Critique::new(Arc::new(model.clone()))
            .run(LaneInput {
                config: &config,
                pull_request: &pr,
                diffs: &diffs,
                file_contents: &BTreeMap::new(),
                scan_findings: &[],
                commits: &[],
                repo_policy: None,
                extracted_rules: &[],
                reviewed_evidence: &reviewed,
                prior_findings: &["Close the socket on the error path".to_string()],
                retrieval_status: crate::retrieve::RetrievalStatus::Off,
                retrieved_context: "",
                redaction_note: "",
                e2e: None,
                tree: None,
                graph: None,
                pass_yield_observer: None,
            })
            .await
            .expect("runs");

        let requests = model.requests();
        assert_eq!(requests.len(), 1, "one call, for the one unreviewed file");

        let system = &requests[0].messages[0].content;
        let user = &requests[0].messages[1].content;

        assert!(
            !system.contains("+earlier") && !user.contains("+earlier"),
            "the unchanged file is not sent at all"
        );
        assert!(user.contains("src/main.rs"), "the delta is the new work");
        assert!(
            system.contains("The file is `src/main.rs`"),
            "each conversation owns exactly one file: {system}"
        );
        assert!(
            user.contains("Close the socket"),
            "prior findings are volatile"
        );
        assert!(
            !system.contains("Close the socket"),
            "prior findings must not enter the cached prefix"
        );
    }

    /// One file per conversation, so a forty-file pull request is forty close
    /// readings rather than one that fades after the first few files.
    #[tokio::test]
    async fn every_changed_file_gets_its_own_conversation() {
        let config = config();
        let model = MockModel::silent();
        let pr = pull_request();
        let diffs = vec![
            parse_file_patch("src/main.rs", PATCH),
            parse_file_patch("src/other.rs", PATCH),
            parse_file_patch("src/third.rs", PATCH),
        ];

        Critique::new(Arc::new(model.clone()))
            .run(LaneInput {
                config: &config,
                pull_request: &pr,
                diffs: &diffs,
                file_contents: &BTreeMap::new(),
                scan_findings: &[],
                commits: &[],
                repo_policy: None,
                extracted_rules: &[],
                reviewed_evidence: "",
                prior_findings: &[],
                retrieval_status: crate::retrieve::RetrievalStatus::Off,
                retrieved_context: "",
                redaction_note: "",
                e2e: None,
                tree: None,
                graph: None,
                pass_yield_observer: None,
            })
            .await
            .expect("runs");

        let requests = model.requests();
        assert_eq!(requests.len(), 3, "one conversation per changed file");

        for (request, path) in requests
            .iter()
            .zip(["src/main.rs", "src/other.rs", "src/third.rs"])
        {
            let system = &request.messages[0].content;
            assert!(
                system.contains(&format!("The file is `{path}`")),
                "each conversation is scoped to its own file: {system}"
            );
        }
    }

    /// Malformed output still never becomes a comment. What changed with the
    /// fan-out is where the failure lands: it is isolated to its own file
    /// rather than failing the lane, so one bad response cannot delete the
    /// review of every other file. A lane where *nothing* could be reviewed
    /// must still not report success — that is the part branch protection
    /// depends on.
    #[tokio::test]
    async fn malformed_model_output_is_reported_rather_than_posted_as_nonsense() {
        let model = MockModel::new().then(json!({"summary": "…", "findings": [{"path": "x"}]}));
        let pr = pull_request();
        let config = config();
        let diffs = diffs();

        let outcome = Critique::new(Arc::new(model))
            .run(LaneInput {
                config: &config,
                pull_request: &pr,
                diffs: &diffs,
                file_contents: &BTreeMap::new(),
                scan_findings: &[],
                commits: &[],
                repo_policy: None,
                extracted_rules: &[],
                reviewed_evidence: "",
                prior_findings: &[],
                retrieval_status: crate::retrieve::RetrievalStatus::Off,
                retrieved_context: "",
                redaction_note: "",
                e2e: None,
                tree: None,
                graph: None,
                pass_yield_observer: None,
            })
            .await
            .expect("the failure is isolated, not propagated");

        assert!(outcome.findings.is_empty(), "nothing nonsensical is posted");
        assert!(
            outcome.summary.contains("src/main.rs"),
            "the file that could not be reviewed is named: {}",
            outcome.summary
        );
        assert_eq!(
            outcome.conclusion(Severity::High),
            CheckConclusion::Neutral,
            "a lane that reviewed nothing must not claim success"
        );
    }

    /// The other half of the isolation rule: one file's failure must leave the
    /// rest of the review standing.
    #[tokio::test]
    async fn a_mechanical_rename_is_verified_and_only_the_residue_reaches_a_model() {
        // opencompany#2313: fifty-one files of one substitution and one file
        // of logic. The substitution is proven line for line here, the
        // summary says so, and the model is asked about the residue alone.
        let config = config();
        let rename = |path: &str, line: &str| {
            let new = line.replace("::openhuman", "");
            parse_file_patch(path, &format!("@@ -1,1 +1,1 @@\n-{line}\n+{new}\n"))
        };
        let diffs = vec![
            rename("src/a.rs", "use openhuman_core::openhuman as oh;"),
            rename("src/b.rs", "    openhuman_core::openhuman::tools::x();"),
            rename("src/c.rs", "let y = openhuman_core::openhuman::A;"),
            parse_file_patch(
                "src/logic.rs",
                "@@ -1,1 +1,2 @@\n-use openhuman_core::openhuman as oh;\n+use openhuman_core as oh;\n+let leak = 1;\n",
            ),
        ];
        let model = MockModel::always(json!({ "summary": "read the logic", "findings": [] }));

        let outcome = run_with(model.clone(), &config, &diffs).await;

        let requests = model.requests();
        assert_eq!(
            requests.len(),
            2,
            "two conversations: the residue, and one sample of the rename"
        );
        let prompts: Vec<&str> = requests
            .iter()
            .map(|r| r.messages[1].content.as_str())
            .collect();
        assert!(prompts.iter().any(|p| p.contains("src/logic.rs")));
        assert!(
            prompts.iter().any(|p| p.contains("src/a.rs")),
            "the first verified file is the sample"
        );
        assert!(!prompts.iter().any(|p| p.contains("src/b.rs")));
        assert!(
            outcome
                .summary
                .contains("3 file(s) are the mechanical rename `::openhuman` → ``"),
            "{}",
            outcome.summary
        );
        assert!(outcome.skipped.is_none());
    }

    #[tokio::test]
    async fn a_pull_request_that_is_only_a_rename_reads_one_sample_and_reaches_a_verdict() {
        let config = config();
        let rename = |path: &str, line: &str| {
            let new = line.replace("::openhuman", "");
            parse_file_patch(path, &format!("@@ -1,1 +1,1 @@\n-{line}\n+{new}\n"))
        };
        let diffs = vec![
            rename("src/a.rs", "use openhuman_core::openhuman as oh;"),
            rename("src/b.rs", "    openhuman_core::openhuman::tools::x();"),
            rename("src/c.rs", "let y = openhuman_core::openhuman::A;"),
        ];
        let model = MockModel::always(json!({ "summary": "a rename", "findings": [] }));

        let outcome = run_with(model.clone(), &config, &diffs).await;

        assert_eq!(
            model.requests().len(),
            1,
            "one sample of the rename is read"
        );
        assert!(
            outcome.skipped.is_none(),
            "a verified rename is a verdict, not a skip"
        );
        assert!(outcome.findings.is_empty());
        assert!(outcome.summary.contains("verified line for line"));
    }

    #[tokio::test]
    async fn one_files_failure_does_not_delete_the_other_files_review() {
        let config = config();
        let diffs = vec![
            parse_file_patch("src/main.rs", PATCH),
            parse_file_patch("src/other.rs", PATCH),
        ];
        let pr = pull_request();

        let model = MockModel::new()
            // src/main.rs: unparseable.
            .then(json!({"summary": "…", "findings": [{"path": "x"}]}))
            // src/other.rs: a real finding, and a falsifier that keeps it.
            .then(json!({
                "summary": "…",
                "findings": [{
                    "path": "src/other.rs",
                    "severity": "high",
                    "confidence": 0.9,
                    "rule": "bounds",
                    "title": "Unchecked index",
                    "body": "…",
                    "existing_code": "    let x = items[i];"
                }]
            }))
            .then(json!({"incorrect": []}));

        let outcome = Critique::new(Arc::new(model))
            .run(LaneInput {
                config: &config,
                pull_request: &pr,
                diffs: &diffs,
                file_contents: &BTreeMap::new(),
                scan_findings: &[],
                commits: &[],
                repo_policy: None,
                extracted_rules: &[],
                reviewed_evidence: "",
                prior_findings: &[],
                retrieval_status: crate::retrieve::RetrievalStatus::Off,
                retrieved_context: "",
                redaction_note: "",
                e2e: None,
                tree: None,
                graph: None,
                pass_yield_observer: None,
            })
            .await
            .expect("runs");

        assert_eq!(
            outcome.findings.len(),
            1,
            "the good file was still reviewed"
        );
        assert_eq!(outcome.findings[0].path, "src/other.rs");
        assert!(
            outcome.summary.contains("src/main.rs"),
            "and the failure is not hidden: {}",
            outcome.summary
        );
    }

    #[tokio::test]
    async fn resolved_findings_are_carried_through() {
        let model = MockModel::new().then(json!({
            "summary": "Earlier issue is fixed.",
            "findings": [],
            "resolved": ["Guard the index before dereferencing"]
        }));
        let outcome = run_with(model, &config(), &diffs()).await;

        assert_eq!(
            outcome.resolved,
            vec!["Guard the index before dereferencing"]
        );
    }

    // --- grouping -----------------------------------------------------------

    /// A file and its underscore test sibling: grouped by name alone, with no
    /// graph, by `lanes::grouping`.
    fn grouped_diffs() -> Vec<FileDiff> {
        vec![
            parse_file_patch(
                "src/widget.rs",
                "@@ -1,1 +1,2 @@\n fn widget() {}\n+    let w = items[i];\n",
            ),
            parse_file_patch(
                "src/widget_test.rs",
                "@@ -1,1 +1,2 @@\n fn widget_test() {}\n+    let t = cases[j];\n",
            ),
        ]
    }

    #[tokio::test]
    async fn grouping_reduces_call_count_for_a_file_and_its_test() {
        let model = MockModel::silent();
        run_with(model.clone(), &config(), &grouped_diffs()).await;

        assert_eq!(
            model.calls(),
            1,
            "one conversation for the file and its test, not two"
        );
    }

    #[tokio::test]
    async fn the_isolation_clause_names_every_file_in_the_group() {
        let model = MockModel::silent();
        run_with(model.clone(), &config(), &grouped_diffs()).await;

        let system = &model.requests()[0].messages[0].content;
        assert!(system.contains("These files only"), "{system}");
        // Fenced as untrusted data, one path per line, not backtick-wrapped
        // prose — see `harness::prompt::isolation_clause`'s group arm.
        assert!(system.contains("src/widget.rs"), "{system}");
        assert!(system.contains("src/widget_test.rs"), "{system}");
        assert!(system.contains("untrusted"), "{system}");
    }

    #[tokio::test]
    async fn a_grouped_finding_anchors_to_the_file_it_names_not_the_first_file_in_the_group() {
        let model = MockModel::new().then(json!({
            "summary": "…",
            "findings": [{
                "path": "src/widget_test.rs",
                "existing_code": "let t = cases[j];",
                "rule": "unchecked-index",
                "title": "Guard the index before dereferencing",
                "body": "`j` is never bounds-checked.",
                "severity": "high", "confidence": 0.9
            }]
        }));
        let outcome = run_with(model, &config(), &grouped_diffs()).await;

        assert_eq!(outcome.findings.len(), 1, "{:#?}", outcome.findings);
        assert_eq!(outcome.findings[0].path, "src/widget_test.rs");
        assert_eq!(
            outcome.findings[0].line,
            Some(2),
            "anchored against its own file's diff, not the group's first file"
        );
    }

    #[tokio::test]
    async fn a_finding_naming_a_path_outside_the_group_is_discarded_like_an_untouched_file() {
        let model = MockModel::new().then(json!({
            "summary": "…",
            "findings": [{
                "path": "src/elsewhere.rs",
                "existing_code": "let w = items[i];",
                "rule": "r", "title": "t", "body": "b",
                "severity": "high", "confidence": 0.9
            }]
        }));
        let outcome = run_with(model, &config(), &grouped_diffs()).await;

        assert!(outcome.findings.is_empty());
        assert!(
            outcome.summary.contains("did not change"),
            "{}",
            outcome.summary
        );
    }

    #[tokio::test]
    async fn grouping_disabled_falls_back_to_per_file_fanout() {
        let mut config = config();
        config.grouping.enabled = false;
        let model = MockModel::silent();
        run_with(model.clone(), &config, &grouped_diffs()).await;

        let requests = model.requests();
        assert_eq!(
            requests.len(),
            2,
            "grouping off is the plain one-conversation-per-file fan-out"
        );

        // Byte-identical to the pre-grouping single-file prompt: the same
        // isolation clause text, naming only that file, with no group
        // language at all — a cassette or a provider's cached prefix from
        // before grouping existed must still match.
        for (request, path) in requests.iter().zip(["src/widget.rs", "src/widget_test.rs"]) {
            let system = &request.messages[0].content;
            assert!(system.contains("## One file only"), "{system}");
            assert!(
                system.contains(&format!("The file is `{path}`.")),
                "{system}"
            );
            assert!(!system.contains("These files only"), "{system}");
        }
    }
}
