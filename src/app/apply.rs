//! `tinysweeper apply` — the write half.
//!
//! Reads a proposal produced by `review` and publishes it. It holds no model
//! key and makes no model call: the verdict was already reached, and this
//! module's only job is to put it on GitHub without changing it.
//!
//! Before writing anything it re-fetches live state and checks the head SHA
//! still matches. A review of a commit nobody is looking at any more is worse
//! than no review — it reports on code that has already been replaced.

use crate::app::review::{PROPOSAL_VERSION, Proposal};
use crate::config::types::{Config, Severity};
use crate::error::{Error, Result};
use crate::evidence::diff::{FileDiff, parse_file_patch};
use crate::forge::types::{ChangedFile, CheckRun, RepoId, ReviewComment, ReviewEvent};
use crate::ports::forge::{ForgeRead, ForgeWrite};
use crate::ports::review_state::ReviewStateStore;
use crate::{MARKER_PREFIX, VERSION};

/// Publish a proposal.
///
/// If a store is provided, extends the stored fingerprints with the identities
/// of newly posted findings after successful publication, so the next review
/// will dedupe them correctly.
pub async fn apply(
    read: &dyn ForgeRead,
    write: &dyn ForgeWrite,
    config: &Config,
    proposal: &Proposal,
    store: Option<&dyn ReviewStateStore>,
) -> Result<()> {
    let repo = RepoId::parse(&proposal.repo)
        .ok_or_else(|| Error::Forge(format!("`{}` is not owner/name", proposal.repo)))?;

    // Re-validate against live state. The review ran minutes ago in another
    // job, and the world moves.
    let live = read.pull_request(&repo, proposal.number).await?;
    if live.head_sha != proposal.head_sha {
        tracing::info!(
            reviewed = %proposal.head_sha,
            live = %live.head_sha,
            "head moved since the review; not publishing a stale verdict"
        );
        return Ok(());
    }

    // Housekeeping first: threads this run decided are settled. Deterministic
    // policy chose them (`crate::threads`), this only executes the list, and a
    // failure here is logged rather than allowed to cost the verdict.
    if let Err(err) =
        crate::threads::apply_plan(write, config, &repo, &proposal.threads, &proposal.head_sha)
            .await
    {
        tracing::warn!(%err, "could not resolve review threads");
    }

    for lane in &proposal.lanes {
        write
            .publish_check(
                &repo,
                CheckRun {
                    name: lane.check_name.clone(),
                    head_sha: proposal.head_sha.clone(),
                    conclusion: Some(lane.conclusion),
                    title: title_for(lane.findings.len(), &lane.summary),
                    summary: render_lane_summary(lane),
                    images: vec![],
                },
            )
            .await?;
    }

    // What tinysweeper already said about this pull request. GitHub keeps only
    // the latest review per reviewer, which makes this load-bearing twice: it
    // is how a fixed pull request gets unblocked — without an explicit clearing
    // verdict a stale objection blocks the merge button until a human dismisses
    // it by hand — and it is how an approval that already stands avoids being
    // restated on every push.
    let (previous, previous_known) = own_review_state(read, &repo, proposal.number).await;
    let event = review_event(config, proposal, previous, live.draft);
    // Every lane is expected to leave an unpostable finding without a line,
    // but `apply` is the final boundary before GitHub sees it. One invalid
    // anchor makes GitHub reject the entire review, including otherwise valid
    // comments, so derive the postable set from the live diff here as well.
    let files = read.changed_files(&repo, proposal.number).await?;
    let comments = inline_comments(proposal, &files);
    let posted: std::collections::BTreeSet<String> = comments
        .iter()
        .filter_map(|comment| fingerprint(&comment.body))
        .collect();
    let unanchored: Vec<&crate::findings::types::Finding> = proposal
        .findings()
        .filter(|finding| finding.line.is_some() && !posted.contains(&identity(finding)))
        .collect();

    // The identities about to be posted, so the store can be extended once the
    // write succeeds.
    //
    // Read straight off the findings rather than parsed back out of the comment
    // bodies this function just rendered. The round-trip through text was both
    // unnecessary and wrong — it searched for `{fp=` while the marker written
    // below is `<!-- tinysweeper:fp=… -->`, so it never matched and the store
    // was never extended. The same condition as `inline_comments`: only an
    // anchored finding becomes a comment, and only a posted finding may
    // suppress a later one.
    let newly_posted: Vec<String> = comments
        .iter()
        .filter_map(|comment| fingerprint(&comment.body))
        .collect();

    // Submit a review if:
    // - there are inline comments to post, or
    // - the verdict is Approve (clears a previous block, or satisfies a
    //   "review required" rule), or
    // - the verdict is RequestChanges (blocks the merge, even if only in the summary).
    // Blocking verdicts must be submitted even without inline comments, because
    // findings that could not be anchored to lines still appear in the summary
    // and need the blocking verdict on GitHub to enforce the gate.
    //
    // The one thing not worth saying twice is an approval that already stands.
    // GitHub keeps the latest review per reviewer, so re-approving changes
    // nothing on the merge button and only adds a timeline entry — on every
    // push, for the whole life of a clean pull request.
    let redundant_approval = event == ReviewEvent::Approve
        && previous == Some(ReviewEvent::Approve)
        && comments.is_empty();

    // A push this review could not vouch for — a model that never answered,
    // a file the forge withheld — cannot leave an earlier approval standing
    // over it: a comment does not withdraw one, and a repository that does
    // not dismiss stale approvals would merge this push on the strength of
    // what was said about the last. So the standing approval is withdrawn,
    // with the reason, and the comment below repeats it.
    //
    // Only when the verdict is a comment. A changes request supersedes the
    // approval on its own, and an approval is a fresh one. Attempted also
    // when the lookup could not say what stands — a failed read of our own
    // history must not become the approval's shield — and best effort: a
    // dismissal that fails is logged, and the comment saying this is not an
    // approval still goes out, rather than nothing at all.
    let unvouched = !proposal.complete() && proposal.skipped.is_none();
    let mut withdrawal_failed = None;
    if unvouched
        && event == ReviewEvent::Comment
        && (previous == Some(ReviewEvent::Approve) || !previous_known)
        && let Err(err) = write
            .dismiss_own_approval(
                &repo,
                proposal.number,
                "tinysweeper could not review the latest push, so its earlier approval no \
                 longer speaks for this pull request.",
            )
            .await
    {
        // Remembered, not swallowed: the comment below still goes out, so a
        // reader sees why, and then the run fails — an approval that may
        // still stand over a push nobody reviewed is not a success, and the
        // failure lands on the pull request as a blocking check.
        tracing::warn!(%err, number = proposal.number, "could not withdraw the standing approval");
        withdrawal_failed = Some(err);
    }

    // A push the model never answered is submitted too, as the comment: the
    // lane checks say "did not review", but a reader of the conversation
    // sees only that the bot said nothing, which on a clean-looking pull
    // request reads as an all-clear. A file the forge withheld is not a
    // reason to comment: it is a property of the pull request, it recurs on
    // every push, and every lane's check already names it — the dismissal
    // above is what a standing approval needed. A kill-switched one is
    // nothing at all: nobody asked.
    let must_say = !proposal.answered() || proposal.version != PROPOSAL_VERSION;
    if !redundant_approval
        && (!comments.is_empty()
            || event == ReviewEvent::Approve
            || event == ReviewEvent::RequestChanges
            || must_say)
    {
        write
            .create_review(
                &repo,
                proposal.number,
                &review_body(proposal, event, previous, &unanchored),
                comments,
                event,
            )
            .await?;

        // After successful publish, extend the stored state with newly posted
        // fingerprints so the next review dedupes them correctly.
        if let Some(store) = store
            && !newly_posted.is_empty()
        {
            let state_key = crate::state::key(&proposal.repo, proposal.number);
            if let Ok(Some(mut current_state)) = store.load_state(&state_key).await {
                current_state.fingerprints.extend(newly_posted.clone());
                if let Err(err) = store.save_state(&state_key, &current_state).await {
                    tracing::warn!(%err, "could not record newly posted fingerprints");
                }
            }
        }
    }

    // The change map, as one comment edited in place forever. Deliberately
    // *not* folded into the review body: a review is submitted only when there
    // is a verdict to give, and the pull request that most needs a picture of
    // itself is often the clean one that gets no inline comments at all.
    //
    // Best effort, and last-but-one on purpose. It is the only thing published
    // here that nobody is gated on, so a failure to draw it must not cost the
    // verdict that was already posted above.
    if let Err(err) = publish_overview(read, write, config, proposal).await {
        tracing::warn!(%err, "could not publish the change map");
    }

    // Triage last, and against `live` rather than a second fetch: the labels
    // restate a verdict whose evidence is now on the pull request, so they can
    // never point at a review that failed to publish. Add-only, so a
    // maintainer's own triage survives every re-run.
    let added =
        crate::issues::pull_request::apply_triage(write, &repo, &live, proposal, &config.issues)
            .await?;
    if !added.is_empty() {
        tracing::info!(number = proposal.number, ?added, "triaged");
    }

    // Everything that could be published was; now the one thing that could
    // not be undone is reported as the failure it is.
    match withdrawal_failed {
        Some(err) => Err(err),
        None => Ok(()),
    }
}

