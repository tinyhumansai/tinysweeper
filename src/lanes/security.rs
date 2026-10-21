//! The `security` lane: what this pull request makes newly attackable.
//!
//! Two halves, and the order between them is the design:
//!
//! 1. The deterministic scanners have **already run**, before any token was
//!    spent. Their findings are facts. This lane republishes them unchanged and
//!    hands them to the model as evidence to *adjudicate* — say whether each is
//!    real here and why — rather than asking a model to re-derive them. A
//!    regular expression that finds a widened workflow permission is cheaper,
//!    faster and more certain than a paragraph asking a model to look for one,
//!    and duplicating the scanner in the prompt would produce two opinions
//!    about one fact.
//! 2. The model then looks for what a scanner cannot see: untrusted input
//!    reaching a dangerous sink, an authorisation check that moved, a new
//!    subprocess or deserialization site. Strictly anchored model proposals
//!    are falsified against the diff and repository evidence they looked up,
//!    before the scanner facts are merged back unchanged.
//!
//! A model verdict never *removes* a scanner finding. Adjudication adds
//! context; it does not get to overrule a deterministic match, because the
//! failure mode of a model talking itself out of a real finding is exactly the
//! one this lane exists to prevent.
//!
//! It fans out one conversation per changed file — see `lanes::fanout` for why.

use std::fmt::Write as _;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;

use crate::config::types::LaneId;
use crate::council;
use crate::error::Result;
use crate::evidence::diff::{FileDiff, render as render_diffs};
use crate::falsify::Falsifier;
use crate::findings::types::Finding;
use crate::flows::panel::Call;
use crate::flows::runner;
use crate::harness::prompt::{self, PromptInputs};
use crate::harness::schema;
use crate::lanes::fanout::{FileReview, per_unit};
use crate::lanes::grouping::{FileGroup, GroupBounds};
use crate::lanes::mechanical;
use crate::lanes::preparation::LanePreparationInput;
use crate::lanes::triage::triage;
use crate::lanes::{
    Anchoring, Lane, LaneInput, LaneOutcome, aggregate_reviewer_responses, reviewer_responses,
};
use crate::ports::model::{Model, Spend, Usage};
use crate::scan::types::{Finding as ScanFinding, ScanKind};

/// The scanner findings this lane owns.
///
/// A partition, not an overlap: `commits` adjudicates secrets, blobs and junk,
/// and two lanes discussing one scanner match would double-report it.
pub const ADJUDICATES: [ScanKind; 2] = [ScanKind::Workflow, ScanKind::Dependency];

/// The security lane.
pub struct Security {
    model: Arc<dyn Model>,
}

impl Security {
    /// Build the lane over `model`.
    pub fn new(model: Arc<dyn Model>) -> Self {
        Self { model }
    }
}

/// The model-free part of preparing one security lane.
pub(crate) struct SecurityPreparation<'a> {
    /// An early content/draft skip, if this lane has no scanner work either.
    pub(crate) skipped: Option<LaneOutcome>,
    /// The deterministic findings this lane must adjudicate and republish.
    pub(crate) scanner: Vec<&'a ScanFinding>,
    /// Groups in the exact triage order the existing security runner reviews.
    pub(crate) groups: Vec<FileGroup>,
    /// Previously reviewed paths proven without a current model response.
    pub(crate) covered_without_model: Vec<String>,
    /// A deterministic substitution proved by the changed lines, if any.
    pub(crate) mechanical: Option<mechanical::Substitution>,
    /// Paths verified mechanically without sending them to a model.
    pub(crate) mechanical_verified: Vec<String>,
    /// Paths intentionally skipped by security triage, with their reasons.
    pub(crate) triage_skipped: Vec<(String, &'static str)>,
    /// Whether triage left no paths for a model response.
    pub(crate) review_paths_empty: bool,
}

/// Prepare security groups without constructing prompts or calling a model.
pub(crate) fn prepare<'a>(input: LanePreparationInput<'a>) -> SecurityPreparation<'a> {
    let scanner = input.scanner_findings_of(&ADJUDICATES);
    // Scanner evidence remains actionable even when the patch only deletes
    // lines; for example, a removed permission guard can widen access.
    if !input.has_reviewable_content() && scanner.is_empty() {
        return SecurityPreparation {
            skipped: Some(LaneOutcome::skipped(
                "No added or modified lines to review.",
            )),
            scanner,
            groups: Vec::new(),
            covered_without_model: Vec::new(),
            mechanical: None,
            mechanical_verified: Vec::new(),
            triage_skipped: Vec::new(),
            review_paths_empty: true,
        };
    }

    if let Some(skipped) = input.skip_as_draft() {
        return SecurityPreparation {
            skipped: Some(skipped),
            scanner,
            groups: Vec::new(),
            covered_without_model: Vec::new(),
            mechanical: None,
            mechanical_verified: Vec::new(),
            triage_skipped: Vec::new(),
            review_paths_empty: true,
        };
    }

    // Keep matched files in scope even if replay or mechanical analysis would
    // otherwise omit them: the scanner fact persists until the risky code is
    // removed from the tree.
    let forced: Vec<&str> = scanner
        .iter()
        .map(|finding| finding.path.as_str())
        .collect();
    let fresh: Vec<FileDiff> =
        crate::evidence::replay::unreviewed(input.reviewed_evidence, input.diffs)
            .into_iter()
            .cloned()
            .collect();
    let fresh_paths: Vec<&str> = fresh.iter().map(|diff| diff.path.as_str()).collect();
    let replayed: Vec<FileDiff> = input
        .diffs
        .iter()
        .filter(|diff| !diff.changed_lines.is_empty() && !fresh_paths.contains(&diff.path.as_str()))
        .cloned()
        .collect();
    let covered_without_model = triage(&replayed, &forced).review;
    let forced_back: Vec<FileDiff> = input
        .diffs
        .iter()
        .filter(|diff| forced.contains(&diff.path.as_str()))
        .filter(|diff| !fresh.iter().any(|fresh| fresh.path == diff.path))
        .cloned()
        .collect();
    let mut considered = fresh;
    considered.extend(forced_back);
    let mut triaged = triage(&considered, &forced);

    let mechanical = mechanical::detect(&considered.iter().collect::<Vec<_>>());
    let candidate_paths = triaged.review.clone();
    let mut mechanical_verified = Vec::new();
    if let Some(substitution) = &mechanical {
        // Scanner-forced paths stay reviewable even when the same substitution
        // is mechanically verified for other files.
        triaged.review.retain(|path| {
            forced.contains(&path.as_str()) || !substitution.verified.contains(path)
        });
        mechanical_verified.extend(
            substitution
                .verified
                .iter()
                .filter(|path| {
                    candidate_paths.contains(path)
                        && !triaged.review.contains(path)
                        && !forced.contains(&path.as_str())
                })
                .cloned(),
        );
    }

    if triaged.review.is_empty()
        && scanner.is_empty()
        && covered_without_model.is_empty()
        && mechanical_verified.is_empty()
    {
        return SecurityPreparation {
            skipped: Some(LaneOutcome::skipped(format!(
                "No changed file has any attack surface.{}",
                skip_note(&triaged.skipped)
            ))),
            scanner,
            groups: Vec::new(),
            covered_without_model,
            mechanical,
            mechanical_verified,
            triage_skipped: triaged.skipped,
            review_paths_empty: true,
        };
    }

    let groups = if input.config.grouping.enabled {
        input.group(
            &triaged.review,
            &GroupBounds {
                max_files: input.config.grouping.max_files,
                max_hunk_chars: input.config.grouping.max_hunk_chars,
            },
        )
    } else {
        triaged
            .review
            .iter()
            .map(|path| FileGroup {
                label: path.clone(),
                paths: vec![path.clone()],
            })
            .collect()
    };

    SecurityPreparation {
        skipped: None,
        scanner,
        groups,
        covered_without_model,
        mechanical,
        mechanical_verified,
        triage_skipped: triaged.skipped,
        review_paths_empty: triaged.review.is_empty(),
    }
}

