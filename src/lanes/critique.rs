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
use crate::lanes::{Lane, LaneInput, LaneOutcome, reviewer_responses};
use crate::ports::model::{Model, Spend};
use crate::position::{PositionRequest, Positioner, Resolution, Unanchored};

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

#[async_trait]
impl Lane for Critique {
    fn id(&self) -> LaneId {
        LaneId::Critique
    }

    async fn run(&self, input: LaneInput<'_>) -> Result<LaneOutcome> {
        // Cheapest possible check first. A pull request that only deletes
        // files, or only touches ignored paths, has nothing for this lane, and
        // asking a model to confirm that is money for a foregone conclusion.
        if !input.has_reviewable_content() {
            return Ok(LaneOutcome::skipped(
                "No added or modified lines to review.",
            ));
        }

        if input.pull_request.draft && !input.config.review.draft_prs {
            return Ok(LaneOutcome::skipped(
                "Draft pull request; set `review.draft_prs = true` to review drafts.",
            ));
        }

        // Files this lane has already reviewed, unchanged since, are skipped
        // outright rather than replayed into a cacheable prefix. Both are
        // sound, but a per-file lane can take the stronger one: skipping pays
        // always, where a cache prefix only pays when the provider honours it.
        // This is what `security` does, and for the same reason.
        let fresh: Vec<&FileDiff> = replay::unreviewed(input.reviewed_evidence, input.diffs)
            .into_iter()
            .filter(|diff| !diff.changed_lines.is_empty())
            .collect();

        // A mechanical rename is verified, not read. Every file the
        // substitution explains in full is proven line for line here and
        // named as such in the summary; the model's budget goes to the files
        // it does not explain — which, on the pull request that motivated
        // this, was one file in fifty-nine. See `lanes::mechanical`.
        let mechanical = mechanical::detect(&fresh);
        // One verified file is still read: the check proves every file got
        // the *same* substitution, not that the substitution is harmless.
        // `check_admin()` → `allow_guest()` across fifty files is uniform and
        // is not a rename. One conversation over one sample answers that;
        // fifty over fifty answered it fifty times.
        let sample = mechanical.as_ref().and_then(|sub| sub.verified.first());
        let paths: Vec<String> = fresh
            .iter()
            .filter(|diff| {
                mechanical.as_ref().is_none_or(|sub| {
                    !sub.verified.contains(&diff.path) || Some(&diff.path) == sample
                })
            })
            .map(|diff| diff.path.clone())
            .collect();

        let changed_paths = input.changed_paths();
        // No model call: groups related changed files so a bug spanning them
        // — a caller and its callee, a function and its test — is visible to
        // one reviewer instead of hidden by the isolation clause each
        // ungrouped conversation is given. Off, or a component too large to
        // bet on, falls back to exactly the singleton fan-out this lane ran
        // before grouping existed — see `lanes::grouping`.
        let groups: Vec<FileGroup> = if input.config.grouping.enabled {
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

        // One capability for the whole lane, so the pull-request budget is
        // enforced across every file and every reviewer at once. That is what
        // lets the groups run concurrently: this lane reviewed them one at a
        // time only because spend is known after a call returns, and there was
        // nowhere else to check it.
        let llm = runner::lane_llm(
            self.model.clone(),
            input.config,
            input.config.models.budget_usd_per_pr,
        );

        let outcome = per_unit(
            &groups,
            |group| group.label.clone(),
            |group| group.paths.clone(),
            |group| {
                let llm = llm.clone();
                let input = &input;
                let changed_paths = &changed_paths;
                async move {
                    let group_diffs: Vec<FileDiff> = group
                        .paths
                        .iter()
                        .filter_map(|path| input.diffs.iter().find(|d| &d.path == path).cloned())
                        .collect();
                    review_group(llm, input, changed_paths, &group.paths, &group_diffs).await
                }
            },
        )
        .await;

        // The graph's own calls are tallied inside the capability, which is the
        // only object every one of them passes through. Folded in once here
        // rather than per file: `llm` is shared across the fan-out, so adding
        // it per file would multiply the bill by the file count.
        let mut outcome = outcome.into_outcome();
        outcome.spend.merge(llm.spend());
        if let Some(sub) = &mechanical {
            outcome.summary = format!("{} {}", outcome.summary.trim(), mechanical::note(sub));
            // Every file was the rename: a real verdict, reached without a
            // model, and the outcome must say so rather than read as skipped.
            if paths.is_empty() {
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
) -> Result<FileReview> {
    let config: &Config = input.config;
    let evidence = replay::render(group_diffs);
    let reviewers = council::reviewers(config, LaneId::Critique);

    // Every reviewer at once, as one graph. `ask_all` returns one answer per
    // reviewer in the order asked, and reports a reviewer it could not reach
    // rather than failing the council for it.
    let calls: Vec<Call> = reviewers
        .iter()
        .map(|reviewer| {
            let built = build_prompt(input, changed_paths, group_paths, &evidence, reviewer);
            Call {
                id: reviewer.id.to_string(),
                model: reviewer.model.to_string(),
                system: built.prefix().to_string(),
                prompt: built.suffix().to_string(),
                schema_name: "tinysweeper_critique".into(),
            }
        })
        .collect();

    let answers = runner::ask_all(
        llm.clone(),
        LaneId::Critique,
        &calls,
        &schema::json_schema(),
        input.asking_about_group(group_diffs),
    )
    .await?;

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
            Err(failure) if reviewers.len() > 1 => {
                spend.merge(failure.spend);
                tracing::warn!(agent = response.id, err = %failure.error, "a council reviewer failed");
                continue;
            }
            Err(failure) => return Err(failure.error),
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

    // Step 5, once over the merged set rather than once per reviewer. The
    // filter can only reject, so more inputs in one pass is identical semantics
    // at a fraction of the calls.
    let filtered = Falsifier::new(llm.model().as_ref(), config)
        .filter_with(LaneId::Critique, findings, &evidence, &looked_up)
        .await;
    spend.merge(filtered.spend);

    let mut findings = filtered.findings;
    let mut rejected = filtered.rejected;

    // The opt-in coverage pass. Gated on the group's own size, not the whole
    // pull request's — a lane fans out per group, so a two-line group must
    // not build a second prompt just because the change elsewhere is large.
    let mut added_by_coverage = 0usize;
    if config.review.passes > 1 && changed_lines(group_diffs) >= COVERAGE_PASS_MIN_LINES {
        // Fed cumulatively: the second coverage pass (pass 3) is told about
        // everything round one *and* the first coverage pass found, so it
        // does not rediscover the first pass's own additions.
        let mut confirmed = findings.clone();

        for _ in 1..config.review.passes {
            let reviewer = &reviewers[0];
            let confirmed_lines = crate::lanes::coverage::confirmed_lines(&confirmed);
            let built = prompt::build(&PromptInputs {
                repo_policy: input.repo_policy,
                extracted_rules: input.extracted_rules,
                prior_findings: input.prior_findings,
                new_evidence: &evidence,
                changed_paths,
                focus_paths: group_paths,
                persona: reviewer.persona,
                retrieved_context: input.retrieved_context,
                memory_context: input.memory_context,
                confirmed_this_round: &confirmed_lines,
                coverage_pass: true,
                ..PromptInputs::new(LaneId::Critique, config)
            });

            let coverage = crate::lanes::coverage::coverage_pass(
                llm.clone(),
                LaneId::Critique,
                reviewer,
                &built,
                &schema::json_schema(),
                "tinysweeper_critique",
                input.asking_about_group(group_diffs),
            )
            .await?;
            spend.merge(coverage.spend);
            looked_up.push_str(&coverage.looked_up);

            // No answer this round: nothing new to place, and a round that
            // could not be reached is not evidence a further one would fare
            // better, so stop rather than pay for another.
            let Some(response) = coverage.response else {
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
                Err(failure) => {
                    // The failed placement can already have paid for several
                    // relocation calls. It remains part of this review's
                    // bill even though this optional coverage response adds
                    // no findings.
                    spend.merge(failure.spend);
                    tracing::warn!(err = %failure.error, "a coverage pass failed to place its findings");
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
                break;
            }

            // Falsify only what this pass added — free when empty, and it
            // never re-judges what round one's own pass already kept.
            let new_filtered = Falsifier::new(llm.model().as_ref(), config)
                .filter_with(LaneId::Critique, new_findings, &evidence, &looked_up)
                .await;
            spend.merge(new_filtered.spend);
            rejected.extend(new_filtered.rejected);

            if new_filtered.findings.is_empty() {
                break;
            }

            added_by_coverage += new_filtered.findings.len();
            confirmed.extend(new_filtered.findings.clone());
            findings.extend(new_filtered.findings);
        }
    }

    Ok(FileReview {
        summary: summarise(
            summary.trim(),
            unanchored,
            discarded,
            &rejected,
            findings.len(),
            added_by_coverage,
        ),
        findings,
        resolved,
        spend,
    })
}

/// Minimum changed lines a group needs before the opt-in coverage pass
/// (`review.passes > 1`) is worth its extra call.
///
/// Not configurable: a repository that wants coverage passes at all is opting
/// into the per-pass cost already, and a second dial here would only let it
/// re-enable the noise this threshold exists to avoid on the two-line groups
/// that make up most pull requests. 40 is comfortably above what a rename or a
/// one-line fix touches, and comfortably below what a group large enough to
/// need a second reviewer look would be.
const COVERAGE_PASS_MIN_LINES: usize = 40;

/// How many lines this group's diffs changed, summed across every file in it.
fn changed_lines(group_diffs: &[FileDiff]) -> usize {
    group_diffs
        .iter()
        .map(|diff| diff.changed_lines.len())
        .sum()
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

/// A placement failure together with the relocation usage incurred first.
///
/// Placement enforces its budget between findings, so it can fail after
/// successful relocation calls. Keeping that partial spend is necessary for
/// accurate reporting and for subsequent budget accounting.
struct PlacementFailure {
    error: crate::error::Error,
    spend: Spend,
}

/// Build one reviewer's prompt for one group.
///
/// Split from [`place`] so every reviewer's prompt is assembled before any call
/// is made: the graph asks them all at once, and a builder that ran inside the
/// call would serialise them again.
fn build_prompt<'a>(
    input: &'a LaneInput<'_>,
    changed_paths: &'a [String],
    group_paths: &'a [String],
    evidence: &'a str,
    reviewer: &council::Reviewer<'_>,
) -> prompt::Prompt {
    let config: &Config = input.config;

    prompt::build(&PromptInputs {
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
        memory_context: input.memory_context,
        ..PromptInputs::new(LaneId::Critique, config)
    })
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
) -> std::result::Result<Asked, Box<PlacementFailure>> {
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
            return Err(Box::new(PlacementFailure {
                error: crate::error::Error::Budget {
                    spent: spend.cost_usd(),
                    limit: config.models.budget_usd_per_pr,
                },
                spend,
            }));
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
/// The model's own prose is **dropped entirely** when falsification left
/// nothing standing, and that is the important case. The prose is written
/// before the filter runs, so it describes findings that no longer exist:
/// a review once opened "One real bug: the coverage edge is never stored",
/// reported no findings, concluded success, and approved the pull request in
/// the same breath — the bug was a hallucination the falsifier correctly
/// removed, and only the summary still claimed it. A lane that reports nothing
/// must not narrate something. What replaces it is the rejection reasons,
/// which say more than the discarded prose did.
///
/// `added_by_coverage` covers the opposite mismatch: `summary` is round one's
/// prose, written before the opt-in coverage pass (`lanes::coverage`) ever
/// runs, so a group round one called clean and the coverage pass then added a
/// finding to would otherwise keep declaring itself clean while `kept` says
/// otherwise. Folded in as a note rather than rewritten, for the same reason
/// the other counts are — round one's own words stay round one's, and what
/// changed after it is stated rather than silently absorbed into them.
fn summarise(
    summary: &str,
    unanchored: usize,
    discarded: usize,
    rejected: &[Rejection],
    kept: usize,
    added_by_coverage: usize,
) -> String {
    if kept == 0 && !rejected.is_empty() {
        let reasons: Vec<String> = rejected
            .iter()
            .map(|item| format!("{} — {}", item.title.trim(), item.reason.trim()))
            .collect();
        return format!(
            "Nothing to report. {} finding{} raised and dropped as disproved by the diff: {}.",
            rejected.len(),
            plural(rejected.len()),
            reasons.join("; ")
        );
    }

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

    if notes.is_empty() {
        return summary.to_string();
    }
    format!("{summary} ({})", notes.join("; "))
}

fn plural(count: usize) -> &'static str {
    if count == 1 { "" } else { "s" }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::types::{Config, Severity};
    use crate::evidence::diff::parse_file_patch;
    use crate::forge::types::CheckConclusion;
    use crate::forge::types::PullRequest;
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
                retrieved_context: "",
                memory_context: "",
                e2e: None,
                tree: None,
                graph: None,
            })
            .await
            .expect("lane runs")
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

    /// How many changed lines a group needs to clear [`COVERAGE_PASS_MIN_LINES`].
    const LARGE_LINES: usize = COVERAGE_PASS_MIN_LINES;

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
        let model = MockModel::new()
            .then(json!({"summary": "Nothing to report.", "findings": []}))
            .then(json!({
                "summary": "…",
                "findings": [finding_named("Guard the second index", 30)]
            }))
            .then(json!({"incorrect": []}));

        let outcome = run_with(model, &config_with_passes(2), &large_diffs()).await;

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

        // The default config ships `passes = 1`.
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
    async fn a_coverage_placement_failure_preserves_round_one_findings() {
        // A coverage response with more unresolvable quotes than the
        // relocation budget affords must not lose round one's own, already
        // falsified finding — placement failing on this optional pass is no
        // additional coverage result, not a reason to fail the whole group.
        //
        // Round one's review call and the coverage pass's own review call are
        // routed through the graph, so they alone count against
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
        assert!(
            (outcome.spend.cost_usd() - 0.06).abs() < f64::EPSILON,
            "the three successful relocations must remain charged: {:#?}",
            outcome.spend
        );
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
                retrieved_context: "",
                memory_context: "",
                e2e: None,
                tree: None,
                graph: None,
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
                retrieved_context: "",
                memory_context: "",
                e2e: None,
                tree: None,
                graph: None,
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
                retrieved_context: "",
                memory_context: "",
                e2e: None,
                tree: None,
                graph: None,
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
                retrieved_context: "",
                memory_context: "",
                e2e: None,
                tree: None,
                graph: None,
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
                retrieved_context: "",
                memory_context: "",
                e2e: None,
                tree: None,
                graph: None,
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