/// Post or update the change-map comment.
///
/// One comment per pull request, found by its marker and edited in place. The
/// alternative — a fresh comment per push — turns a diagram into a scroll bar,
/// and the diagram of a two-push-old head is not a diagram of the pull request.
///
/// Writes nothing at all when the map has no relationship worth explaining:
/// disconnected names are not a flow, and a comment containing only those
/// names is noise with a picture in it.
async fn publish_overview(
    read: &dyn ForgeRead,
    write: &dyn ForgeWrite,
    config: &Config,
    proposal: &Proposal,
) -> Result<()> {
    if !config.overview.enabled {
        return Ok(());
    }
    let Some(map) = &proposal.overview else {
        return Ok(());
    };
    let Some(body) = crate::overview::comment(map) else {
        return Ok(());
    };

    let repo = RepoId::parse(&proposal.repo)
        .ok_or_else(|| Error::Forge(format!("`{}` is not owner/name", proposal.repo)))?;

    // A comment with no id cannot be edited, so it falls through to posting a
    // new one. That is the harmless direction to be wrong in: a duplicate
    // comment is noise, whereas editing the wrong comment destroys someone's
    // words.
    let existing =
        crate::findings::prior::own_comment(read, &repo, proposal.number, crate::overview::MARKER)
            .await?
            .and_then(|comment| comment.id);

    match existing {
        Some(id) => write.update_comment(&repo, id, &body).await,
        None => write
            .create_comment(&repo, proposal.number, &body)
            .await
            .map(|_| ()),
    }
}

/// Decide how to submit the review.
///
/// `previous` is tinysweeper's own last verdict on this pull request, if any.
fn review_event(
    config: &Config,
    proposal: &Proposal,
    previous: Option<ReviewEvent>,
    draft: bool,
) -> ReviewEvent {
    // Blocking needs BOTH a failing lane and a finding severe enough to justify
    // it. The lane conclusion alone is not enough: `fail_on` and
    // `request_changes_at` are independent knobs, so a lane configured to fail
    // on medium must still be able to fail a check without also blocking the
    // merge when the merge gate is set to high. Reading only the conclusion
    // here made `request_changes_at` inert.
    //
    // The severity is read from the lane's findings rather than the surviving
    // comments, so a recurred problem whose comment was deduped away still
    // blocks — being already visible is not being fixed.
    let blocks = match config.request_changes_at() {
        Some(threshold) => proposal.blocked() && proposal.has_severity_at_or_above(threshold),
        None => false,
    };
    if blocks {
        return ReviewEvent::RequestChanges;
    }

    // Approving is gated on every lane passing, not on the weaker `blocks`
    // above. Those two differ exactly when `request_changes_at` is `"off"`, and
    // in that case a failing lane must not be approved — turning blocking off
    // asks tinysweeper to stop objecting, not to start endorsing.
    //
    // `complete` is the second condition, and it is why the aggregate check run
    // could be removed. An approval is now the whole verdict, so it has to carry
    // what that check carried: a file the forge never showed us is not a file we
    // can vouch for. Nothing blocks, so there is nothing to object to — and
    // nothing to endorse either, which is a `Comment`.
    // `!draft` is the third condition, and it is not a preference. With
    // `review.draft_prs = false` every lane *skips* a draft, so the proposal
    // comes back with nothing blocking and nothing unreviewed — which reads as
    // "clean" to both conditions above. The bot would then endorse a pull
    // request it had deliberately declined to look at, and on a repository
    // requiring a review that endorsement is what lets it merge.
    //
    // Read from live state rather than from the proposal, because the author
    // may have marked it draft in the minutes since the review ran.
    if !draft && !proposal.blocked() && proposal.complete() && config.review.approve_when_clean {
        return ReviewEvent::Approve;
    }

    // Clean now, blocked before: clear it even when approving is off, and even
    // on a draft. Anything else leaves the author stuck behind an objection
    // that no longer applies, needing a human to dismiss a review by hand.
    //
    // Deliberately not gated on `draft`. Refusing to *endorse* a draft is not
    // the same as refusing to *unblock* one, and conflating them would strand
    // every draft that was ever blocked.
    //
    // Gated on the review being complete, though. "Clean now" is only a
    // finding when somebody looked at everything: a push during a provider
    // outage comes back with every lane unanswered and nothing blocking, and
    // a push whose largest file the forge withheld may be hiding the very
    // thing objected to. Clearing the block on either would let a gap in
    // the review approve what a review had objected to.
    if previous == Some(ReviewEvent::RequestChanges) && proposal.complete() {
        return ReviewEvent::Approve;
    }

    ReviewEvent::Comment
}

/// tinysweeper's own last review verdict on this pull request.
///
/// Read from the forge rather than remembered, so it stays correct across a
/// restart, a redeploy, and a human dismissing the review by hand.
///
/// The second value says whether the answer is known: `(None, false)` is a
/// lookup that failed, which is not the same as having never reviewed.
async fn own_review_state(
    read: &dyn ForgeRead,
    repo: &RepoId,
    number: u64,
) -> (Option<ReviewEvent>, bool) {
    match read.own_review_state(repo, number).await {
        Ok(state) => (state, true),
        Err(err) => {
            // Failing closed here would mean never clearing a block. Failing
            // open at worst re-states a verdict that already stands — except
            // for the one caller that must fail closed, which reads the flag.
            tracing::warn!(%err, "could not read the previous review state");
            (None, false)
        }
    }
}

/// What settling the `e2e` check run amounted to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum E2eSettlement {
    /// No review left that lane waiting on anything.
    NothingWatched,
    /// The pull request has moved on; the next review starts a new watch.
    HeadMoved,
    /// At least one watched job has not concluded.
    StillPending,
    /// The check run was concluded and the watch cleared.
    Published(crate::forge::types::CheckConclusion),
}

/// Conclude the `e2e` check run once the jobs it was waiting on have spoken.
///
/// The second half of that lane's verdict, and the reason this lives here:
/// the review decided everything it could — the harness, the coverage, the
/// jobs that would never run — and recorded what it was still waiting on in
/// `ReviewedState::e2e`. This function executes that recorded plan and
/// nothing else. No model is consulted; the decision is
/// `lanes::e2e::runs::settle`, arithmetic over the check runs the forge
/// holds, and the only write is the one check run the review already
/// published as `Neutral`.
///
/// Called on every `check_run`/`check_suite` completion the server sees for
/// the pull request, so the cheap exits come first: no watch, wrong head, or
/// a job still running each cost at most two reads.
pub async fn settle_e2e(
    read: &dyn ForgeRead,
    write: &dyn ForgeWrite,
    config: &Config,
    store: &dyn ReviewStateStore,
    repo: &RepoId,
    number: u64,
) -> Result<E2eSettlement> {
    use crate::config::types::LaneId;

    let key = crate::state::key(&repo.to_string(), number);
    let Some(state) = store.load_state(&key).await? else {
        return Ok(E2eSettlement::NothingWatched);
    };
    let Some(watch) = state.e2e else {
        return Ok(E2eSettlement::NothingWatched);
    };

    let live = read.pull_request(repo, number).await?;
    if live.head_sha != watch.head_sha {
        // Not cleared: the review of the new head rewrites the whole record,
        // and clearing here would race it.
        return Ok(E2eSettlement::HeadMoved);
    }

    let checks = read.check_runs(repo, &watch.head_sha).await?;
    let Some(settled) =
        crate::lanes::e2e::runs::settle(&watch, &checks, config.fail_on(LaneId::E2e))
    else {
        return Ok(E2eSettlement::StillPending);
    };

    write
        .publish_check(
            repo,
            CheckRun {
                name: LaneId::E2e.check_name(),
                head_sha: watch.head_sha.clone(),
                conclusion: Some(settled.conclusion),
                title: match settled.conclusion {
                    crate::forge::types::CheckConclusion::Failure => {
                        "An end-to-end job did not pass".into()
                    }
                    _ => "End-to-end jobs concluded".into(),
                },
                summary: crate::findings::render::lane_summary(
                    &settled.summary,
                    &[],
                    VERSION,
                    true,
                ),
                images: vec![],
            },
        )
        .await?;

    // Cleared only after the write succeeded: a failed publish leaves the
    // watch in place so the next completion event retries it.
    //
    // `clear_e2e_watch` rather than a reload-then-`save_state`: the store
    // applies the condition ("still exactly this watch") and the write
    // together, so a new review's `save_state` landing in the gap between
    // this call's own `load_state` above and now cannot be discarded by an
    // unconditional write-back the way reloading-and-saving still could —
    // see its doc comment on `ReviewStateStore`.
    if let Err(err) = store.clear_e2e_watch(&key, &watch).await {
        tracing::warn!(%err, "could not clear the e2e watch; the next completion will republish");
    }
    Ok(E2eSettlement::Published(settled.conclusion))
}