#[async_trait]
impl Lane for Security {
    fn id(&self) -> LaneId {
        LaneId::Security
    }

    async fn run(&self, input: LaneInput<'_>) -> Result<LaneOutcome> {
        let prepared = prepare(input.preparation());
        self.run_prepared(input, prepared).await
    }
}

impl Security {
    /// Review one planner-selected group, retaining scanner evidence for its members.
    pub(crate) async fn review_selected_group<'a>(
        llm: Arc<crate::flows::caps::ModelCapability>,
        input: &LaneInput<'a>,
        prepared: &SecurityPreparation<'a>,
        group: &FileGroup,
    ) -> Result<FileReview> {
        let changed_paths = input.changed_paths();
        let group_diffs: Vec<FileDiff> = group
            .paths
            .iter()
            .filter_map(|path| input.diffs.iter().find(|diff| &diff.path == path).cloned())
            .collect();
        let scanner: Vec<&ScanFinding> = prepared
            .scanner
            .iter()
            .copied()
            .filter(|finding| group.paths.contains(&finding.path))
            .collect();
        let review = review_group(
            llm.clone(),
            input.config,
            input.repo_policy,
            input.extracted_rules,
            input.prior_findings,
            input.retrieved_context,
            input.redaction_note,
            input.asking_about_group(&group_diffs),
            &changed_paths,
            &group.paths,
            &group_diffs,
            &scanner,
            input.pass_yield_observer,
        )
        .await?;
        Ok(review)
    }

    /// Apply scanner, triage and mechanical results after selected group reviews.
    pub(crate) fn finish_selected<'a>(
        prepared: SecurityPreparation<'a>,
        mut outcome: LaneOutcome,
        llm: Arc<crate::flows::caps::ModelCapability>,
    ) -> Result<LaneOutcome> {
        if let Some(skipped) = prepared.skipped {
            return Ok(skipped);
        }
        outcome.spend.merge(llm.spend());
        let mut covered_without_model = prepared.covered_without_model;
        covered_without_model.extend(prepared.mechanical_verified);
        outcome.include_reviewed_paths(covered_without_model);
        outcome
            .summary
            .push_str(&skip_note(&prepared.triage_skipped));
        if let Some(substitution) = &prepared.mechanical {
            outcome.summary = format!(
                "{} {}",
                outcome.summary.trim(),
                mechanical::note(substitution)
            );
            if prepared.review_paths_empty {
                outcome.skipped = None;
            }
        }
        merge_scanner_findings(&mut outcome, &prepared.scanner);
        if !prepared.scanner.is_empty() {
            // A scanner fact remains actionable even when every selected
            // model group was omitted or failed; unanswered work still keeps
            // non-blocking outcomes neutral in the proposal gate.
            outcome.skipped = None;
        }
        Ok(outcome)
    }

    /// Run the exact model-free candidate preparation already used by planning.
    pub(crate) async fn run_prepared<'a>(
        &self,
        input: LaneInput<'a>,
        prepared: SecurityPreparation<'a>,
    ) -> Result<LaneOutcome> {
        self.run_prepared_with_execution(input, prepared, None)
            .await
    }

    /// Run the prepared lane under its original review-wide stop window.
    pub(crate) async fn run_prepared_with_execution<'a>(
        &self,
        input: LaneInput<'a>,
        prepared: SecurityPreparation<'a>,
        execution: Option<Arc<crate::app::review_execution::ReviewExecution>>,
    ) -> Result<LaneOutcome> {
        if let Some(skipped) = prepared.skipped {
            return Ok(skipped);
        }
        let scanner = prepared.scanner;
        let groups = prepared.groups;
        let mut covered_without_model = prepared.covered_without_model;
        let mechanical_verified = prepared.mechanical_verified;
        let mechanical = prepared.mechanical;
        let triage_skipped = prepared.triage_skipped;
        let review_paths_empty = prepared.review_paths_empty;

        // One capability for the whole lane, so the pull-request budget holds
        // across every file and every reviewer at once — which is what lets the
        // groups run concurrently rather than one at a time.
        let llm = runner::reviewer_lane_llm(
            self.model.clone(),
            input.config,
            input.config.models.budget_usd_per_pr,
        );

        let changed_paths = input.changed_paths();

        let review = |group: crate::lanes::grouping::FileGroup| {
            let llm = llm.clone();
            let config = input.config;
            let repo_policy = input.repo_policy;
            let extracted_rules = input.extracted_rules;
            let prior_findings = input.prior_findings;
            let retrieved_context = input.retrieved_context;
            let redaction_note = input.redaction_note;
            let pass_yield_observer = input.pass_yield_observer;
            let input = &input;
            let diffs = input.diffs;
            let scanner = &scanner;
            let changed_paths = &changed_paths;
            async move {
                let group_diffs: Vec<FileDiff> = group
                    .paths
                    .iter()
                    .filter_map(|path| diffs.iter().find(|d| &d.path == path).cloned())
                    .collect();
                let asking = input.asking_about_group(&group_diffs);
                review_group(
                    llm,
                    config,
                    repo_policy,
                    extracted_rules,
                    prior_findings,
                    retrieved_context,
                    redaction_note,
                    asking,
                    changed_paths,
                    &group.paths,
                    &group_diffs,
                    scanner,
                    pass_yield_observer,
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

        let mut outcome = outcome.into_outcome();
        covered_without_model.extend(mechanical_verified);
        outcome.include_reviewed_paths(covered_without_model);
        // Tallied inside the capability, which every graph call passes through.
        // Folded in once rather than per file: `llm` is shared across the
        // fan-out, so per-file would multiply the bill by the file count.
        outcome.spend.merge(llm.spend());
        outcome.summary.push_str(&skip_note(&triage_skipped));
        if let Some(sub) = &mechanical {
            outcome.summary = format!("{} {}", outcome.summary.trim(), mechanical::note(sub));
            if review_paths_empty {
                outcome.skipped = None;
            }
        }
        merge_scanner_findings(&mut outcome, &scanner);
        if !scanner.is_empty() {
            // A confirmed scanner finding remains actionable when every
            // selected model group was omitted at the execution cutoff.
            // Keep the partial coverage and omission metadata on this outcome.
            outcome.skipped = None;
        }
        Ok(outcome)
    }
}

/// Review one group of related changed files, in a conversation that knows
/// about no file outside it.
///
// Every argument is one prompt layer, and they are passed individually rather
// than as a context struct because each has a different trust level — see
// `harness::prompt`. Bundling them would make it easy to route the untrusted
// ones to the wrong half of the prompt.
#[allow(clippy::too_many_arguments)]
async fn review_group(
    llm: std::sync::Arc<crate::flows::caps::ModelCapability>,
    config: &crate::config::types::Config,
    repo_policy: Option<&str>,
    extracted_rules: &[String],
    prior_findings: &[String],
    retrieved_context: &str,
    redaction_note: &str,
    asking: runner::Asking<'_>,
    changed_paths: &[String],
    group_paths: &[String],
    group_diffs: &[FileDiff],
    scanner: &[&ScanFinding],
    pass_yield_observer: Option<&dyn crate::lanes::PassYieldObserver>,
) -> Result<FileReview> {
    let evidence = render_diffs(group_diffs);
    let scanner_evidence = render_scanner(scanner, group_paths);

    let base_inputs = PromptInputs {
        repo_policy,
        extracted_rules,
        prior_findings,
        new_evidence: &evidence,
        changed_paths,
        focus_paths: group_paths,
        scanner_evidence: &scanner_evidence,
        retrieved_context,
        redaction_note,
        ..PromptInputs::new(LaneId::Security, config)
    };
    let built = prompt::build(&base_inputs);

    // Every reviewer at once, as one concurrent round. With no council configured this is
    // the single default reviewer on the lane's own model, so a solo run and a
    // council run are one code path.
    let reviewers = council::reviewers(config, LaneId::Security);
    let calls: Vec<Call> = reviewers
        .iter()
        .map(|reviewer| Call {
            id: reviewer.id.to_string(),
            model: reviewer.model.to_string(),
            system: built.prefix().to_string(),
            prompt: built.suffix().to_string(),
            schema_name: "tinysweeper_security".into(),
        })
        .collect();

    let round_one = runner::ask_all_accounted(
        llm.clone(),
        LaneId::Security,
        &calls,
        &schema::json_schema(),
        asking,
    )
    .await?;
    let round_one_usage = round_one.usage;
    let round_one_elapsed = round_one.elapsed;
    let answers = round_one.answers;

    // A group whose every reviewer failed is a group nobody read. Failing here
    // is what puts it in the fan-out's failure list, where the summary names
    // it — the alternative is an unreviewed group that reads as clean.
    let responses = reviewer_responses(LaneId::Security, &reviewers, &answers)?;
    let mut looked_up = responses
        .iter()
        .map(|response| response.looked_up.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    let Some(outcome) = aggregate_reviewer_responses(
        LaneId::Security,
        responses,
        group_diffs,
        Anchoring::Strict,
        config.council.corroboration,
    ) else {
        return Err(crate::error::Error::lane(
            LaneId::Security.as_str(),
            format!("no reviewer could review {}", group_paths.join(" + ")),
        ));
    };

    // Only model proposals enter this filter. Scanner facts are merged later
    // and never depend on a model's ability to confirm them.
    let round_one_candidates = outcome.findings.len();
    let filtered = Falsifier::new(llm.model().as_ref(), config)
        .filter_with(LaneId::Security, outcome.findings, &evidence, &looked_up)
        .await;
    let mut findings = filtered.findings;
    let round_one_survivors = findings.len();
    let mut execution_refusal = filtered.execution_refusal;
    let mut rejected = filtered.rejected;
    let mut spend = outcome.spend;
    spend.merge(filtered.spend);
    let mut metrics = crate::lanes::coverage::Metrics::start(
        round_one_usage,
        round_one_elapsed,
        round_one_candidates,
        round_one_survivors,
    );
    if execution_refusal.is_none()
        && let Some(observer) = pass_yield_observer
    {
        observer.record_pass_yield(
            LaneId::Security,
            1,
            round_one_candidates,
            round_one_survivors,
        );
    }

    // Adaptive coverage uses the same strict anchoring and falsification as
    // round one, judging only new proposals against the evidence already read.
    let mut added_by_coverage = 0usize;
    if execution_refusal.is_none()
        && config.review.passes > 1
        && (round_one_candidates > 0 || group_diffs.iter().any(crate::lanes::triage::high_risk))
    {
        let mut confirmed = findings.clone();

        for pass_index in 1..config.review.passes {
            let reviewer = &reviewers[0];
            let confirmed_lines = crate::lanes::coverage::confirmed_lines(&confirmed);
            let built = prompt::build(&PromptInputs {
                confirmed_this_round: &confirmed_lines,
                coverage_pass: true,
                ..base_inputs.clone()
            });

            let coverage = match crate::lanes::coverage::coverage_pass(
                llm.clone(),
                LaneId::Security,
                reviewer,
                &built,
                &schema::json_schema(),
                "tinysweeper_security",
                asking,
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
                    // The graph returned no scoped accounting. Count the
                    // attempt, but do not borrow usage from the lane-wide
                    // cumulative tally shared by concurrent groups.
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
            looked_up.push('\n');
            looked_up.push_str(&coverage.looked_up);

            if let Some(refusal) = coverage.execution_refusal {
                execution_refusal = Some(refusal);
                metrics.record(coverage_usage, coverage_elapsed, 0, 0);
                metrics.stop(crate::lanes::coverage::StopReason::Refused);
                break;
            }

            let Some(response) = coverage.response else {
                metrics.record(coverage_usage, coverage_elapsed, 0, 0);
                metrics.stop(crate::lanes::coverage::StopReason::Failed);
                break;
            };
            let had_raw_proposals = !response.findings.is_empty();

            let anchored = LaneOutcome::from_response(
                LaneId::Security,
                response,
                group_diffs,
                Anchoring::Strict,
                Spend::default(),
            );

            // Same dedupe as critique's coverage pass: drop anything that
            // corroborates, or fingerprints identically to, a finding already
            // confirmed this unit.
            let had_proposals = !anchored.findings.is_empty();
            let mut new_findings: Vec<Finding> = anchored
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
            for finding in &mut new_findings {
                finding.review_pass = pass_index + 1;
            }

            if new_findings.is_empty() {
                if let Some(observer) = pass_yield_observer {
                    observer.record_pass_yield(LaneId::Security, pass_index + 1, 0, 0);
                }
                metrics.record(coverage_usage, coverage_elapsed, 0, 0);
                metrics.stop(if !had_proposals && had_raw_proposals {
                    crate::lanes::coverage::StopReason::PlacementFailure
                } else if had_proposals {
                    crate::lanes::coverage::StopReason::Duplicate
                } else {
                    crate::lanes::coverage::StopReason::Empty
                });
                break;
            }

            let candidate_count = new_findings.len();
            let filtered = Falsifier::new(llm.model().as_ref(), config)
                .filter_with(LaneId::Security, new_findings, &evidence, &looked_up)
                .await;
            spend.merge(filtered.spend);
            rejected.extend(filtered.rejected);
            if let Some(refusal) = filtered.execution_refusal {
                execution_refusal = Some(refusal);
                let survivors = filtered.findings.len();
                findings.extend(filtered.findings);
                metrics.record(coverage_usage, coverage_elapsed, candidate_count, survivors);
                metrics.stop(crate::lanes::coverage::StopReason::Failed);
                break;
            }
            let new_findings = filtered.findings;
            if let Some(observer) = pass_yield_observer {
                observer.record_pass_yield(
                    LaneId::Security,
                    pass_index + 1,
                    candidate_count,
                    new_findings.len(),
                );
            }
            if new_findings.is_empty() {
                metrics.record(coverage_usage, coverage_elapsed, candidate_count, 0);
                metrics.stop(crate::lanes::coverage::StopReason::NonSurviving);
                break;
            }

            metrics.record(
                coverage_usage,
                coverage_elapsed,
                candidate_count,
                new_findings.len(),
            );
            added_by_coverage += new_findings.len();
            confirmed.extend(new_findings.clone());
            findings.extend(new_findings);
        }
    }
    metrics.emit(LaneId::Security, group_paths);

    // The reviewer wrote its summary before filtering. Even a partial
    // rejection can make that prose assert a bug we have now disproved.
    let summary = if rejected.is_empty() {
        outcome.summary
    } else if findings.is_empty() {
        "No model findings survived falsification.".to_string()
    } else {
        format!(
            "{} model finding(s) survived falsification.",
            findings.len()
        )
    };
    Ok(FileReview {
        summary: coverage_note(&summary, added_by_coverage),
        findings,
        resolved: outcome.resolved,
        spend,
        execution_refusal,
    })
}

/// Say, in the summary, when adaptive coverage found something round
/// one had not.
///
/// Round one's summary is written before the coverage pass ever runs, so on
/// its own it can say "nothing to report" for a group that, findings-wise,
/// no longer means that — a single-group review can fail on a coverage
/// finding while the summary still declares it clean. Rather than trying to
/// detect and rewrite round one's own prose, this appends a plain count in
/// the same parenthetical style `critique::summarise` uses for its own
/// bookkeeping notes, so the mismatch is visible instead of silent.
fn coverage_note(summary: &str, added_by_coverage: usize) -> String {
    if added_by_coverage == 0 {
        return summary.to_string();
    }
    format!(
        "{} ({added_by_coverage} finding{} added by a second pass)",
        summary.trim(),
        if added_by_coverage == 1 { "" } else { "s" }
    )
}

/// Say, in the summary, which files were never sent to a model and why.
///
/// A review that quietly skipped half the pull request reads exactly like one
/// that found nothing wrong with it, so the skip is always stated.
fn skip_note(skipped: &[(String, &'static str)]) -> String {
    if skipped.is_empty() {
        return String::new();
    }
    const LISTED: usize = 5;
    let mut names: Vec<String> = skipped
        .iter()
        .take(LISTED)
        .map(|(path, reason)| format!("{path} ({reason})"))
        .collect();
    if skipped.len() > LISTED {
        names.push(format!("and {} more", skipped.len() - LISTED));
    }
    format!(
        " {} file{} not security-reviewed: {}.",
        skipped.len(),
        if skipped.len() == 1 { " was" } else { "s were" },
        names.join(", ")
    )
}

/// Render scanner findings for adjudication, by type and location only.
///
/// `paths` restricts the findings shown to those files — a group's own paths,
/// so a conversation is not shown a scanner match for a file another
/// conversation owns. Empty renders every finding, which is what a
/// whole-pull-request caller with no group of its own wants.
///
/// `redacted_hint` is the only thing from the match itself that is ever shown,
/// and the scanner already guaranteed it carries no entropy. The value has no
/// route into this string because [`ScanFinding`] has nowhere to keep it.
pub(crate) fn render_scanner(findings: &[&ScanFinding], paths: &[String]) -> String {
    let mut out = String::new();
    for finding in findings {
        if !paths.is_empty() && !paths.iter().any(|p| p == &finding.path) {
            continue;
        }
        let location = match finding.line {
            Some(line) => format!("{}:{line}", finding.path),
            None => finding.path.clone(),
        };
        let _ = writeln!(
            out,
            "- [{}] {location} — {} ({}): {}",
            finding.kind.as_str(),
            finding.rule,
            finding.severity,
            finding.title
        );
        if let Some(hint) = &finding.redacted_hint {
            let _ = writeln!(out, "  matched: {hint}");
        }
    }
    out
}

/// Republish the scanner findings this lane owns, and drop any model finding
/// that merely restates one.
///
/// The scanner half is unconditional. A model that decides a deterministic
/// match is a false positive is welcome to say so in its summary; it does not
/// get to delete the finding, because "the model was talked out of it" is not a
/// failure mode anyone can audit.
pub(crate) fn merge_scanner_findings(outcome: &mut LaneOutcome, scanner: &[&ScanFinding]) {
    let mut deterministic: Vec<Finding> = scanner
        .iter()
        .map(|scan| {
            let mut finding = Finding::from((*scan).clone());
            finding.lane = LaneId::Security;
            finding
        })
        .collect();

    // Same file, same rule, already reported deterministically.
    outcome.findings.retain(|model| {
        !deterministic
            .iter()
            .any(|scan| scan.path == model.path && scan.rule == model.rule)
    });

    deterministic.append(&mut outcome.findings);
    outcome.findings = deterministic;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::types::{Config, PathInstruction, Severity};
    use crate::evidence::diff::parse_file_patch;
    use crate::forge::types::PullRequest;
    use crate::harness::mock::MockModel;
    use serde_json::json;
    use std::collections::BTreeMap;

    fn config() -> Config {
        let mut config: Config = crate::config::DEFAULTS
            .parse::<toml::Table>()
            .unwrap()
            .try_into()
            .unwrap();
        config.review.passes = 1;
        config
    }

    #[tokio::test]
    async fn one_pass_empty_security_group_records_first_pass_yield() {
        let mut config = config();
        config.review.lanes = vec!["security".into()];
        let model = MockModel::new().then(json!({
            "summary": "No security findings.",
            "findings": []
        }));
        let timing =
            crate::review_timing::ReviewTiming::new("tinyhumansai/tinysweeper", 7, "Incremental");
        let pull_request = pull_request();
        Security::new(Arc::new(model))
            .run(LaneInput {
                config: &config,
                pull_request: &pull_request,
                diffs: &diffs(),
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
            .expect("security lane runs");

        let yield_one = timing
            .snapshot()
            .pass_yields
            .get("security:1")
            .copied()
            .expect("one-pass security yield is recorded");
        assert_eq!(yield_one.groups, 1);
        assert_eq!(yield_one.candidates, 0);
        assert_eq!(yield_one.survivors, 0);
    }

    const PATCH: &str = "@@ -1,3 +1,5 @@\n fn handler(req: Request) {\n+    let cmd = req.query(\"cmd\");\n+    Command::new(\"sh\").arg(\"-c\").arg(cmd).spawn();\n }\n";

    fn diffs() -> Vec<FileDiff> {
        vec![parse_file_patch("src/handler.rs", PATCH)]
    }

    fn pull_request() -> PullRequest {
        PullRequest {
            number: 7,
            title: "feat: add a handler".into(),
            head_sha: "abc123".into(),
            ..PullRequest::default()
        }
    }

    async fn run_with(
        model: MockModel,
        config: &Config,
        diffs: &[FileDiff],
        scan_findings: &[ScanFinding],
    ) -> LaneOutcome {
        run_with_reviewed(model, config, diffs, scan_findings, "").await
    }

    async fn run_with_reviewed(
        model: MockModel,
        config: &Config,
        diffs: &[FileDiff],
        scan_findings: &[ScanFinding],
        reviewed_evidence: &str,
    ) -> LaneOutcome {
        run_with_context(model, config, diffs, scan_findings, reviewed_evidence, "").await
    }

    async fn run_with_context(
        model: MockModel,
        config: &Config,
        diffs: &[FileDiff],
        scan_findings: &[ScanFinding],
        reviewed_evidence: &str,
        redaction_note: &str,
    ) -> LaneOutcome {
        let pr = pull_request();
        Security::new(Arc::new(model))
            .run(LaneInput {
                config,
                pull_request: &pr,
                diffs,
                file_contents: &BTreeMap::new(),
                scan_findings,
                commits: &[],
                repo_policy: None,
                extracted_rules: &[],
                reviewed_evidence,
                prior_findings: &[],
                retrieval_status: crate::retrieve::RetrievalStatus::Off,
                retrieved_context: "",
                redaction_note,
                e2e: None,
                tree: None,
                graph: None,
                pass_yield_observer: None,
            })
            .await
            .expect("lane runs")
    }

    fn workflow_finding() -> ScanFinding {
        ScanFinding::new(
            ScanKind::Workflow,
            Severity::High,
            ".github/workflows/ci.yml",
            "workflow-write-all",
            "Narrow the workflow's permissions",
            "`permissions: write-all` grants far more than the job needs.",
        )
        .at_line(3)
    }

    #[test]
    fn scanner_blocking_fact_survives_an_all_omitted_large_fanout() {
        let finding = workflow_finding();
        let prepared = SecurityPreparation {
            skipped: None,
            scanner: vec![&finding],
            groups: Vec::new(),
            covered_without_model: Vec::new(),
            mechanical: None,
            mechanical_verified: Vec::new(),
            triage_skipped: Vec::new(),
            review_paths_empty: false,
        };
        let outcome = LaneOutcome {
            summary: "No files received a complete review.".into(),
            skipped: Some("all groups were omitted".into()),
            unanswered: vec![finding.path.clone()],
            omissions: vec![crate::lanes::fanout::OmittedGroup {
                paths: vec![finding.path.clone()],
                reason: crate::lanes::fanout::OmissionReason::DrainDeadline,
            }],
            ..LaneOutcome::default()
        };

        let outcome = Security::finish_selected(
            prepared,
            outcome,
            crate::flows::runner::lane_llm(
                Arc::new(MockModel::silent()),
                &config(),
                config().models.budget_usd_per_pr,
            ),
        )
        .expect("scanner finalization is deterministic");

        assert!(outcome.skipped.is_none());
        assert_eq!(
            outcome.conclusion(Severity::High),
            crate::forge::types::CheckConclusion::Failure
        );
        assert_eq!(outcome.findings.len(), 1);
        assert_eq!(outcome.omissions.len(), 1);
    }

    /// A synthetic patch large enough to exercise multi-pass prompt handling.
    fn large_patch() -> String {
        let mut patch = String::from("@@ -1,2 +1,42 @@\n fn handler(req: Request) {\n");
        for i in 0..40 {
            patch.push_str(&format!("+    let x{i} = {i};\n"));
        }
        patch.push_str(" }\n");
        patch
    }

    fn large_diffs() -> Vec<FileDiff> {
        vec![parse_file_patch("src/large.rs", &large_patch())]
    }

    fn config_with_passes(passes: u8) -> Config {
        let mut config = config();
        config.review.passes = passes;
        config
    }

    fn finding_at_line(title: &str, line: u64) -> serde_json::Value {
        json!({
            "path": "src/large.rs",
            "line": line,
            "rule": "unchecked-index",
            "title": title,
            "body": "detail.",
            "severity": "high",
            "confidence": 0.9
        })
    }

    #[tokio::test]
    async fn a_small_group_with_a_candidate_gets_an_adaptive_pass() {
        let mut candidate = finding_at_line("Review the changed behavior", 2);
        candidate["path"] = json!("src/ordinary.rs");
        let model = MockModel::new()
            .then(json!({"summary":"Potential issue.", "findings":[candidate]}))
            .then(json!({"incorrect":[], "security_scope":[{
                "index":1, "verdict":"in_scope", "attacker_input":"request input",
                "dangerous_operation":"ordinary operation", "security_impact":"unexpected access",
                "reason":"The claim describes a changed trust boundary."}]}))
            .then(json!({"summary":"Nothing further.", "findings":[]}));
        let handle = model.clone();
        let low_risk = vec![parse_file_patch(
            "src/ordinary.rs",
            "@@ -1,1 +1,2 @@\n fn ordinary() {}\n+    let value = 1;\n",
        )];

        run_with(model, &config_with_passes(2), &low_risk, &[]).await;

        assert_eq!(
            handle.calls(),
            3,
            "review, falsification, and adaptive review"
        );
    }

    #[tokio::test]
    async fn a_coverage_pass_runs_once_more_above_the_threshold() {
        let model = MockModel::new()
            .then(json!({
                "summary": "…",
                "findings": [finding_at_line("Guard the first index", 5)]
            }))
            .then(json!({"incorrect": []}))
            .then(json!({"summary": "…", "findings": []}));
        let handle = model.clone();

        run_with(model, &config_with_passes(2), &large_diffs(), &[]).await;

        assert_eq!(
            handle.calls(),
            3,
            "round one, its falsification, and the empty coverage pass"
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
    async fn security_review_request_uses_its_custom_reviewer_effort() {
        let model = MockModel::new().then(json!({
            "summary": "No security findings.",
            "findings": []
        }));
        let handle = model.clone();
        let mut config = config();
        config.models.reasoning_effort = "high".into();
        config.models.reviewer_reasoning_effort = "low".into();

        run_with(model, &config, &diffs(), &[]).await;

        let requests = handle.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].reasoning_effort.as_deref(), Some("low"));
    }

    #[tokio::test]
    async fn adaptive_requests_keep_path_rules_and_redaction_guidance() {
        let mut config = config_with_passes(2);
        config.path_instructions = vec![
            PathInstruction {
                glob: "src/auth.rs".into(),
                instructions: "Treat request-derived shell arguments as tainted.".into(),
                rules: None,
                lanes: vec![LaneId::Security],
                merge: false,
            },
            PathInstruction {
                glob: "docs/**".into(),
                instructions: "This unrelated rule must stay out of the request.".into(),
                rules: None,
                lanes: vec![LaneId::Security],
                merge: false,
            },
        ];
        let model = MockModel::new()
            .then(json!({"summary": "Nothing to report.", "findings": []}))
            .then(json!({"summary": "Nothing further.", "findings": []}));
        let handle = model.clone();
        let redaction_note = "1 credential value was removed; never ask for or guess it.";

        let mut diffs = large_diffs();
        diffs[0].path = "src/auth.rs".into();
        run_with_context(model, &config, &diffs, &[], "", redaction_note).await;

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
                prompt.contains("Treat request-derived shell arguments as tainted."),
                "{prompt}"
            );
            assert!(
                !prompt.contains("This unrelated rule must stay out of the request."),
                "{prompt}"
            );
            assert!(prompt.contains(redaction_note), "{prompt}");
        }
    }

    #[tokio::test]
    async fn a_malformed_coverage_response_keeps_round_ones_findings() {
        // The coverage pass is an optional extra look, not round one itself:
        // a reviewer that answers with something that fails the schema (here,
        // a finding missing every required field) must not discard what round
        // one already found. `reviewer_responses` treats a schema failure from
        // a lone reviewer as fatal, which used to propagate straight out of
        // `coverage_pass` via `?` and fail the whole group.
        let model = MockModel::new()
            .then(json!({
                "summary": "…",
                "findings": [finding_at_line("Guard the first index", 5)]
            }))
            .then(json!({"incorrect": []}))
            .then(json!({"summary": "…", "findings": [{"rule": "x"}]}));

        let outcome = run_with(model, &config_with_passes(2), &large_diffs(), &[]).await;

        assert_eq!(outcome.findings.len(), 1, "{:#?}", outcome.findings);
        assert_eq!(outcome.findings[0].title, "Guard the first index");
    }

    #[tokio::test]
    async fn a_new_second_pass_finding_unlocks_the_third_pass() {
        let model = MockModel::new()
            .then(json!({
                "summary": "…",
                "findings": [finding_at_line("Guard the first index", 5)]
            }))
            .then(json!({"incorrect": []}))
            .then(json!({
                "summary": "…",
                "findings": [finding_at_line("Guard the second index", 20)]
            }))
            .then(json!({"incorrect": []}))
            .then(json!({"summary": "Nothing further.", "findings": []}));
        let handle = model.clone();

        let outcome = run_with(model, &config_with_passes(3), &large_diffs(), &[]).await;

        assert_eq!(outcome.findings.len(), 2, "{:#?}", outcome.findings);
        assert_eq!(
            outcome
                .findings
                .iter()
                .map(|finding| finding.review_pass)
                .collect::<Vec<_>>(),
            vec![1, 2]
        );
        assert_eq!(handle.calls(), 5);
        let prompt = handle
            .requests()
            .last()
            .unwrap()
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
                "findings": [finding_at_line("Guard the first index", 5)]
            }))
            .then(json!({"incorrect": []}))
            .then(json!({
                "summary": "…",
                "findings": [finding_at_line("Guard the second index", 20)]
            }))
            .then(json!({"incorrect": []}))
            .then(json!({"summary": "…", "findings": [{"rule": "broken"}]}));

        let outcome = run_with(model, &config_with_passes(3), &large_diffs(), &[]).await;

        assert_eq!(outcome.findings.len(), 2, "{:#?}", outcome.findings);
    }

    #[tokio::test]
    async fn a_failed_coverage_pass_keeps_round_ones_findings() {
        let model = MockModel::new()
            .then(json!({
                "summary": "…",
                "findings": [finding_at_line("Guard the first index", 5)]
            }))
            .then(json!({"incorrect": []}))
            .then_error("provider unavailable");
        let handle = model.clone();

        let outcome = run_with(model, &config_with_passes(3), &large_diffs(), &[]).await;

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
            let lane = Security::new(model);
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
    async fn a_coverage_finding_updates_a_clean_round_one_summary() {
        // Round one said "Nothing to report." before the coverage pass ever
        // ran. If the coverage pass then finds something, the summary must
        // not keep declaring the group clean while `findings` says otherwise.
        let mut coverage_finding = finding_at_line("Guard the second index", 9);
        coverage_finding["path"] = json!("src/auth.rs");
        let model = MockModel::new()
            .then(json!({"summary": "Nothing to report.", "findings": []}))
            .then(json!({
                "summary": "…",
                "findings": [coverage_finding]
            }))
            .then(json!({"incorrect": []}));

        let mut diffs = large_diffs();
        diffs[0].path = "src/auth.rs".into();
        let outcome = run_with(model, &config_with_passes(2), &diffs, &[]).await;

        assert_eq!(outcome.findings.len(), 1, "{:#?}", outcome.findings);
        assert!(
            outcome.summary.contains("1 finding added by a second pass"),
            "{}",
            outcome.summary
        );
    }

    // --- golden test -------------------------------------------------------

    #[tokio::test]
    async fn golden_a_command_injection_on_a_changed_line_survives() {
        let model = MockModel::always(json!({
            "summary": "Adds a shell command built from request input.",
            "findings": [
                {
                    "path": "src/handler.rs", "line": 3,
                    "rule": "command-injection",
                    "title": "Do not build a shell command from request input",
                    "body": "`cmd` comes straight from the query string.",
                    "severity": "critical", "confidence": 0.95
                },
                {
                    "path": "src/handler.rs", "line": 1,
                    "rule": "unrelated",
                    "title": "Something about an unchanged line",
                    "body": "…", "severity": "high", "confidence": 0.9
                }
            ]
        }));
        let outcome = run_with(model, &config(), &diffs(), &[]).await;

        assert_eq!(outcome.findings.len(), 1, "{:#?}", outcome.findings);
        assert_eq!(outcome.findings[0].rule, "command-injection");
        assert_eq!(outcome.findings[0].severity, Severity::Critical);
        assert_eq!(outcome.findings[0].lane, LaneId::Security);
        assert!(outcome.summary.contains("1 finding discarded"));
    }

    #[tokio::test]
    async fn a_scanner_finding_is_republished_even_when_the_model_says_nothing() {
        let outcome = run_with(
            MockModel::silent(),
            &config(),
            &diffs(),
            &[workflow_finding()],
        )
        .await;

        assert_eq!(outcome.findings.len(), 1);
        assert_eq!(outcome.findings[0].rule, "workflow-write-all");
        assert_eq!(outcome.findings[0].lane, LaneId::Security);
    }

    #[tokio::test]
    async fn a_model_finding_restating_a_scanner_match_is_dropped() {
        // Otherwise the author gets the same permission problem twice: once
        // from the regular expression that is certain about it, and once from
        // the model that was shown the regular expression's output.
        let model = MockModel::always(json!({
            "summary": "The scanner is right.",
            "findings": [{
                "path": ".github/workflows/ci.yml", "line": 3,
                "rule": "workflow-write-all",
                "title": "Narrow the workflow permissions",
                "body": "…", "severity": "high", "confidence": 0.9
            }]
        }));
        let workflow = parse_file_patch(
            ".github/workflows/ci.yml",
            "@@ -1,2 +1,3 @@\n name: ci\n on: push\n+permissions: write-all\n",
        );
        let outcome = run_with(model, &config(), &[workflow], &[workflow_finding()]).await;

        assert_eq!(outcome.findings.len(), 1, "{:#?}", outcome.findings);
        assert_eq!(
            outcome.findings[0].confidence, 0.8,
            "the scanner's, not the model's"
        );
    }

    #[tokio::test]
    async fn the_model_cannot_talk_the_lane_out_of_a_scanner_finding() {
        let model = MockModel::always(json!({
            "summary": "That permission setting is fine, actually.",
            "findings": []
        }));
        let workflow = parse_file_patch(
            ".github/workflows/ci.yml",
            "@@ -1,2 +1,3 @@\n name: ci\n on: push\n+permissions: write-all\n",
        );
        let outcome = run_with(model, &config(), &[workflow], &[workflow_finding()]).await;

        assert_eq!(outcome.findings.len(), 1);
    }

    #[tokio::test]
    async fn the_scanner_findings_reach_the_prompt_as_evidence_not_instructions() {
        let model = MockModel::silent();
        let workflow = parse_file_patch(
            ".github/workflows/ci.yml",
            "@@ -1,2 +1,3 @@\n name: ci\n on: push\n+permissions: write-all\n",
        );
        run_with(model.clone(), &config(), &[workflow], &[workflow_finding()]).await;

        let prompt = model.last_prompt().expect("recorded");
        assert!(prompt.contains("scanner-findings"), "{prompt}");
        assert!(prompt.contains("workflow-write-all"));
        assert!(prompt.contains("Do not repeat them and do not re-scan"));
    }

    #[tokio::test]
    async fn each_file_gets_its_own_conversation() {
        let model = MockModel::silent();
        let diffs = vec![
            parse_file_patch("src/a.rs", "@@ -1 +1,2 @@\n a\n+b\n"),
            parse_file_patch("src/b.rs", "@@ -1 +1,2 @@\n a\n+b\n"),
        ];
        run_with(model.clone(), &config(), &diffs, &[]).await;

        assert_eq!(model.calls(), 2);
        let prompts: Vec<String> = model
            .requests()
            .iter()
            .map(|r| r.messages[0].content.clone())
            .collect();
        assert!(prompts.iter().any(|p| p.contains("`src/a.rs`")));
        assert!(prompts.iter().any(|p| p.contains("`src/b.rs`")));
        assert!(
            prompts.iter().all(|p| p.contains("One file only")),
            "each conversation must be told it owns one file"
        );
    }

    #[tokio::test]
    async fn one_files_failure_leaves_the_other_files_reviewed() {
        let model = MockModel::new()
            .then(json!({
                "summary": "Fine.",
                "findings": [{
                    "path": "src/a.rs", "line": 2, "rule": "r",
                    "title": "t", "body": "b",
                    "severity": "high", "confidence": 0.9
                }]
            }))
            .then_error("upstream exploded");
        let diffs = vec![
            parse_file_patch("src/a.rs", "@@ -1 +1,2 @@\n a\n+b\n"),
            parse_file_patch("src/b.rs", "@@ -1 +1,2 @@\n a\n+b\n"),
        ];
        let outcome = run_with(model, &config(), &diffs, &[]).await;

        assert_eq!(outcome.findings.len(), 1);
        assert!(
            outcome.summary.contains("could not be reviewed"),
            "{}",
            outcome.summary
        );
    }

    // --- triage -------------------------------------------------------------

    #[tokio::test]
    async fn a_lockfile_change_costs_no_model_call() {
        let model = MockModel::silent();
        let diffs = vec![
            parse_file_patch("Cargo.lock", "@@ -1 +1,2 @@\n a\n+serde = \"1\"\n"),
            parse_file_patch("src/handler.rs", "@@ -1 +1,2 @@\n a\n+let x = 1;\n"),
        ];
        let outcome = run_with(model.clone(), &config(), &diffs, &[]).await;

        assert_eq!(model.calls(), 1, "only the source file is worth a call");
        let prompt = model.last_prompt().expect("recorded");
        assert!(prompt.contains("`src/handler.rs`"), "{prompt}");
        assert!(
            outcome.summary.contains("Cargo.lock"),
            "the skip has to be visible: {}",
            outcome.summary
        );
    }

    #[tokio::test]
    async fn the_riskiest_file_is_reviewed_first() {
        let model = MockModel::silent();
        let diffs = vec![
            parse_file_patch("src/render.rs", "@@ -1 +1,2 @@\n a\n+let x = 1;\n"),
            parse_file_patch(
                "src/auth/session.rs",
                "@@ -1 +1,2 @@\n a\n+Command::new(\"sh\").arg(cmd).spawn();\n",
            ),
        ];
        run_with(model.clone(), &config(), &diffs, &[]).await;

        let first = &model.requests()[0].messages[0].content;
        assert!(
            first.contains("`src/auth/session.rs`"),
            "the budget must buy the riskiest file first: {first}"
        );
    }

    #[tokio::test]
    async fn a_scanner_finding_pulls_a_skippable_file_back_into_the_review() {
        let model = MockModel::silent();
        let diffs = vec![parse_file_patch(
            "Cargo.lock",
            "@@ -1 +1,2 @@\n a\n+left-pad = \"1\"\n",
        )];
        let dependency = ScanFinding::new(
            ScanKind::Dependency,
            Severity::High,
            "Cargo.lock",
            "dependency-yanked",
            "A yanked crate was added",
            "…",
        );
        run_with(model.clone(), &config(), &diffs, &[dependency]).await;

        assert_eq!(model.calls(), 1, "a scanner match is worth adjudicating");
    }

    #[tokio::test]
    async fn a_pull_request_of_only_generated_files_never_calls_the_model() {
        let model = MockModel::silent();
        let diffs = vec![parse_file_patch(
            "web/dist/app.min.js",
            "@@ -1 +1,2 @@\n a\n+var a=1\n",
        )];
        let outcome = run_with(model.clone(), &config(), &diffs, &[]).await;

        assert_eq!(model.calls(), 0);
        assert!(outcome.skipped.is_some(), "{:?}", outcome.skipped);
    }

    #[tokio::test]
    async fn a_pull_request_with_nothing_to_review_never_calls_the_model() {
        let model = MockModel::new();
        let outcome = run_with(model.clone(), &config(), &[], &[]).await;

        assert_eq!(model.calls(), 0);
        assert!(outcome.skipped.is_some());
    }

    #[tokio::test]
    async fn a_draft_is_skipped_unless_the_repository_opts_in() {
        let config = config();
        let model = MockModel::new();
        let pr = PullRequest {
            draft: true,
            ..pull_request()
        };
        let diffs = diffs();

        let outcome = Security::new(Arc::new(model.clone()))
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
    async fn a_credential_the_model_quoted_never_reaches_a_finding() {
        let key = format!("{}{}", "AKIA", "IOSFODNN7EXAMPLE");
        let model = MockModel::always(json!({
            "summary": "…",
            "findings": [{
                "path": "src/handler.rs", "line": 2,
                "rule": "hardcoded-credential",
                "title": format!("Remove the key {key}"),
                "body": format!("`{key}` is committed."),
                "severity": "critical", "confidence": 1.0
            }]
        }));
        let outcome = run_with(model, &config(), &diffs(), &[]).await;

        let rendered = serde_json::to_string(&outcome.findings).unwrap();
        assert!(!rendered.contains("IOSFODNN7EXAMPLE"), "{rendered}");
    }

    #[tokio::test]
    async fn a_file_reviewed_last_push_and_unchanged_costs_nothing_this_push() {
        // The largest avoidable cost in a review. This lane spends one model
        // call per file, so before this a push touching one file still paid for
        // every file in the pull request — and the bill grew with the branch
        // rather than with the change.
        let unchanged = parse_file_patch("src/handler.rs", PATCH);
        let touched = parse_file_patch(
            "src/other.rs",
            "@@ -1,1 +1,2 @@\n fn other() {\n+    let _ = std::process::Command::new(\"sh\");\n",
        );
        let previous = crate::evidence::replay::render(std::slice::from_ref(&unchanged));

        let model = MockModel::silent();
        let both = vec![unchanged, touched];
        run_with_reviewed(model.clone(), &config(), &both, &[], &previous).await;

        let paths: Vec<String> = model
            .requests()
            .into_iter()
            .map(|r| r.messages[1].content.clone())
            .collect();
        assert_eq!(paths.len(), 1, "one call, for the file that changed");
        assert!(
            paths[0].contains("src/other.rs") && !paths[0].contains("src/handler.rs"),
            "{paths:?}"
        );
    }

    #[tokio::test]
    async fn replayed_security_coverage_excludes_out_of_scope_prose() {
        let handler = parse_file_patch("src/handler.rs", PATCH);
        let readme = parse_file_patch("README.md", "@@ -1,1 +1,2 @@\n description\n+more prose\n");
        let previous = crate::evidence::replay::render(std::slice::from_ref(&handler));

        let outcome = run_with_reviewed(
            MockModel::silent(),
            &config(),
            &[handler, readme],
            &[],
            &previous,
        )
        .await;

        let coverage = outcome.coverage.expect("security coverage is recorded");
        assert!(coverage.reviewed.contains(&"src/handler.rs".to_owned()));
        assert!(
            !coverage.reviewed.contains(&"README.md".to_owned())
                && !coverage.unanswered.contains(&"README.md".to_owned()),
            "out-of-scope prose is not an uncovered security path: {coverage:?}"
        );
    }

    #[tokio::test]
    async fn a_scanner_match_is_re_reported_even_on_an_unchanged_file() {
        // A committed key is reported every push until it is gone. "We
        // mentioned it last time" is not a reason to stop, so a scanner match
        // forces its file back past the incremental skip.
        let unchanged = parse_file_patch(".github/workflows/ci.yml", PATCH);
        let previous = crate::evidence::replay::render(std::slice::from_ref(&unchanged));

        let outcome = run_with_reviewed(
            MockModel::silent(),
            &config(),
            std::slice::from_ref(&unchanged),
            &[workflow_finding()],
            &previous,
        )
        .await;

        assert!(
            outcome
                .findings
                .iter()
                .any(|f| f.rule == "workflow-write-all"),
            "{outcome:?}"
        );
    }
    #[tokio::test]
    async fn a_true_test_coverage_proposal_is_not_a_security_finding() {
        let model = MockModel::new()
            .then(json!({"summary":"Add focused tests for the behavior change.", "findings":[{
                "path":"src/handler.rs", "line":2, "rule":"repository-rule",
                "title":"Add focused tests for the round behavior change",
                "body":"The complete diff changes behavior without focused tests; add coverage for the new behavior.",
                "severity":"medium", "confidence":0.99}]}))
            .then(json!({"incorrect":[], "security_scope":[{
                "index":1, "verdict":"out_of_scope", "attacker_input":"",
                "dangerous_operation":"", "security_impact":"",
                "reason":"This asks for generic behavior coverage, with no claimed attacker path or security impact."}]}));
        let outcome = run_with(model, &config(), &diffs(), &[workflow_finding()]).await;
        assert_eq!(outcome.findings.len(), 1, "{outcome:?}");
        assert_eq!(outcome.findings[0].rule, "workflow-write-all");
        assert!(
            !outcome.summary.contains("Add focused tests"),
            "{}",
            outcome.summary
        );
    }

    fn unsupported_proposal() -> serde_json::Value {
        json!({"path":"src/handler.rs","line":2,"rule":"temporary-borrow",
            "title":"Keep the temporary alive","body":"This temporary borrow cannot compile.",
            "severity":"high","confidence":0.9})
    }

    fn disprove_first() -> serde_json::Value {
        json!({"incorrect":[{"index":1,"reason":"The borrow ends within the statement."}]})
    }

    #[tokio::test]
    async fn disproved_model_claims_leave_neither_findings_nor_stale_summary() {
        let model = MockModel::new()
            .then(json!({"summary":"The temporary borrow cannot compile.", "findings":[unsupported_proposal()]}))
            .then(disprove_first());
        let outcome = run_with(model, &config(), &diffs(), &[]).await;
        assert!(outcome.findings.is_empty(), "{outcome:?}");
        assert!(
            !outcome.summary.contains("cannot compile"),
            "{}",
            outcome.summary
        );
    }

    #[tokio::test]
    async fn disproving_model_claims_preserves_deterministic_scanner_facts() {
        let model = MockModel::new()
            .then(json!({"summary":"The temporary borrow cannot compile.", "findings":[unsupported_proposal()]}))
            .then(disprove_first());
        let outcome = run_with(model.clone(), &config(), &diffs(), &[workflow_finding()]).await;
        assert_eq!(outcome.findings.len(), 1, "{outcome:?}");
        assert_eq!(outcome.findings[0].rule, "workflow-write-all");
        let requests = model.requests();
        let prompt = &requests.last().unwrap().messages[1].content;
        assert!(!prompt.contains("workflow-write-all"), "{prompt}");
    }

    #[tokio::test]
    async fn a_failed_security_falsifier_keeps_the_original_model_finding() {
        let model = MockModel::new()
            .then(json!({"summary":"A potential bug.", "findings":[unsupported_proposal()]}))
            .then_error("filter unavailable");
        let outcome = run_with(model.clone(), &config(), &diffs(), &[]).await;
        assert_eq!(outcome.findings.len(), 1, "{outcome:?}");
        assert_eq!(outcome.findings[0].rule, "temporary-borrow");
        assert_eq!(model.requests().len(), 2);
    }

    #[tokio::test]
    async fn disproved_coverage_proposals_do_not_unlock_another_pass() {
        let mut proposal = finding_at_line("Unsupported claim", 5);
        proposal["path"] = json!("src/auth.rs");
        let model = MockModel::new()
            .then(json!({"summary":"Nothing to report.", "findings":[]}))
            .then(json!({"summary":"An invalid index.", "findings":[proposal]}))
            .then(disprove_first());
        let mut diffs = large_diffs();
        diffs[0].path = "src/auth.rs".into();
        let outcome = run_with(model.clone(), &config_with_passes(3), &diffs, &[]).await;
        assert!(outcome.findings.is_empty(), "{outcome:?}");
        assert_eq!(model.requests().len(), 3);
    }
    #[tokio::test]
    async fn partial_rejection_drops_stale_prose_and_preserves_the_other_finding() {
        let mut genuine = unsupported_proposal();
        genuine["line"] = json!(3);
        genuine["rule"] = json!("shell-injection");
        genuine["title"] = json!("Avoid request-derived shell commands");
        genuine["body"] = json!("The request cmd parameter reaches sh -c without validation.");
        let model = MockModel::new()
            .then(json!({"summary":"The temporary borrow cannot compile.", "findings":[unsupported_proposal(),genuine]}))
            .then(disprove_first());
        let outcome = run_with(model, &config(), &diffs(), &[]).await;
        assert_eq!(outcome.findings.len(), 1, "{outcome:?}");
        assert_eq!(outcome.findings[0].rule, "shell-injection");
        assert!(
            !outcome.summary.contains("cannot compile"),
            "{}",
            outcome.summary
        );
    }

    #[tokio::test]
    async fn out_of_scope_coverage_proposals_do_not_unlock_another_pass() {
        let mut proposal = finding_at_line("Add focused tests for round behavior", 5);
        proposal["path"] = json!("src/auth.rs");
        proposal["body"] =
            json!("The behavior changed without focused tests; add generic behavior coverage.");
        let model = MockModel::new()
            .then(json!({"summary":"Nothing to report.", "findings":[]}))
            .then(json!({"summary":"Add focused tests.", "findings":[proposal]}))
            .then(json!({"incorrect":[], "security_scope":[{
                "index":1, "verdict":"out_of_scope", "attacker_input":"",
                "dangerous_operation":"", "security_impact":"",
                "reason":"Generic behavior-test coverage asserts no security exploit."}]}));
        let mut diffs = large_diffs();
        diffs[0].path = "src/auth.rs".into();
        let outcome = run_with(model.clone(), &config_with_passes(3), &diffs, &[]).await;
        assert!(outcome.findings.is_empty(), "{outcome:?}");
        assert_eq!(model.requests().len(), 3);
    }

    #[tokio::test]
    async fn security_falsification_receives_the_dependency_manifest_the_reviewer_read() {
        let model = MockModel::new()
            .then(json!({"summary":"Check the feature.", "findings":[],
                "lookups":[{"kind":"read","path":"Cargo.toml","start":1,"end":2,"why":"dependency features"}]}))
            .then(json!({"summary":"An unsupported feature.", "findings":[unsupported_proposal()]}))
            .then(disprove_first());
        let tree = crate::ports::tree::MockTree::from_files([(
            "Cargo.toml",
            "[dependencies]\nmongodb = { version = \"3.9.1\", features = [\"compat-3-3-0\"] }\n",
        )]);
        let config = config();
        let policy = crate::config::types::LookupPolicy {
            enabled: true,
            rounds: 2,
            per_round: 3,
            max_chars: 10_000,
            checkout: false,
        };
        let paths = vec!["src/handler.rs".to_string()];
        let outcome = review_group(
            runner::lane_llm(Arc::new(model.clone()), &config, 100.0),
            &config,
            None,
            &[],
            &[],
            "",
            "",
            runner::Asking {
                tree: Some(&tree),
                lookup: Some(&policy),
                ..Default::default()
            },
            &paths,
            &paths,
            &diffs(),
            &[],
            None,
        )
        .await
        .expect("review runs");
        assert!(outcome.findings.is_empty());
        let requests = model.requests();
        assert_eq!(requests.len(), 3);
        let prompt = &requests[2].messages[1].content;
        assert!(prompt.contains("compat-3-3-0"), "{prompt}");
        assert!(prompt.contains("3.9.1"), "{prompt}");
    }
}