/// `text` as a Markdown code span that `text` cannot break out of.
///
/// Paths here are the contributor's: a filename with a backtick would close
/// the span and write Markdown into a review the bot signs. The span is
/// fenced with one more backtick than the longest run inside, which is how
/// CommonMark spells a literal backtick, and control characters — a newline
/// ends a span — are dropped.
fn code_span(text: &str) -> String {
    let clean: String = text.chars().filter(|c| !c.is_control()).collect();
    let longest = clean.split(|c| c != '`').map(str::len).max().unwrap_or(0);
    let fence = "`".repeat(longest + 1);
    if clean.contains('`') {
        // A space each side is what lets a span begin or end with a backtick;
        // CommonMark strips exactly one from each end.
        format!("{fence} {clean} {fence}")
    } else {
        format!("{fence}{clean}{fence}")
    }
}

fn title_for(findings: usize, summary: &str) -> String {
    match findings {
        0 => summary.chars().take(80).collect(),
        1 => "1 finding".into(),
        n => format!("{n} findings"),
    }
}

/// The rendered summary, for tests in sibling modules.
#[cfg(test)]
pub(crate) fn render_lane_summary_for_test(lane: &crate::app::review::LaneProposal) -> String {
    render_lane_summary(lane)
}

fn render_lane_summary(lane: &crate::app::review::LaneProposal) -> String {
    // `Neutral` is exactly "this lane formed no opinion" — not implemented,
    // skipped as a draft, or every model call in the chain failed. Anything
    // else reached a verdict, including a clean one.
    let reached_a_verdict = lane.conclusion != crate::forge::types::CheckConclusion::Neutral;
    let mut out = crate::findings::render::lane_summary(
        &lane.summary,
        &lane.findings,
        VERSION,
        reached_a_verdict,
    );

    // Below the gate, above notice. A line each: where, what, how sure. Not a
    // comment and not a verdict, so the wording says so.
    if !lane.noted.is_empty() {
        out.push_str(
            "\n**Worth a look** — below the posting gate, so not a comment and not a block:\n\n",
        );
        for finding in &lane.noted {
            out.push_str(&format!(
                "- `{}`{} — {} {}\n",
                finding.path,
                finding.line.map(|l| format!(":{l}")).unwrap_or_default(),
                crate::findings::render::escape_cell(&finding.title),
                crate::findings::render::confidence_badge(finding.confidence),
            ));
        }
    }

    // Resolved findings are reported, not discarded. An author who fixed
    // something needs to see that it was noticed; otherwise the only signal a
    // review ever gives is a new objection.
    if !lane.resolved.is_empty() {
        out.push_str("\n**Fixed since the last review**\n\n");
        for title in &lane.resolved {
            // Escape titles to prevent model-authored markup from breaking the page.
            out.push_str(&format!(
                "- {}\n",
                crate::findings::render::escape_cell(title)
            ));
        }
    }

    out
}

/// The fingerprint marker to stamp on a finding's comment.
///
/// Falls back to a title-derived fingerprint for a proposal written before the
/// identity was stamped during review — an old `findings.json` still publishes,
/// it just dedupes on the weaker key it was written with.
fn identity(finding: &crate::findings::types::Finding) -> String {
    finding
        .identity
        .clone()
        .unwrap_or_else(|| finding.fingerprint(&finding.title))
}

/// Read the identity from a comment this module just rendered.
///
/// The marker is the durable representation written to GitHub, so using it
/// here keeps state recording tied to the exact comments that survived the
/// final diff-anchor validation.
fn fingerprint(body: &str) -> Option<String> {
    let marker = format!("<!-- {MARKER_PREFIX}fp=");
    body.split_once(&marker)
        .and_then(|(_, rest)| rest.split_once(" -->"))
        .map(|(value, _)| value.to_string())
        .filter(|value| !value.is_empty())
}

fn review_body(
    proposal: &Proposal,
    event: ReviewEvent,
    previous: Option<ReviewEvent>,
    unanchored: &[&crate::findings::types::Finding],
) -> String {
    let blocking = proposal
        .lanes
        .iter()
        .filter(|l| l.conclusion.blocks())
        .count();

    let mut body = match event {
        ReviewEvent::RequestChanges => {
            let worst = proposal
                .findings()
                .map(|f| f.severity)
                .max()
                .unwrap_or(Severity::Low);
            format!(
                "Requesting changes: {blocking} lane(s) blocking, worst finding is **{worst}**.\n\n\
                 Fix or reply to the findings below and push. The next review clears this \
                 automatically once they are gone — you should not need to dismiss anything by \
                 hand."
            )
        }
        // An approval means two different things depending on what stood
        // before it, and saying the wrong one is worse than saying nothing: a
        // first-time approval that claims to be "clearing the changes request"
        // invents an objection that was never made.
        ReviewEvent::Approve if previous == Some(ReviewEvent::RequestChanges) => {
            "The previously-blocking findings are resolved. Clearing the changes request."
                .to_string()
        }
        ReviewEvent::Approve => "tinysweeper found nothing blocking. Approving.".to_string(),
        // Not approving a clean-looking review is a decision, and the reader
        // deserves the reason: silence here reads as an all-clear.
        ReviewEvent::Comment if blocking == 0 && !proposal.complete() => {
            let unanswered = proposal.unanswered();
            let shown: Vec<&str> = unanswered.iter().copied().take(8).collect();
            let more = unanswered.len().saturating_sub(shown.len());
            format!(
                "tinysweeper found nothing blocking, but could not review everything, so this \
                 is not an approval: {}{}.",
                shown
                    .iter()
                    .map(|name| code_span(name))
                    .collect::<Vec<_>>()
                    .join(", "),
                if more > 0 {
                    format!(" and {more} more")
                } else {
                    String::new()
                }
            )
        }
        ReviewEvent::Comment if blocking == 0 => "tinysweeper found nothing blocking.".to_string(),
        ReviewEvent::Comment => format!("tinysweeper: {blocking} lane(s) blocking."),
    };

    if !unanchored.is_empty() {
        body.push_str("\n\n### Findings not posted inline\n");
        for finding in unanchored {
            body.push_str(&format!(
                "\n- **{}** (`{}`): {}\n\n  {}\n",
                finding.title, finding.path, finding.rule, finding.body
            ));
        }
    }

    // The full token breakdown goes in the body deliberately. Cache hit rate is
    // the difference between a cheap re-review and a ruinous one, and nobody
    // tunes a number they cannot see.
    //
    // A fenced, column-aligned block rather than a `<sub>` sentence per lane:
    // the point of the breakdown is comparing lanes, and six numbers that land
    // in a different place on every row cannot be compared at a glance. The
    // fence also stops the renderer reflowing away the alignment.
    body.push_str("\n\n");
    body.push_str(&crate::findings::render::cost_table(
        &proposal.usage(),
        &proposal.models,
        &proposal.lane_costs(),
    ));
    body.push_str(&format!(
        "\n<!-- {MARKER_PREFIX}state v=1 sha={} -->",
        proposal.head_sha
    ));
    body
}

/// Inline comments for findings whose anchors GitHub can accept on the live diff.
fn inline_comments(proposal: &Proposal, files: &[ChangedFile]) -> Vec<ReviewComment> {
    let diffs: std::collections::BTreeMap<&str, FileDiff> = files
        .iter()
        .filter_map(|file| {
            file.patch
                .as_deref()
                .map(|patch| (file.path.as_str(), parse_file_patch(&file.path, patch)))
        })
        .collect();

    proposal
        .findings()
        .filter_map(|finding| {
            let line = finding.line?;
            // A suggestion block replaces exactly the lines the comment is
            // anchored to, so carrying one *changes the anchor*: it widens to
            // the span the replacement covers. Without a suggestion the comment
            // stays a single-line pin, which is what a reader wants — a
            // multi-line highlight for a one-sentence remark is noise.
            let (start_line, line) = match &finding.applicable {
                Some(suggestion) if suggestion.start_line < suggestion.end_line => {
                    (Some(suggestion.start_line), suggestion.end_line)
                }
                Some(suggestion) => (None, suggestion.end_line),
                None => (None, line),
            };
            let start = start_line.unwrap_or(line);
            if !diffs
                .get(finding.path.as_str())
                .is_some_and(|diff| diff.within_hunk(start, line))
            {
                tracing::warn!(
                    path = %finding.path,
                    start_line = start,
                    end_line = line,
                    "dropping an inline finding outside the live diff"
                );
                return None;
            }
            let suggestion = finding
                .applicable
                .as_ref()
                .map(|s| format!("\n\n```suggestion\n{}\n```", s.replacement))
                .unwrap_or_default();
            Some(ReviewComment {
                path: finding.path.clone(),
                line: Some(line),
                start_line,
                // The forge assigns the author on the way in; on the way out it
                // is what tells dedupe whether a marker is ours.
                author: String::new(),
                // Badges first, on their own line, then the title, then the
                // body. A reader scanning a page of comments decides whether to
                // stop on the badges alone, so they must not be buried in a
                // run-on line with the title — and the footer is the wrong
                // place for the one fact that decides attention.
                //
                // The rule is a labelled line at normal size, not `<sub>`. A
                // rule name is often a whole sentence explaining *why* the
                // finding was raised — "Untrusted-input injection: the diff
                // contains an instruction addressed to the reviewer…" — and
                // shrinking the sentence that justifies the comment to
                // footnote size buries the reasoning under the assertion.
                // `rule_line` splits the class from the explanation so the
                // first is scannable and the second still reads as prose.
                //
                // The suggestion block sits after the prose and before the
                // footer: GitHub renders it as a diff with a commit button, and
                // a reader has to have been told why before being offered the
                // button.
                body: format!(
                    "{}  {}\n\n**{}**\n\n{}{}\n\n{} · <!-- {MARKER_PREFIX}fp={} -->",
                    crate::findings::render::priority_badge(finding.severity),
                    crate::findings::render::lane_confidence_badge(
                        finding.lane,
                        finding.confidence
                    ),
                    finding.title,
                    finding.body,
                    suggestion,
                    crate::findings::render::rule_line(&finding.rule),
                    // The identity review stamped, over the code this finding
                    // anchors to. Recomputing it here from the title — as this
                    // once did — makes the marker depend on the model's
                    // wording, so a rephrased sentence looks like a new finding
                    // and gets posted again on the next push.
                    identity(finding),
                ),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::review::LaneProposal;
    use crate::config::types::{LaneId, Severity};
    use crate::findings::types::Finding;
    use crate::forge::types::{ChangedFile, CheckConclusion, IssueComment, PullRequest};
    use crate::forge::{MockForge, MockState, Write};

    fn config() -> Config {
        crate::config::DEFAULTS
            .parse::<toml::Table>()
            .unwrap()
            .try_into()
            .unwrap()
    }

    fn proposal(head: &str, findings: Vec<Finding>) -> Proposal {
        let highest_severity = findings.iter().map(|finding| finding.severity).max();
        Proposal {
            overview: None,
            unreviewed: vec![],
            skipped: None,
            version: crate::app::review::PROPOSAL_VERSION,
            repo: "tinyhumansai/tinysweeper".into(),
            number: 7,
            head_sha: head.into(),
            lanes: vec![LaneProposal {
                lane: LaneId::Critique,
                check_name: "tinysweeper/critique".into(),
                conclusion: if findings.is_empty() {
                    CheckConclusion::Success
                } else {
                    CheckConclusion::Failure
                },
                summary: "Reviewed.".into(),
                findings,
                noted: Vec::new(),
                resolved: vec![],
                pending: vec![],
                deduped: 0,
                highest_severity,
                usage: Default::default(),
                models: vec![],
                unanswered: vec![],
            }],
            cost_usd: 0.01,
            input_tokens: 10_000,
            output_tokens: 400,
            cached_tokens: 800,
            embed_tokens: 0,
            models: vec!["moonshotai/kimi-k3".into()],
            threads: Default::default(),
        }
    }

    #[tokio::test]
    async fn settling_the_e2e_check_waits_for_the_watched_jobs_then_publishes_once() {
        use crate::lanes::e2e::runs::Watch;
        use crate::state::memory::MemoryState;
        use crate::state::types::ReviewedState;

        let repo = RepoId::parse("tinyhumansai/tinysweeper").unwrap();
        let mut state = MockState::default();
        state.pull_requests.insert(
            7,
            PullRequest {
                number: 7,
                head_sha: "abc123".into(),
                ..PullRequest::default()
            },
        );
        state.set_check("abc123", "playwright", None);
        let forge = MockForge::with_state(state.clone());
        let store = MemoryState::new();
        store
            .save_state(
                &crate::state::key("tinyhumansai/tinysweeper", 7),
                &ReviewedState {
                    head_sha: "abc123".into(),
                    e2e: Some(Watch {
                        head_sha: "abc123".into(),
                        jobs: vec!["playwright".into()],
                        summary: "Coverage looks complete.".into(),
                        failed: false,
                    }),
                    ..ReviewedState::default()
                },
            )
            .await
            .unwrap();

        let settled = settle_e2e(&forge, &forge, &config(), &store, &repo, 7)
            .await
            .expect("settles");
        assert_eq!(settled, E2eSettlement::StillPending);
        assert!(
            forge.writes().is_empty(),
            "nothing published while a job runs"
        );

        state.set_check("abc123", "playwright", Some(CheckConclusion::Failure));
        let forge = MockForge::with_state(state.clone());
        let settled = settle_e2e(&forge, &forge, &config(), &store, &repo, 7)
            .await
            .expect("settles");
        assert_eq!(settled, E2eSettlement::Published(CheckConclusion::Failure));
        let checks = forge.checks();
        let check = checks
            .get("tinysweeper/e2e")
            .expect("the e2e check was published");
        assert_eq!(check.head_sha, "abc123");
        assert_eq!(check.conclusion, Some(CheckConclusion::Failure));
        assert!(
            check.summary.contains("`playwright`: **failure**"),
            "{}",
            check.summary
        );
        assert!(
            check.summary.contains("Coverage looks complete."),
            "{}",
            check.summary
        );

        // The watch is cleared, so the next completion event does nothing.
        let forge = MockForge::with_state(state);
        let settled = settle_e2e(&forge, &forge, &config(), &store, &repo, 7)
            .await
            .expect("settles");
        assert_eq!(settled, E2eSettlement::NothingWatched);
        assert!(forge.writes().is_empty());
    }

    #[tokio::test]
    async fn clearing_the_watch_does_not_discard_a_review_that_landed_concurrently() {
        // The race `clear_e2e_watch` exists to close: `settle_e2e` reads the
        // `abc123` watch, and — in the window before its own write-back — a
        // review of a *newer* push saves its own state: a new head, new
        // fingerprints, and its own watch. A reload-then-unconditional
        // `save_state` at that point would overwrite the new record with the
        // stale one, minus `e2e`. Exercised directly against the store
        // rather than through the full `settle_e2e` flow, which has no seam
        // to inject a concurrent write at that exact point; this is exactly
        // the call `settle_e2e`'s write-back makes.
        use crate::lanes::e2e::runs::Watch;
        use crate::state::memory::MemoryState;
        use crate::state::types::ReviewedState;

        let store = MemoryState::new();
        let key = crate::state::key("tinyhumansai/tinysweeper", 7);
        store
            .save_state(
                &key,
                &ReviewedState {
                    head_sha: "abc123".into(),
                    e2e: Some(Watch {
                        head_sha: "abc123".into(),
                        jobs: vec!["playwright".into()],
                        summary: String::new(),
                        failed: false,
                    }),
                    ..ReviewedState::default()
                },
            )
            .await
            .unwrap();

        // The concurrent write: a review of a newer push landed between
        // `settle_e2e`'s `load_state` (which read the `abc123` watch above)
        // and now.
        let concurrent = ReviewedState {
            head_sha: "newer".into(),
            fingerprints: vec!["fresh-finding".into()],
            e2e: Some(Watch {
                head_sha: "newer".into(),
                jobs: vec!["playwright".into()],
                summary: "Newer review's summary.".into(),
                failed: false,
            }),
            ..ReviewedState::default()
        };
        store.save_state(&key, &concurrent).await.unwrap();

        // `settle_e2e` finishes settling the `abc123` watch it read earlier
        // and tries to clear it.
        let cleared = store.clear_e2e_watch(&key, "abc123").await.unwrap();
        assert!(
            !cleared,
            "the stored watch is for `newer` now, not `abc123`; nothing should match"
        );

        let after = store.load_state(&key).await.unwrap().expect("still there");
        assert_eq!(
            after, concurrent,
            "the concurrent review's state must survive intact"
        );
    }

    #[tokio::test]
    async fn a_moved_head_leaves_the_watch_for_the_next_review_to_replace() {
        use crate::lanes::e2e::runs::Watch;
        use crate::state::memory::MemoryState;
        use crate::state::types::ReviewedState;

        let repo = RepoId::parse("tinyhumansai/tinysweeper").unwrap();
        let mut state = MockState::default();
        state.pull_requests.insert(
            7,
            PullRequest {
                number: 7,
                head_sha: "newer".into(),
                ..PullRequest::default()
            },
        );
        state.set_check("older", "playwright", Some(CheckConclusion::Success));
        let forge = MockForge::with_state(state);
        let store = MemoryState::new();
        let key = crate::state::key("tinyhumansai/tinysweeper", 7);
        store
            .save_state(
                &key,
                &ReviewedState {
                    head_sha: "older".into(),
                    e2e: Some(Watch {
                        head_sha: "older".into(),
                        jobs: vec!["playwright".into()],
                        summary: String::new(),
                        failed: false,
                    }),
                    ..ReviewedState::default()
                },
            )
            .await
            .unwrap();

        let settled = settle_e2e(&forge, &forge, &config(), &store, &repo, 7)
            .await
            .expect("settles");
        assert_eq!(settled, E2eSettlement::HeadMoved);
        assert!(
            forge.writes().is_empty(),
            "a stale verdict is never published"
        );
        assert!(
            store.load_state(&key).await.unwrap().unwrap().e2e.is_some(),
            "the new head's review replaces the record; nothing is cleared here"
        );
    }

    fn finding() -> Finding {
        Finding {
            lane: LaneId::Critique,
            severity: Severity::High,
            confidence: 0.9,
            path: "src/main.rs".into(),
            line: Some(2),
            end_line: None,
            rule: "unchecked-index".into(),
            title: "Guard the index before dereferencing".into(),
            body: "`i` is never bounds-checked.".into(),
            suggestion: None,
            applicable: None,
            late: false,
            identity: None,
            corroboration: 1,
        }
    }

    fn forge(head: &str) -> MockForge {
        forge_draft(head, false)
    }

    fn forge_draft(head: &str, draft: bool) -> MockForge {
        let mut state = MockState::default();
        state.pull_requests.insert(
            7,
            PullRequest {
                number: 7,
                head_sha: head.into(),
                draft,
                ..PullRequest::default()
            },
        );
        state.files.insert(
            7,
            vec![ChangedFile {
                path: "src/main.rs".into(),
                patch: Some("@@ -1,1 +1,2 @@\n fn main() {}\n+let i = 0;\n".into()),
                ..ChangedFile::default()
            }],
        );
        MockForge::with_state(state)
    }

    #[tokio::test]
    async fn a_check_run_is_published_per_lane() {
        let forge = forge("abc123");
        apply(&forge, &forge, &config(), &proposal("abc123", vec![]), None)
            .await
            .expect("applies");

        let checks = forge.checks();
        assert_eq!(
            checks["tinysweeper/critique"].conclusion,
            Some(CheckConclusion::Success)
        );
    }

    #[tokio::test]
    async fn publishing_also_triages_the_pull_request() {
        // Triage rides on `apply` rather than on a job of its own so it reaches
        // every trigger the review already has, and so a labelled severity can
        // never disagree with the check run published beside it.
        let forge = forge("abc123");
        apply(
            &forge,
            &forge,
            &config(),
            &proposal("abc123", vec![finding()]),
            None,
        )
        .await
        .expect("applies");

        let labels = forge
            .writes()
            .into_iter()
            .find_map(|w| match w {
                Write::Labels { labels, .. } => Some(labels),
                _ => None,
            })
            .expect("the pull request was labelled");

        // One label, on the one axis triage owns: the priority derived from the
        // review's own worst finding.
        assert_eq!(labels, vec!["priority: p1"]);
    }

    #[tokio::test]
    async fn a_stale_head_publishes_nothing() {
        // The review ran against a commit that has since been replaced.
        // Publishing would report on code nobody is looking at.
        let forge = forge("newer456");
        apply(&forge, &forge, &config(), &proposal("abc123", vec![]), None)
            .await
            .expect("returns cleanly");

        assert!(forge.wrote_nothing(), "{:#?}", forge.writes());
    }

    #[tokio::test]
    async fn findings_become_inline_comments_carrying_a_fingerprint() {
        let forge = forge("abc123");
        apply(
            &forge,
            &forge,
            &config(),
            &proposal("abc123", vec![finding()]),
            None,
        )
        .await
        .expect("applies");

        let review = forge
            .writes()
            .into_iter()
            .find_map(|w| match w {
                Write::Review { comments, .. } => Some(comments),
                _ => None,
            })
            .expect("a review was posted");

        assert_eq!(review.len(), 1);
        assert_eq!(review[0].path, "src/main.rs");
        assert_eq!(review[0].line, Some(2));
        assert!(
            review[0].body.contains("tinysweeper:fp="),
            "{}",
            review[0].body
        );
    }

    #[tokio::test]
    async fn an_out_of_diff_anchor_is_kept_in_the_review_body_but_not_posted_inline() {
        let forge = forge("abc123");
        let mut invalid = finding();
        invalid.line = Some(99);

        apply(
            &forge,
            &forge,
            &config(),
            &proposal("abc123", vec![invalid]),
            None,
        )
        .await
        .expect("applies without asking GitHub to reject the whole review");

        let (body, event) = review_of(&forge).expect("the blocking verdict remains visible");
        assert_eq!(event, ReviewEvent::RequestChanges);
        assert!(body.contains("Guard the index"), "{body}");
        assert!(
            forge.writes().iter().all(|write| match write {
                Write::Review { comments, .. } => comments.is_empty(),
                _ => true,
            }),
            "{:#?}",
            forge.writes()
        );
    }

    #[tokio::test]
    async fn a_clean_review_posts_no_inline_comments_at_all() {
        let forge = forge("abc123");
        apply(&forge, &forge, &config(), &proposal("abc123", vec![]), None)
            .await
            .expect("applies");

        // The review itself is an approval; what it must not carry is a single
        // inline comment, because there was nothing to say about a line.
        assert!(
            forge.writes().iter().all(|w| match w {
                Write::Review { comments, .. } => comments.is_empty(),
                _ => true,
            }),
            "{:#?}",
            forge.writes()
        );
    }

    fn review_of(forge: &MockForge) -> Option<(String, ReviewEvent)> {
        forge.writes().into_iter().find_map(|w| match w {
            Write::Review { body, event, .. } => Some((body, event)),
            _ => None,
        })
    }

    #[tokio::test]
    async fn a_high_finding_requests_changes_and_blocks_the_merge() {
        let forge = forge("abc123");
        apply(
            &forge,
            &forge,
            &config(),
            &proposal("abc123", vec![finding()]),
            None,
        )
        .await
        .expect("applies");

        let (body, event) = review_of(&forge).expect("a review was posted");
        assert_eq!(event, ReviewEvent::RequestChanges);
        assert!(body.contains("Requesting changes"), "{body}");
        assert!(body.contains("**high**"), "{body}");
    }

    /// A stamped suggestion becomes a one-click block, and the comment widens
    /// to the span it replaces — GitHub substitutes exactly the anchored lines,
    /// so a narrower anchor would delete the rest of the block.
    #[tokio::test]
    async fn an_applicable_suggestion_becomes_a_commit_button_over_its_own_span() {
        let mut f = finding();
        f.applicable = Some(crate::findings::types::Suggestion {
            start_line: 2,
            end_line: 4,
            replacement: "    if let Some(x) = items.get(i) {\n        use_it(x);\n    }".into(),
        });

        let forge = forge("abc123");
        apply(
            &forge,
            &forge,
            &config(),
            &proposal("abc123", vec![f]),
            None,
        )
        .await
        .expect("applies");

        let comment = forge
            .writes()
            .into_iter()
            .find_map(|w| match w {
                Write::Review { comments, .. } => comments.into_iter().next(),
                _ => None,
            })
            .expect("an inline comment");

        assert_eq!(comment.start_line, Some(2));
        assert_eq!(comment.line, Some(4));
        assert!(
            comment
                .body
                .contains("```suggestion\n    if let Some(x) = items.get(i) {"),
            "{}",
            comment.body
        );
        // Before the footer, so the reader has the reason before the button.
        let block = comment.body.find("```suggestion").expect("a block");
        let footer = comment.body.find("**[RULE]").expect("a footer");
        assert!(block < footer, "{}", comment.body);
    }

    /// Without a suggestion the comment stays a single-line pin. Widening it
    /// unconditionally would highlight a whole block for a one-line remark.
    #[tokio::test]
    async fn a_finding_with_no_applicable_suggestion_stays_a_single_line_pin() {
        let forge = forge("abc123");
        apply(
            &forge,
            &forge,
            &config(),
            &proposal("abc123", vec![finding()]),
            None,
        )
        .await
        .expect("applies");

        let comment = forge
            .writes()
            .into_iter()
            .find_map(|w| match w {
                Write::Review { comments, .. } => comments.into_iter().next(),
                _ => None,
            })
            .expect("an inline comment");

        assert_eq!(comment.start_line, None);
        assert_eq!(comment.line, Some(2));
        assert!(!comment.body.contains("```suggestion"), "{}", comment.body);
    }

    #[tokio::test]
    async fn an_inline_comment_leads_with_its_badges() {
        // A reader scanning a page of comments decides whether to stop on the
        // badges, so they go first, on their own line, before the title.
        let forge = forge("abc123");
        apply(
            &forge,
            &forge,
            &config(),
            &proposal("abc123", vec![finding()]),
            None,
        )
        .await
        .expect("applies");

        let body = forge
            .writes()
            .into_iter()
            .find_map(|w| match w {
                Write::Review { comments, .. } => comments.into_iter().next(),
                _ => None,
            })
            .expect("an inline comment")
            .body;
        let mut lines = body.lines();

        let first = lines.next().expect("a first line");
        assert!(first.starts_with("![priority"), "{body}");
        assert!(first.contains("label=priority"), "{body}");
        assert!(first.contains("critique-"), "lane and confidence: {body}");
        assert!(
            !first.contains("**"),
            "the title must not share the badge line: {body}"
        );

        assert!(body.contains("**Guard the index"), "{body}");

        // The rule is a labelled line at readable size, not a `<sub>` footnote.
        // A rule name is frequently a whole sentence explaining why the finding
        // was raised, and shrinking the justification below the assertion it
        // justifies is exactly backwards.
        assert!(
            body.contains("**[RULE] "),
            "the rule needs its label: {body}"
        );
        assert!(
            !body.contains("<sub>"),
            "the rule must not be shrunk to a footnote: {body}"
        );

        // The fingerprint marker must remain the *last* marker in the body:
        // `findings::prior::marker_value` reads it with `rfind`, so anything
        // appended after it would silently break cross-push dedupe.
        let marker = body.rfind("tinysweeper:fp=").expect("a marker");
        assert!(
            body[marker..].find("-->").is_some(),
            "the marker must close: {body}"
        );
        assert!(
            !body[marker..].contains("!["),
            "nothing may follow the fingerprint marker: {body}"
        );
    }

    #[tokio::test]
    async fn publishing_records_the_identities_it_posted() {
        // The store extension used to parse the fingerprint back out of the
        // comment body it had just rendered, looking for `{fp=` while the
        // marker written is `<!-- tinysweeper:fp=… -->`. It never matched, so
        // nothing was ever recorded and the next review re-posted everything.
        // A no-op is indistinguishable from success without this test.
        let mut posted = finding();
        posted.identity = Some("0123456789abcdef".into());

        let store = crate::state::MemoryState::default();
        let key = crate::state::key("tinyhumansai/tinysweeper", 7);
        store
            .save_state(&key, &Default::default())
            .await
            .expect("seeds");

        let forge = forge("abc123");
        apply(
            &forge,
            &forge,
            &config(),
            &proposal("abc123", vec![posted]),
            Some(&store),
        )
        .await
        .expect("applies");

        let recorded = store.load_state(&key).await.expect("loads").expect("state");
        assert!(
            recorded
                .fingerprints
                .contains(&"0123456789abcdef".to_string()),
            "{:?}",
            recorded.fingerprints
        );
    }

    #[tokio::test]
    async fn an_unanchored_blocking_finding_still_submits_the_request_changes_verdict() {
        // When a finding could not be anchored to a line, it has no inline
        // comment but still appears in the review body/summary. The RequestChanges
        // verdict must still be submitted to enforce the gate, even though no
        // inline comments are present.
        let mut unanchored = finding();
        unanchored.line = None;

        let forge = forge("abc123");
        apply(
            &forge,
            &forge,
            &config(),
            &proposal("abc123", vec![unanchored]),
            None,
        )
        .await
        .expect("applies");

        let (body, event) = review_of(&forge).expect("a review was posted");
        assert_eq!(event, ReviewEvent::RequestChanges);
        assert!(body.contains("Requesting changes"), "{body}");
        // No inline comments because the finding is unanchored
        let writes = forge.writes();
        assert!(!writes.iter().any(|w| matches!(w, Write::Review {
            comments,
            ..
        } if !comments.is_empty())));
    }

    #[tokio::test]
    async fn a_finding_below_the_threshold_only_comments() {
        let mut low = finding();
        low.severity = Severity::Medium;

        let forge = forge("abc123");
        apply(
            &forge,
            &forge,
            &config(),
            &proposal("abc123", vec![low]),
            None,
        )
        .await
        .expect("applies");

        assert_eq!(review_of(&forge).expect("posted").1, ReviewEvent::Comment);
    }

    #[tokio::test]
    async fn a_fixed_pull_request_has_its_block_cleared() {
        // The half that matters most. GitHub keeps only the latest review per
        // reviewer, so without an explicit approval a stale objection blocks
        // the merge button until a human dismisses it by hand.
        let forge = forge("abc123").with_own_review(7, ReviewEvent::RequestChanges);
        apply(&forge, &forge, &config(), &proposal("abc123", vec![]), None)
            .await
            .expect("applies");

        let (body, event) = review_of(&forge).expect("an approval was posted");
        assert_eq!(event, ReviewEvent::Approve);
        assert!(body.contains("Clearing the changes request"), "{body}");
    }

    #[tokio::test]
    async fn a_deduped_high_finding_keeps_an_existing_block() {
        let forge = forge("abc123").with_own_review(7, ReviewEvent::RequestChanges);
        let mut proposal = proposal("abc123", vec![]);
        proposal.lanes[0].conclusion = CheckConclusion::Failure;
        proposal.lanes[0].highest_severity = Some(Severity::High);
        proposal.lanes[0].deduped = 1;

        apply(&forge, &forge, &config(), &proposal, None)
            .await
            .expect("applies");

        assert_eq!(
            review_of(&forge).expect("a blocking review was posted").1,
            ReviewEvent::RequestChanges
        );
    }

    #[tokio::test]
    async fn a_clean_pull_request_is_approved() {
        let forge = forge("abc123");
        apply(&forge, &forge, &config(), &proposal("abc123", vec![]), None)
            .await
            .expect("applies");

        let (body, event) = review_of(&forge).expect("an approval was posted");
        assert_eq!(event, ReviewEvent::Approve);
        // A first approval must not claim to be clearing an objection that was
        // never made.
        assert!(
            !body.contains("Clearing the changes request"),
            "nothing was blocking, so there is nothing to clear: {body}"
        );
    }

    #[tokio::test]
    async fn a_draft_pull_request_is_never_approved() {
        // The trap: with `review.draft_prs = false` every lane *skips* a draft,
        // so the proposal has nothing blocking and nothing unreviewed — which
        // reads as "clean" to both approval conditions. The bot would approve a
        // pull request it had deliberately declined to look at, and on a
        // repository requiring a review that approval is what lets it merge.
        let forge = forge_draft("abc123", true);
        apply(&forge, &forge, &config(), &proposal("abc123", vec![]), None)
            .await
            .expect("applies");

        if let Some((body, event)) = review_of(&forge) {
            assert_ne!(
                event,
                ReviewEvent::Approve,
                "a lane that skipped a draft vouched for nothing: {body}"
            );
        }
    }

    #[tokio::test]
    async fn a_draft_that_was_blocked_before_is_still_cleared() {
        // Refusing to approve a draft must not strand an author behind an
        // objection that no longer applies. Clearing a block is not an
        // endorsement, and it is the one case where a draft still gets one.
        let forge = forge_draft("abc123", true).with_own_review(7, ReviewEvent::RequestChanges);
        apply(&forge, &forge, &config(), &proposal("abc123", vec![]), None)
            .await
            .expect("applies");

        let (body, event) = review_of(&forge).expect("a clearing review was posted");
        assert_eq!(event, ReviewEvent::Approve, "{body}");
    }

    #[tokio::test]
    async fn a_pull_request_with_unread_files_is_not_approved() {
        // The reason the aggregate check run could be removed. It used to carry
        // this by degrading itself to `Neutral`; the approval carries it now, so
        // if this regresses the bot endorses a change it never saw — and does it
        // silently, because nothing else reports the gap as a verdict.
        let mut incomplete = proposal("abc123", vec![]);
        incomplete.unreviewed = vec!["src/generated_huge.rs".into()];

        let forge = forge("abc123");
        apply(&forge, &forge, &config(), &incomplete, None)
            .await
            .expect("applies");

        match review_of(&forge) {
            None => {}
            Some((body, event)) => assert_ne!(
                event,
                ReviewEvent::Approve,
                "nothing blocks, but nothing is vouched for either: {body}"
            ),
        }
    }

    #[tokio::test]
    async fn a_review_nobody_answered_is_not_approved() {
        // The production case, 2026-09-15: every model call 403'd on an
        // exhausted gateway budget, every lane came back Neutral with "could
        // not be reviewed", nothing blocked — and the bot posted "found
        // nothing blocking. Approving. $0.0000 · 0 in / 0 out". Neutral does
        // not block, so the only thing standing between that and an approval
        // is the lane saying what it never got an answer on.
        let mut unanswered = proposal("abc123", vec![]);
        for lane in &mut unanswered.lanes {
            lane.conclusion = CheckConclusion::Neutral;
            lane.summary = "Reviewed 0 files; 0 findings. 1 file could not be reviewed.".into();
            lane.unanswered = vec!["src/lib.rs".into()];
        }
        assert!(!unanswered.blocked());
        assert!(!unanswered.complete());

        let forge = forge("abc123");
        apply(&forge, &forge, &config(), &unanswered, None)
            .await
            .expect("applies");

        let (body, event) = review_of(&forge).expect("a verdict is posted");
        assert_ne!(event, ReviewEvent::Approve, "{body}");
        assert_ne!(event, ReviewEvent::RequestChanges, "{body}");
        assert!(
            body.contains("not an approval") && body.contains("`src/lib.rs`"),
            "the reader is told why: {body}"
        );
    }

    #[tokio::test]
    async fn a_review_nobody_answered_does_not_clear_an_earlier_block() {
        // The other direction of the same outage: the last review requested
        // changes, the next push finds every model call failing. "Clean now"
        // is not a finding when nobody looked, so the block stands — and the
        // comment says why rather than leaving the author to guess.
        let mut unanswered = proposal("abc123", vec![]);
        for lane in &mut unanswered.lanes {
            lane.conclusion = CheckConclusion::Neutral;
            lane.unanswered = vec!["src/lib.rs".into()];
        }

        let forge = forge("abc123").with_own_review(7, ReviewEvent::RequestChanges);
        apply(&forge, &forge, &config(), &unanswered, None)
            .await
            .expect("applies");

        let (body, event) = review_of(&forge).expect("a verdict is posted");
        assert_ne!(
            event,
            ReviewEvent::Approve,
            "an outage must not clear a block: {body}"
        );
        assert!(body.contains("not an approval"), "{body}");
    }

    #[tokio::test]
    async fn a_review_nobody_answered_withdraws_the_approval_that_stood_before_it() {
        // GitHub keeps an approval in force under any number of comments, so
        // the comment alone would leave the bot vouching for a push it never
        // read. The approval is dismissed, with the reason.
        let mut unanswered = proposal("abc123", vec![]);
        for lane in &mut unanswered.lanes {
            lane.conclusion = CheckConclusion::Neutral;
            lane.unanswered = vec!["src/lib.rs".into()];
        }

        let forge = forge("abc123").with_own_review(7, ReviewEvent::Approve);
        apply(&forge, &forge, &config(), &unanswered, None)
            .await
            .expect("applies");

        let dismissed = forge.writes().into_iter().find_map(|w| match w {
            Write::DismissApproval { message, .. } => Some(message),
            _ => None,
        });
        let message = dismissed.expect("the standing approval is withdrawn");
        assert!(message.contains("could not review"), "{message}");
        let (body, event) = review_of(&forge).expect("and the reason is posted");
        assert_ne!(event, ReviewEvent::Approve, "{body}");

        // A clean push that *was* answered dismisses nothing.
        let answered = self::tests::forge("abc123").with_own_review(7, ReviewEvent::Approve);
        apply(
            &answered,
            &answered,
            &config(),
            &proposal("abc123", vec![]),
            None,
        )
        .await
        .expect("applies");
        assert!(
            !answered
                .writes()
                .iter()
                .any(|w| matches!(w, Write::DismissApproval { .. })),
            "nothing to withdraw from a review that answered"
        );
    }

    #[tokio::test]
    async fn an_unanswered_review_dismisses_even_when_its_own_history_cannot_be_read() {
        // A failed lookup of our own past verdicts must not shield an
        // approval that may be standing: the dismissal is attempted anyway,
        // and is a no-op when nothing stands.
        let mut unanswered = proposal("abc123", vec![]);
        for lane in &mut unanswered.lanes {
            lane.conclusion = CheckConclusion::Neutral;
            lane.unanswered = vec!["src/lib.rs".into()];
        }
        let forge = forge("abc123")
            .with_own_review(7, ReviewEvent::Approve)
            .failing_own_review_state();
        apply(&forge, &forge, &config(), &unanswered, None)
            .await
            .expect("applies");
        assert!(
            forge
                .writes()
                .iter()
                .any(|w| matches!(w, Write::DismissApproval { .. })),
            "the dismissal is attempted when the history is unreadable"
        );
    }

    #[tokio::test]
    async fn a_kill_switched_pull_request_is_neither_approved_nor_commented_on() {
        // Every lane skipped, nothing unreviewed, nothing unanswered: clean
        // by every other measure, and the one review nobody asked for.
        let mut skipped = proposal("abc123", vec![]);
        for lane in &mut skipped.lanes {
            lane.conclusion = CheckConclusion::Neutral;
            lane.summary = "Skipped: `do-not-review` is applied.".into();
        }
        skipped.skipped = Some("`do-not-review` is applied".into());
        assert!(!skipped.complete());

        let forge = forge("abc123").with_own_review(7, ReviewEvent::Approve);
        apply(&forge, &forge, &config(), &skipped, None)
            .await
            .expect("applies");
        assert!(review_of(&forge).is_none(), "nobody asked for a verdict");
        assert!(
            !forge
                .writes()
                .iter()
                .any(|w| matches!(w, Write::DismissApproval { .. })),
            "and nothing is withdrawn for it either"
        );
    }

    #[tokio::test]
    async fn a_blocking_verdict_is_posted_without_a_dismissal_first() {
        // A changes request supersedes the approval by itself; a dismissal
        // call in front of it is one more thing that can fail before the
        // verdict that matters is posted.
        let mut mixed = proposal("abc123", vec![finding()]);
        // The lane blocks on one file and got no answer on another.
        mixed.lanes[0].unanswered = vec!["src/other.rs".into()];
        assert!(!mixed.complete());
        let forge = forge("abc123").with_own_review(7, ReviewEvent::Approve);
        apply(&forge, &forge, &config(), &mixed, None)
            .await
            .expect("applies");
        let (_, event) = review_of(&forge).expect("the block is posted");
        assert_eq!(event, ReviewEvent::RequestChanges);
        assert!(
            !forge
                .writes()
                .iter()
                .any(|w| matches!(w, Write::DismissApproval { .. }))
        );
    }

    #[tokio::test]
    async fn a_version_one_proposal_is_never_approved() {
        // Written by a `review` that did not record what went unanswered:
        // its silence is not an answer, so it can post but not endorse.
        let mut legacy = proposal("abc123", vec![]);
        legacy.version = 1;
        assert!(!legacy.complete());
        let mut newer = proposal("abc123", vec![]);
        newer.version = crate::app::review::PROPOSAL_VERSION + 1;
        assert!(
            !newer.complete(),
            "nor is one this binary cannot fully read"
        );

        let forge = forge("abc123");
        apply(&forge, &forge, &config(), &legacy, None)
            .await
            .expect("applies");
        if let Some((body, event)) = review_of(&forge) {
            assert_ne!(event, ReviewEvent::Approve, "{body}");
        }
    }

    #[test]
    fn a_contributor_path_cannot_break_out_of_its_code_span() {
        assert_eq!(code_span("src/lib.rs"), "`src/lib.rs`");
        let hostile = "x`.rs` **bold**\n# heading";
        let rendered = code_span(hostile);
        assert!(rendered.starts_with("`` "), "{rendered}");
        assert!(!rendered.contains('\n'), "{rendered}");
        assert!(rendered.ends_with(" ``"), "{rendered}");
    }

    #[tokio::test]
    async fn a_withdrawal_that_fails_still_posts_the_reason_and_then_fails_the_run() {
        let mut unanswered = proposal("abc123", vec![]);
        for lane in &mut unanswered.lanes {
            lane.conclusion = CheckConclusion::Neutral;
            lane.unanswered = vec!["src/lib.rs".into()];
        }
        let forge = forge("abc123")
            .with_own_review(7, ReviewEvent::Approve)
            .failing_dismissals();
        let outcome = apply(&forge, &forge, &config(), &unanswered, None).await;
        let (body, event) = review_of(&forge).expect("the reason is still posted");
        assert_ne!(event, ReviewEvent::Approve, "{body}");
        assert!(body.contains("not an approval"), "{body}");
        assert!(
            outcome.is_err(),
            "an approval that may still stand over an unreviewed push is not a success"
        );
    }

    #[tokio::test]
    async fn unread_files_do_not_block_either() {
        // The other half. Refusing to approve is not the same as objecting: we
        // do not know there is a problem, only that we did not look, and
        // blocking would punish the contributor for the forge's truncation.
        let mut incomplete = proposal("abc123", vec![]);
        incomplete.unreviewed = vec!["src/generated_huge.rs".into()];

        let forge = forge("abc123");
        apply(&forge, &forge, &config(), &incomplete, None)
            .await
            .expect("applies");

        if let Some((body, event)) = review_of(&forge) {
            assert_ne!(event, ReviewEvent::RequestChanges, "{body}");
        }
    }

    #[tokio::test]
    async fn approving_can_be_turned_off_without_turning_blocking_off() {
        let mut config = config();
        config.review.approve_when_clean = false;

        let forge = forge("abc123");
        apply(&forge, &forge, &config, &proposal("abc123", vec![]), None)
            .await
            .expect("applies");

        assert!(review_of(&forge).is_none(), "{:#?}", forge.writes());
    }

    #[tokio::test]
    async fn an_approval_that_already_stands_is_not_restated() {
        // Otherwise every push to a clean pull request adds a review that
        // changes nothing on the merge button.
        let forge = forge("abc123").with_own_review(7, ReviewEvent::Approve);
        apply(&forge, &forge, &config(), &proposal("abc123", vec![]), None)
            .await
            .expect("applies");

        assert!(review_of(&forge).is_none(), "{:#?}", forge.writes());
    }

    #[tokio::test]
    async fn a_previous_block_is_cleared_by_an_approval_naming_it() {
        let forge = forge("abc123").with_own_review(7, ReviewEvent::RequestChanges);
        apply(&forge, &forge, &config(), &proposal("abc123", vec![]), None)
            .await
            .expect("applies");

        let (body, event) = review_of(&forge).expect("a clearing review was posted");
        assert_eq!(event, ReviewEvent::Approve);
        assert!(
            body.contains("Clearing the changes request"),
            "the author needs to be told the block is gone: {body}"
        );
    }

    #[tokio::test]
    async fn a_failing_lane_is_never_approved_just_because_blocking_is_off() {
        // `request_changes_at = "off"` asks tinysweeper to stop objecting. It
        // does not ask it to start endorsing a pull request whose gate is red.
        let mut config = config();
        config.review.request_changes_at = "off".into();

        let forge = forge("abc123");
        apply(
            &forge,
            &forge,
            &config,
            &proposal("abc123", vec![finding()]),
            None,
        )
        .await
        .expect("applies");

        assert_eq!(review_of(&forge).expect("posted").1, ReviewEvent::Comment);
    }

    #[tokio::test]
    async fn blocking_can_be_turned_off_entirely() {
        let mut config = config();
        config.review.request_changes_at = "off".into();

        let forge = forge("abc123");
        apply(
            &forge,
            &forge,
            &config,
            &proposal("abc123", vec![finding()]),
            None,
        )
        .await
        .expect("applies");

        assert_eq!(review_of(&forge).expect("posted").1, ReviewEvent::Comment);
    }

    #[tokio::test]
    async fn the_review_body_reports_cost_and_cache_hits() {
        let forge = forge("abc123");
        apply(
            &forge,
            &forge,
            &config(),
            &proposal("abc123", vec![finding()]),
            None,
        )
        .await
        .expect("applies");

        let body = forge
            .writes()
            .into_iter()
            .find_map(|w| match w {
                Write::Review { body, .. } => Some(body),
                _ => None,
            })
            .expect("review posted");

        assert!(body.contains("$0.0100"), "{body}");
        assert!(body.contains("10,000 in"), "{body}");
        assert!(body.contains("400 out"), "{body}");
        assert!(body.contains("800 cached (8%)"), "{body}");
        assert!(body.contains("kimi-k3"), "{body}");
    }
    // --- the change map ----------------------------------------------------

    /// A proposal carrying one changed behaviour and its caller.
    fn proposal_with_map(head: &str) -> Proposal {
        use crate::evidence::diff::parse_file_patch;
        use crate::index::types::{EdgeKind, GraphEdge, GraphNode, Neighbourhood};

        let diffs = [parse_file_patch(
            "src/lanes/critique.rs",
            "@@ -1,1 +1,2 @@ fn review() {\n x\n+y\n",
        )];
        let walk = Neighbourhood {
            nodes: vec![
                GraphNode::symbol("o/r", "src/lanes/critique.rs", "review"),
                GraphNode::symbol("o/r", "src/app/run.rs", "run"),
            ],
            edges: vec![GraphEdge::new(
                "o/r",
                "src/app/run.rs#run",
                "src/lanes/critique.rs#review",
                EdgeKind::Calls,
                "src/app/run.rs",
            )],
        };
        Proposal {
            overview: Some(crate::overview::build(
                &diffs,
                &[],
                crate::overview::GraphView::Walked(&walk),
                &config().overview,
            )),
            ..proposal(head, vec![])
        }
    }

    fn overview_comments(forge: &MockForge) -> Vec<Write> {
        forge
            .writes()
            .into_iter()
            .filter(|write| match write {
                Write::Comment { body, .. } | Write::CommentUpdate { body, .. } => {
                    body.contains(crate::overview::MARKER)
                }
                _ => false,
            })
            .collect()
    }

    #[tokio::test]
    async fn a_pull_request_gets_one_change_map_comment() {
        let forge = forge("abc123");
        apply(
            &forge,
            &forge,
            &config(),
            &proposal_with_map("abc123"),
            None,
        )
        .await
        .expect("applies");

        let posted = overview_comments(&forge);
        assert_eq!(posted.len(), 1, "{posted:#?}");
        assert!(
            matches!(&posted[0], Write::Comment { body, .. } if body.contains("```mermaid")),
            "{posted:#?}"
        );
    }

    #[tokio::test]
    async fn a_second_push_edits_the_same_comment_rather_than_adding_one() {
        // The whole reason the map carries a marker. A fresh diagram per push
        // turns a pull request into a scroll bar, and the older diagrams are
        // all wrong by then.
        let forge = forge("abc123").with_comments(
            7,
            vec![IssueComment {
                id: Some(4242),
                author: "tinysweeper[bot]".into(),
                body: format!("{}\n\nan earlier diagram", crate::overview::MARKER),
            }],
        );

        apply(
            &forge,
            &forge,
            &config(),
            &proposal_with_map("abc123"),
            None,
        )
        .await
        .expect("applies");

        let posted = overview_comments(&forge);
        assert_eq!(posted.len(), 1, "{posted:#?}");
        assert!(
            matches!(&posted[0], Write::CommentUpdate { comment_id, .. } if *comment_id == 4242),
            "{posted:#?}"
        );
    }

    #[tokio::test]
    async fn a_contributor_who_copies_the_marker_does_not_get_their_comment_edited() {
        // Anyone can paste a marker into their own comment. Editing it because
        // of that would be a write we were tricked into making, and it would
        // destroy somebody's words.
        let forge = forge("abc123").with_comments(
            7,
            vec![IssueComment {
                id: Some(4242),
                author: "helpful-contributor".into(),
                body: format!("{} nice bot", crate::overview::MARKER),
            }],
        );

        apply(
            &forge,
            &forge,
            &config(),
            &proposal_with_map("abc123"),
            None,
        )
        .await
        .expect("applies");

        let posted = overview_comments(&forge);
        assert_eq!(posted.len(), 1, "{posted:#?}");
        assert!(
            matches!(&posted[0], Write::Comment { .. }),
            "a new comment, not an edit of theirs: {posted:#?}"
        );
    }

    #[tokio::test]
    async fn turning_the_map_off_posts_no_comment() {
        let forge = forge("abc123");
        let mut config = config();
        config.overview.enabled = false;

        apply(&forge, &forge, &config, &proposal_with_map("abc123"), None)
            .await
            .expect("applies");

        assert!(
            overview_comments(&forge).is_empty(),
            "{:#?}",
            forge.writes()
        );
    }

    #[tokio::test]
    async fn a_stale_head_draws_nothing_either() {
        let forge = forge("def456");
        apply(
            &forge,
            &forge,
            &config(),
            &proposal_with_map("abc123"),
            None,
        )
        .await
        .expect("applies");

        assert!(
            overview_comments(&forge).is_empty(),
            "{:#?}",
            forge.writes()
        );
    }

    #[tokio::test]
    async fn a_proposal_written_before_the_map_existed_still_publishes() {
        // `overview: None` is what an old `findings.json` deserialises to, and
        // it must mean "no map was built", never "the change touches nothing".
        let forge = forge("abc123");
        apply(&forge, &forge, &config(), &proposal("abc123", vec![]), None)
            .await
            .expect("applies");

        assert!(overview_comments(&forge).is_empty());
        assert!(!forge.checks().is_empty(), "the verdict still went out");
    }

    #[tokio::test]
    async fn a_pull_request_with_no_review_to_submit_still_gets_its_map() {
        // The reason the map is its own comment rather than a paragraph in the
        // review body. This pull request is clean and already approved, so no
        // review is submitted at all — and it is exactly the pull request whose
        // reviewer has nothing but the files tab to go on.
        let forge = forge("abc123").with_own_review(7, ReviewEvent::Approve);
        apply(
            &forge,
            &forge,
            &config(),
            &proposal_with_map("abc123"),
            None,
        )
        .await
        .expect("applies");

        assert!(
            !forge
                .writes()
                .iter()
                .any(|write| matches!(write, Write::Review { .. })),
            "{:#?}",
            forge.writes()
        );
        assert_eq!(overview_comments(&forge).len(), 1);
    }

    #[tokio::test]
    async fn a_planned_thread_is_resolved_when_the_verdict_is_published() {
        // The mutation half of thread resolution. The decision was taken during
        // review, deterministically; apply only executes it, and only for the
        // threads the plan names.
        let forge = forge("abc123");
        let mut proposal = proposal("abc123", vec![]);
        proposal.threads = crate::threads::ThreadPlan {
            resolve: vec![crate::threads::PlannedResolve {
                id: "PRRT_1".into(),
                reason: "the finding no longer reproduces on the new code".into(),
            }],
        };

        apply(&forge, &forge, &config(), &proposal, None)
            .await
            .expect("applies");

        assert!(
            forge
                .writes()
                .contains(&crate::forge::mock::Write::ThreadResolved {
                    thread_id: "PRRT_1".into()
                }),
            "{:?}",
            forge.writes()
        );
    }

    #[tokio::test]
    async fn a_stale_verdict_resolves_nothing() {
        // The head moved, so the whole verdict is withheld — including the
        // thread plan, which was computed against findings from a commit that
        // is no longer what the pull request proposes.
        let forge = forge("def456");
        let mut proposal = proposal("abc123", vec![]);
        proposal.threads = crate::threads::ThreadPlan {
            resolve: vec![crate::threads::PlannedResolve {
                id: "PRRT_1".into(),
                reason: "stale".into(),
            }],
        };

        apply(&forge, &forge, &config(), &proposal, None)
            .await
            .expect("applies");

        assert!(forge.writes().is_empty(), "{:?}", forge.writes());
    }
}
