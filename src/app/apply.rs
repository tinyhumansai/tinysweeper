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
use crate::forge::types::{ChangedFile, CheckRun, OwnReview, RepoId, ReviewComment, ReviewEvent};
use crate::ports::forge::{ForgeRead, ForgeWrite};
use crate::ports::review_state::ReviewStateStore;
use crate::{MARKER_PREFIX, VERSION};

/// Publish the deterministic queued status already authorized by server admission.
///
/// This exception carries only a server-decided status plan. It does not accept
/// contributor-authored content or a model proposal.
#[cfg(feature = "serve")]
pub async fn publish_queued_review_status(
    write: &dyn ForgeWrite,
    repo: &RepoId,
    head_sha: &str,
    reviews_ahead: usize,
    external_id: &str,
) -> Result<u64> {
    write
        .publish_check(
            repo,
            crate::server::status::queued(head_sha, reviews_ahead, external_id),
        )
        .await
}

/// Transition the previously published queue check under the same status-only write boundary.
#[cfg(feature = "serve")]
pub async fn start_queued_review_status(
    write: &dyn ForgeWrite,
    repo: &RepoId,
    check_id: u64,
    head_sha: &str,
) -> Result<()> {
    write
        .update_check(repo, check_id, crate::server::status::in_progress(head_sha))
        .await
}

/// Conclude a detached queued check after its review job was superseded.
#[cfg(feature = "serve")]
pub async fn retire_queued_review_status(
    write: &dyn ForgeWrite,
    repo: &RepoId,
    check_id: u64,
    head_sha: &str,
) -> Result<()> {
    write
        .update_check(
            repo,
            check_id,
            crate::server::status::queued_review_incomplete(head_sha, None),
        )
        .await
}

/// Publish a decided terminal status when a superseded queued run has no
/// recoverable ID. This remains a status-only write plan from server policy.
#[cfg(feature = "serve")]
pub async fn publish_superseded_review_status(
    write: &dyn ForgeWrite,
    repo: &RepoId,
    head_sha: &str,
    external_id: &str,
) -> Result<u64> {
    write
        .publish_check(
            repo,
            crate::server::status::queued_review_incomplete(head_sha, Some(external_id)),
        )
        .await
}

/// Whether a review proposal was published against the live pull request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplyDisposition {
    /// The review was published (or was already present for this exact head).
    Published,
    /// The pull request head changed before publication.
    Superseded,
    /// The pull request was closed or merged before publication.
    Closed,
}

/// A fully decided MCP issue creation.
#[cfg(feature = "serve")]
pub struct McpIssuePlan {
    /// Repository already canonicalised and authorised by the planner.
    pub repo: RepoId,
    /// GitHub App installation covering the repository.
    pub installation: u64,
    /// Final issue title.
    pub title: String,
    /// Final template-aware, enriched body.
    pub body: String,
    /// Labels requested by the authenticated caller.
    pub labels: Vec<String>,
}

/// Which side of the GitHub create request an MCP apply failure occurred on.
#[cfg(feature = "serve")]
#[derive(Debug, thiserror::Error)]
pub enum McpIssueApplyError {
    /// No create request was sent, so the caller may safely release its claim.
    #[error("{0}")]
    BeforeWrite(Error),
    /// A create request was attempted and may have succeeded despite the error.
    #[error("{0}")]
    Ambiguous(Error),
}

/// Mint the write credential and execute one previously decided MCP issue plan.
#[cfg(feature = "serve")]
pub async fn apply_mcp_issue(
    auth: &crate::server::auth::AppAuth,
    plan: &McpIssuePlan,
) -> std::result::Result<u64, McpIssueApplyError> {
    let token = auth
        .installation_token(plan.installation)
        .await
        .map_err(McpIssueApplyError::BeforeWrite)?;
    let write =
        crate::forge::github::GitHubWrite::new(&token).map_err(McpIssueApplyError::BeforeWrite)?;
    execute_mcp_issue(&write, plan)
        .await
        .map_err(McpIssueApplyError::Ambiguous)
}

#[cfg(feature = "serve")]
async fn execute_mcp_issue(write: &dyn ForgeWrite, plan: &McpIssuePlan) -> Result<u64> {
    write
        .create_issue(&plan.repo, &plan.title, &plan.body, &plan.labels)
        .await
}

/// Publish a proposal.
///
/// If a store is provided, extends the stored fingerprints with the identities
/// of newly posted findings after successful publication, so the next review
/// will dedupe them correctly.
/// Publish a proposal and report whether its reviewed head was still current.
pub async fn apply(
    read: &dyn ForgeRead,
    write: &dyn ForgeWrite,
    config: &Config,
    proposal: &Proposal,
    store: Option<&dyn ReviewStateStore>,
) -> Result<ApplyDisposition> {
    let repo = RepoId::parse(&proposal.repo)
        .ok_or_else(|| Error::Forge(format!("`{}` is not owner/name", proposal.repo)))?;

    // Re-validate against live state. The review ran minutes ago in another
    // job, and the world moves.
    let live = read.pull_request(&repo, proposal.number).await?;
    if !live.open || live.merged {
        tracing::info!(
            number = proposal.number,
            merged = live.merged,
            "pull request is closed; not publishing the review"
        );
        return Ok(ApplyDisposition::Closed);
    }
    if live.head_sha != proposal.head_sha {
        tracing::info!(
            reviewed = %proposal.head_sha,
            live = %live.head_sha,
            "head moved since the review; not publishing a stale verdict"
        );
        return Ok(ApplyDisposition::Superseded);
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
                    title: title_for(lane.findings.len(), lane.coverage.as_ref()),
                    summary: render_lane_summary(lane),
                    status: None,
                    external_id: None,
                    images: vec![],
                },
            )
            .await?;
    }

    // What tinysweeper already said about this pull request. GitHub keeps only
    // the latest review per reviewer, which makes this load-bearing twice: it
    // is how a fixed pull request gets unblocked — without an explicit clearing
    // verdict a stale objection blocks the merge button until a human dismisses
    // it by hand — and it is how an approval of this same commit avoids being
    // restated on a repeated review.
    let (previous_review, previous_known) = own_review_state(read, &repo, proposal.number).await;
    let previous = previous_review.as_ref().map(|review| review.event);
    let event = review_event(config, proposal, previous, live.draft);
    let blocking_findings = blocking_finding_set(proposal);
    let published_before = match store {
        Some(store) => store
            .load_state(&crate::state::key(&proposal.repo, proposal.number))
            .await
            .ok()
            .flatten()
            .and_then(|state| state.published_review),
        None => None,
    };
    let same_published_decision = published_before.as_ref().is_some_and(|published| {
        blocking_findings.as_ref().is_some_and(|blocking| {
            published.event == event && published.blocking_findings == *blocking
        })
    });
    let same_standing_review = previous_known
        && published_before
            .as_ref()
            .is_some_and(|published| match event {
                ReviewEvent::Approve | ReviewEvent::RequestChanges => {
                    previous_review.as_ref().is_some_and(|review| {
                        review.event == event
                            && review.head_sha.as_deref() == Some(published.head_sha.as_str())
                    })
                }
                ReviewEvent::Comment => match (&published.standing_review, &previous_review) {
                    (Some(expected), Some(actual)) => expected == actual,
                    // Before standing snapshots were persisted, only a live
                    // COMMENT on the exact published head is enough evidence.
                    (None, Some(actual)) => {
                        actual.event == ReviewEvent::Comment
                            && actual.head_sha.as_deref() == Some(published.head_sha.as_str())
                    }
                    _ => false,
                },
            });
    // Every lane is expected to leave an unpostable finding without a line,
    // but `apply` is the final boundary before GitHub sees it. One invalid
    // anchor makes GitHub reject the entire review, including otherwise valid
    // comments, so derive the postable set from the live diff here as well.
    let files = read.changed_files(&repo, proposal.number).await?;
    let comments = inline_comments(proposal, &files);
    let posted: std::collections::BTreeSet<String> = comments
        .iter()
        .flat_map(|comment| fingerprints(&comment.body))
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
        .flat_map(|comment| fingerprints(&comment.body))
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
    // Suppress a repeated approval only for the exact commit already reviewed.
    // A changed head needs its own verdict, even when the old approval still
    // stands on GitHub. Unknown commit metadata cannot certify this head.
    let redundant_approval = event == ReviewEvent::Approve
        && previous == Some(ReviewEvent::Approve)
        && previous_review
            .as_ref()
            .and_then(|review| review.head_sha.as_deref())
            == Some(proposal.head_sha.as_str())
        && comments.is_empty();

    // A known publication can be repeated on a new head only while that exact
    // decision still stands in forge history. Approvals require complete
    // current-head coverage; a later incomplete push must still withdraw them.
    let redundant_decision = same_published_decision
        && same_standing_review
        && match event {
            ReviewEvent::Approve => proposal.complete(),
            ReviewEvent::RequestChanges => true,
            ReviewEvent::Comment => {
                published_before
                    .as_ref()
                    .and_then(|published| published.coverage_complete)
                    == Some(proposal.complete())
            }
        };

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
    let mut review_published = false;
    if !redundant_approval
        && !redundant_decision
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
        review_published = true;

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

    record_apply_state(
        store,
        proposal,
        if review_published {
            blocking_findings.map(|blocking_findings| {
                (
                    event,
                    blocking_findings,
                    standing_review_after_success(
                        event,
                        proposal,
                        previous_review.as_ref(),
                        previous_known,
                        unvouched,
                        withdrawal_failed.is_some(),
                    ),
                    proposal.complete(),
                )
            })
        } else {
            None
        },
    )
    .await;

    // The change map, as one comment edited in place forever. Deliberately
    // *not* folded into the review body: a review is submitted only when there
    // is a verdict to give, and the pull request that most needs a picture of
    // itself is often the clean one that gets no inline comments at all.
    //
    // Best effort, and last-but-one on purpose. It is the only thing published
    // here that nobody is gated on, so a failure to draw it must not cost the
    // verdict that was already posted above.
    if let Err(err) = publish_review_hub(read, write, config, proposal, store).await {
        tracing::warn!(%err, "could not publish the durable review hub");
    }

    // The wireframe gallery: its own comment, for the same reason it is not
    // folded into the review hub — a screen-by-screen ASCII gallery can be
    // the bulkiest thing this bot posts, and burying it in the narrative
    // summary is how nobody scrolls to it. Best effort, same as the hub.
    if let Err(err) = publish_wireframe(read, write, proposal).await {
        tracing::warn!(%err, "could not publish the UI wireframe gallery");
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
        None => Ok(ApplyDisposition::Published),
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
async fn publish_review_hub(
    read: &dyn ForgeRead,
    write: &dyn ForgeWrite,
    config: &Config,
    proposal: &Proposal,
    store: Option<&dyn ReviewStateStore>,
) -> Result<()> {
    if !config.summary.enabled || proposal.skipped.is_some() {
        return Ok(());
    }
    let body = crate::summary::render(config, proposal);

    let repo = RepoId::parse(&proposal.repo)
        .ok_or_else(|| Error::Forge(format!("`{}` is not owner/name", proposal.repo)))?;

    // A comment with no id cannot be edited, so it falls through to posting a
    // new one. That is the harmless direction to be wrong in: a duplicate
    // comment is noise, whereas editing the wrong comment destroys someone's
    // words.
    let state_key = crate::state::key(&proposal.repo, proposal.number);
    let remembered_id = match store {
        Some(store) => store
            .load_state(&state_key)
            .await
            .ok()
            .flatten()
            .and_then(|state| state.hub_comment_id),
        None => None,
    };
    // State is a lookup hint, not authority to edit an arbitrary comment.
    // Re-discovery authenticates both the bot author and marker before any
    // update; a stale or corrupted stored id can therefore never redirect the
    // write onto somebody else's comment.
    let discovered = discover_review_hub(read, &repo, proposal.number).await?;
    let existing = match remembered_id {
        Some(id) if discovered == Some(id) => Some(id),
        Some(id) => {
            tracing::warn!(
                comment_id = id,
                "stored review-hub id was not authenticated; using discovery"
            );
            discovered
        }
        None => discovered,
    };

    let id = match existing {
        Some(id) => {
            write.update_comment(&repo, id, &body).await?;
            id
        }
        None => write.create_comment(&repo, proposal.number, &body).await?,
    };
    if let Some(store) = store
        && let Ok(Some(mut state)) = store.load_state(&state_key).await
    {
        state.hub_comment_id = Some(id);
        if let Err(err) = store.save_state(&state_key, &state).await {
            tracing::warn!(%err, "could not persist the review-hub comment id");
        }
    }
    Ok(())
}

async fn discover_review_hub(
    read: &dyn ForgeRead,
    repo: &RepoId,
    number: u64,
) -> Result<Option<u64>> {
    let current =
        crate::findings::prior::own_comment(read, repo, number, crate::summary::MARKER).await?;
    match current.and_then(|comment| comment.id) {
        Some(id) => Ok(Some(id)),
        None => Ok(crate::findings::prior::own_comment(
            read,
            repo,
            number,
            crate::summary::LEGACY_MARKER,
        )
        .await?
        .and_then(|comment| comment.id)),
    }
}

/// Post or update the wireframe gallery comment.
///
/// One comment per pull request, found by its marker and edited in place —
/// the same discipline as [`publish_review_hub`], and for the same reason: a
/// fresh gallery per push turns the pull request into a scroll of wireframes
/// that stopped being the current ones several pushes ago.
///
/// `Ok(())` with nothing written when the proposal carries no set (the
/// feature was off, or predates the field), or when the set has no screens.
async fn publish_wireframe(
    read: &dyn ForgeRead,
    write: &dyn ForgeWrite,
    proposal: &Proposal,
) -> Result<()> {
    let Some(set) = &proposal.wireframe else {
        return Ok(());
    };
    let Some(body) = crate::wireframe::render::comment(set) else {
        return Ok(());
    };
    let repo = RepoId::parse(&proposal.repo)
        .ok_or_else(|| Error::Forge(format!("`{}` is not owner/name", proposal.repo)))?;
    let existing = crate::findings::prior::own_comment(
        read,
        &repo,
        proposal.number,
        crate::wireframe::render::MARKER,
    )
    .await?
    .and_then(|comment| comment.id);
    match existing {
        Some(id) => write.update_comment(&repo, id, &body).await?,
        None => {
            write.create_comment(&repo, proposal.number, &body).await?;
        }
    }
    Ok(())
}

/// Whether this proposal draws a Changes Requested review.
///
/// The one predicate both the submitted review and the umbrella
/// `tinysweeper/review` check read, so the two cannot disagree.
///
/// Blocking needs BOTH a failing lane and a finding severe enough to justify
/// it. The lane conclusion alone is not enough: `fail_on` and
/// `request_changes_at` are independent knobs, so a lane configured to fail
/// on medium must still be able to fail a check without also blocking the
/// merge when the merge gate is set to high. Reading only the conclusion
/// made `request_changes_at` inert.
///
/// The severity is read from the lane's findings rather than the surviving
/// comments, so a recurred problem whose comment was deduped away still
/// blocks — being already visible is not being fixed.
pub fn requests_changes(config: &Config, proposal: &Proposal) -> bool {
    match config.request_changes_at() {
        Some(threshold) => proposal.blocked() && proposal.has_severity_at_or_above(threshold),
        None => false,
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
    if requests_changes(config, proposal) {
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

/// Every lane must carry the pre-dedupe identities for this comparison to be
/// trusted. Older proposal files deserialize them as `None` and remain noisy.
fn blocking_finding_set(proposal: &Proposal) -> Option<Vec<String>> {
    let mut identities = std::collections::BTreeSet::new();
    for lane in &proposal.lanes {
        identities.extend(lane.blocking_findings.as_ref()?.iter().cloned());
    }
    Some(identities.into_iter().collect())
}

/// Record a successful publication without replacing the incremental replay
/// state prepared by the read half.
async fn record_apply_state(
    store: Option<&dyn ReviewStateStore>,
    proposal: &Proposal,
    publication: Option<(
        ReviewEvent,
        Vec<String>,
        Option<crate::forge::types::OwnReview>,
        bool,
    )>,
) {
    let Some(store) = store else { return };
    let key = crate::state::key(&proposal.repo, proposal.number);
    let mut state = match store.load_state(&key).await {
        Ok(Some(state)) => state,
        Ok(None) => crate::state::ReviewedState {
            head_sha: proposal.head_sha.clone(),
            ..crate::state::ReviewedState::default()
        },
        Err(err) => {
            tracing::warn!(%err, "could not load review state to record the published decision");
            return;
        }
    };
    state.latest_usage = Some(crate::state::ReviewUsage {
        cost_usd: proposal.usage().cost_usd.to_string(),
        input_tokens: proposal.usage().input_tokens,
        output_tokens: proposal.usage().output_tokens,
        cached_tokens: proposal.usage().cached_tokens,
        embed_tokens: proposal.usage().embed_tokens,
        models: proposal.models.clone(),
    });
    if let Some((event, blocking_findings, standing_review, coverage_complete)) = publication {
        state.published_review = Some(crate::state::PublishedReview {
            event,
            head_sha: proposal.head_sha.clone(),
            blocking_findings,
            standing_review,
            coverage_complete: Some(coverage_complete),
        });
    }
    if let Err(err) = store.save_state(&key, &state).await {
        tracing::warn!(%err, "could not record the published review decision");
    }
}

/// Record the forge state that should still stand after the successful review.
/// A later COMMENT does not retire a prior approval or changes request, and a
/// failed approval withdrawal must remain visible in the next comparison.
fn standing_review_after_success(
    event: ReviewEvent,
    proposal: &Proposal,
    previous: Option<&crate::forge::types::OwnReview>,
    previous_known: bool,
    unvouched: bool,
    withdrawal_failed: bool,
) -> Option<crate::forge::types::OwnReview> {
    if event == ReviewEvent::Comment {
        if !previous_known {
            return None;
        }
        if let Some(previous) = previous
            && (previous.event == ReviewEvent::RequestChanges
                || (previous.event == ReviewEvent::Approve && (!unvouched || withdrawal_failed)))
        {
            return Some(previous.clone());
        }
    }
    Some(crate::forge::types::OwnReview {
        event,
        head_sha: Some(proposal.head_sha.clone()),
    })
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
) -> (Option<OwnReview>, bool) {
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
                summary: crate::findings::render::settled_lane_summary(
                    &settled.summary,
                    &[],
                    VERSION,
                    true,
                ),
                status: None,
                external_id: None,
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

/// Build a short check title from deterministic counts, never model prose.
fn title_for(findings: usize, coverage: Option<&crate::lanes::FileCoverage>) -> String {
    let finding_title = match findings {
        1 => "1 finding".to_string(),
        count => format!("{count} findings"),
    };
    if let Some(coverage) = coverage {
        let reviewed = coverage.reviewed.len();
        let total = reviewed.saturating_add(coverage.unanswered.len());
        return format!("{finding_title} · {reviewed}/{total} files");
    }
    finding_title
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
    if lane.noted.iter().any(|finding| !finding.grouped) {
        if lane.context_degraded {
            out.push_str(
                "\n**Unverified findings** — repository context was unavailable; these notes are not comments and do not affect the check verdict:\n\n",
            );
        } else {
            out.push_str(
                "\n**Worth a look** — below the posting gate, so not a comment and not a block:\n\n",
            );
        }
        for finding in lane.noted.iter().filter(|finding| !finding.grouped) {
            out.push_str(&format!(
                "- {} — {} {}\n",
                crate::markdown::code_span(&format!(
                    "{}{}",
                    finding.path,
                    finding.line.map(|l| format!(":{l}")).unwrap_or_default()
                )),
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
fn fingerprints(body: &str) -> Vec<String> {
    crate::findings::prior::fingerprints_in(body)
}

/// Alias identities immediately precede the authoritative final fingerprint.
/// Keeping the markers adjacent lets the reader reject marker-shaped text a
/// model may have placed in the finding body.
fn alias_marker(finding: &crate::findings::types::Finding) -> String {
    let aliases: Vec<&str> = finding
        .aliases
        .iter()
        .map(String::as_str)
        .filter(|alias| {
            alias.len() == 16
                && alias
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        })
        .collect();
    if aliases.is_empty() {
        String::new()
    } else {
        format!("<!-- {MARKER_PREFIX}fps={} -->", aliases.join(","))
    }
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
            // Overflow counts toward the verdict's worst severity: a high
            // finding the budget moved to the hub is still what blocks.
            let worst = proposal
                .reported()
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
            let unanswered = proposal.unanswered().len();
            format!(
                "tinysweeper could not complete coverage ({unanswered} unanswered item(s)), so this is not an approval."
            )
        }
        ReviewEvent::Comment if blocking == 0 => "tinysweeper found nothing blocking.".to_string(),
        ReviewEvent::Comment => format!("tinysweeper: {blocking} lane(s) blocking."),
    };

    if !unanchored.is_empty() {
        body.push_str("\n\n### Findings not posted inline\n");
        for finding in unanchored {
            body.push_str(&format!(
                "\n- **{}** ({}): {}\n\n  {}\n",
                crate::markdown::prose(&finding.title),
                crate::markdown::code_span(&finding.path),
                crate::markdown::prose(&finding.rule),
                crate::markdown::prose(&finding.body)
            ));
        }
    }

    // Over-budget findings are named here as well as in the hub. The hub is
    // best-effort (it is skipped when summaries are disabled, and a failed
    // update is only logged), while the review body is always submitted, so a
    // blocking finding that did not fit the budget is never only in a place
    // that might not be written.
    let over: Vec<&crate::findings::types::Finding> = proposal.overflowed().collect();
    if !over.is_empty() {
        body.push_str(
            "\n\n### Over the comment budget\n\nNot posted inline; listed here so none is lost.\n",
        );
        for finding in over {
            body.push_str(&format!(
                "\n- **{}** {} ({})",
                finding.severity,
                crate::markdown::prose(&finding.title),
                crate::markdown::code_span(&format!(
                    "{}{}",
                    finding.path,
                    finding
                        .line
                        .map(|line| format!(":{line}"))
                        .unwrap_or_default()
                ))
            ));
        }
        body.push('\n');
    }

    // Detailed usage remains in the machine-readable marker and timing log.
    // A collapsed total is enough context for someone reading the review.
    body.push_str(&format!(
        "\n\n<details>\n<summary>Diagnostics</summary>\n\nTotal spend: ${:.4}\n\n</details>",
        proposal.usage().cost_usd
    ));
    body.push_str(&format!(
        "\n<!-- {MARKER_PREFIX}state v=1 sha={} usage=cost:{:.4},input:{},output:{},cached:{},embed:{} -->",
        proposal.head_sha,
        proposal.usage().cost_usd,
        proposal.usage().input_tokens,
        proposal.usage().output_tokens,
        proposal.usage().cached_tokens,
        proposal.usage().embed_tokens,
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
            // A suggestion block replaces exactly the lines the comment is
            // anchored to, so carrying one *changes the anchor*: it widens to
            // the span the replacement covers. Without a suggestion the comment
            // stays a single-line pin, which is what a reader wants — a
            // multi-line highlight for a one-sentence remark is noise.
            let (start, line) = finding.published_range()?;
            let start_line = (start < line).then_some(start);
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
                .map(|s| {
                    let label = if finding.aliases.is_empty() {
                        ""
                    } else {
                        "\n\n**Suggested change for the opening observation**"
                    };
                    format!("{label}\n\n```suggestion\n{}\n```", s.replacement)
                })
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
                    "{}  {}\n\n**{}**\n\n{}{}\n\n{} · {}<!-- {MARKER_PREFIX}fp={} -->",
                    crate::findings::render::priority_badge(finding.severity),
                    crate::findings::render::lane_confidence_badge(
                        finding.lane,
                        finding.confidence
                    ),
                    crate::findings::render::escape_emphasis(&finding.title),
                    crate::markdown::prose(&finding.body),
                    suggestion,
                    crate::findings::render::rule_line(&finding.rule),
                    alias_marker(finding),
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

/// Render inline comments for review-flow tests in the read-only half.
///
/// Keeping the test on the production renderer is what proves aliases survive
/// the complete render/load boundary rather than two isolated helper tests.
#[cfg(test)]
pub(crate) fn test_inline_comments(
    proposal: &Proposal,
    files: &[ChangedFile],
) -> Vec<ReviewComment> {
    inline_comments(proposal, files)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::review::LaneProposal;
    use crate::config::types::{LaneId, Severity};
    use crate::findings::types::Finding;
    use crate::forge::types::{ChangedFile, CheckConclusion, IssueComment, PullRequest};
    use crate::forge::{MockForge, MockState, Write};

    #[test]
    fn check_titles_are_count_based_and_never_truncate_model_prose() {
        assert_eq!(title_for(2, None), "2 findings");
        assert_eq!(title_for(1, None), "1 finding");
        assert_eq!(title_for(0, None), "0 findings");
        let title = title_for(usize::MAX, None);
        assert!(title.len() <= 80, "{} chars: {title}", title.len());
        assert!(!title.contains("summary"));
    }

    #[test]
    fn a_partial_lane_title_reports_file_coverage() {
        let coverage = crate::lanes::FileCoverage {
            reviewed: (0..8).map(|n| format!("src/{n}.rs")).collect(),
            unanswered: vec!["src/8.rs".into(), "src/9.rs".into()],
        };
        assert_eq!(title_for(2, Some(&coverage)), "2 findings · 8/10 files");
    }

    #[test]
    fn lane_titles_report_full_coverage_even_without_findings() {
        let coverage = crate::lanes::FileCoverage {
            reviewed: (0..10).map(|n| format!("src/{n}.rs")).collect(),
            unanswered: vec![],
        };
        let title = title_for(0, Some(&coverage));
        assert_eq!(title, "0 findings · 10/10 files");
        assert!(title.len() <= 80);
    }

    #[cfg(feature = "serve")]
    #[tokio::test]
    async fn applying_an_mcp_issue_plan_performs_exactly_its_write() {
        let forge = MockForge::new();
        let plan = McpIssuePlan {
            repo: RepoId::parse("acme/widget").unwrap(),
            installation: 7,
            title: "Parser can loop".into(),
            body: "Reproduction and code context.".into(),
            labels: vec!["bug".into()],
        };

        let number = execute_mcp_issue(&forge, &plan).await.expect("applies");

        assert_eq!(number, 1);
        assert_eq!(
            forge.writes(),
            vec![Write::IssueCreated {
                title: plan.title,
                body: plan.body,
                labels: plan.labels,
            }]
        );
    }

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
            summary: None,
            prior_findings: vec![],
            overview: None,
            wireframe: None,
            unreviewed: vec![],
            partial_review: None,
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
                completed: true,
                findings,
                blocking_findings: Some(vec![]),
                noted: Vec::new(),
                context_degraded: false,
                resolved: vec![],
                pending: vec![],
                skipped: None,
                deduped: 0,
                highest_severity,
                usage: Default::default(),
                models: vec![],
                unanswered: vec![],
                omissions: vec![],
                coverage: None,
                overflow: vec![],
            }],
            cost_usd: 0.01,
            input_tokens: 10_000,
            output_tokens: 400,
            cached_tokens: 800,
            embed_tokens: 0,
            models: vec!["moonshotai/kimi-k3".into()],
            completion_accounting: None,
            logical_usage: None,
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
                        summary:
                            "Coverage looks complete. [click](https://evil.example) @user <script> `hostile` | value"
                                .into(),
                        failed: false,
                        generation: "gen-1".into(),
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
        assert!(!check.summary.contains("[click](https://evil.example)"));
        assert!(!check.summary.contains("https://evil.example"));
        assert!(!check.summary.contains("@user"));
        assert!(!check.summary.contains("<script>"));
        assert!(!check.summary.contains("`hostile`"));

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
        let original_watch = Watch {
            head_sha: "abc123".into(),
            jobs: vec!["playwright".into()],
            summary: String::new(),
            failed: false,
            generation: "gen-1".into(),
        };
        store
            .save_state(
                &key,
                &ReviewedState {
                    head_sha: "abc123".into(),
                    e2e: Some(original_watch.clone()),
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
                generation: "gen-2".into(),
            }),
            ..ReviewedState::default()
        };
        store.save_state(&key, &concurrent).await.unwrap();

        // `settle_e2e` finishes settling the `abc123` watch it read earlier
        // and tries to clear it.
        let cleared = store.clear_e2e_watch(&key, &original_watch).await.unwrap();
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
    async fn clearing_the_watch_does_not_discard_a_same_head_re_review() {
        // The narrower case `head_sha`-only comparison missed: a manual
        // re-review of the *same* commit (the `/admin/reviews` route can
        // trigger one) saves a replacement watch with the same `head_sha`
        // but different `jobs`/`summary`/`failed`. `clear_e2e_watch` must
        // compare the whole watch, not just the head it is for.
        use crate::lanes::e2e::runs::Watch;
        use crate::state::memory::MemoryState;
        use crate::state::types::ReviewedState;

        let store = MemoryState::new();
        let key = crate::state::key("tinyhumansai/tinysweeper", 7);
        let stale_watch = Watch {
            head_sha: "abc123".into(),
            jobs: vec!["playwright".into()],
            summary: "First pass.".into(),
            failed: false,
            generation: "gen-1".into(),
        };
        store
            .save_state(
                &key,
                &ReviewedState {
                    head_sha: "abc123".into(),
                    e2e: Some(stale_watch.clone()),
                    ..ReviewedState::default()
                },
            )
            .await
            .unwrap();

        // A manual re-review of the same head lands a different watch —
        // same `head_sha`, different `jobs` (a second job was added to the
        // e2e-required config in the meantime, say) — before the first
        // settlement's clear runs.
        let re_reviewed = ReviewedState {
            head_sha: "abc123".into(),
            e2e: Some(Watch {
                head_sha: "abc123".into(),
                jobs: vec!["playwright".into(), "cypress".into()],
                summary: "Second pass.".into(),
                failed: false,
                generation: "gen-2".into(),
            }),
            ..ReviewedState::default()
        };
        store.save_state(&key, &re_reviewed).await.unwrap();

        let cleared = store.clear_e2e_watch(&key, &stale_watch).await.unwrap();
        assert!(
            !cleared,
            "the stored watch differs (jobs, summary) even though the head is the same"
        );

        let after = store.load_state(&key).await.unwrap().expect("still there");
        assert_eq!(
            after, re_reviewed,
            "the re-review's watch must survive intact"
        );
    }

    #[tokio::test]
    async fn a_byte_identical_re_review_still_gets_its_own_generation() {
        // The case content alone cannot distinguish: a same-head manual
        // re-review that changes nothing substantive (same jobs, same
        // summary, same verdict) still produces a *new* watch. Without
        // `generation`, that new watch would compare equal to the old one
        // `settle_e2e` is settling and would be cleared right out from under
        // it, even though nothing about the content differs to warn anyone.
        use crate::lanes::e2e::runs::Watch;
        use crate::state::memory::MemoryState;
        use crate::state::types::ReviewedState;

        let store = MemoryState::new();
        let key = crate::state::key("tinyhumansai/tinysweeper", 7);
        let old_watch = Watch {
            head_sha: "abc123".into(),
            jobs: vec!["playwright".into()],
            summary: "Coverage looks complete.".into(),
            failed: false,
            generation: "gen-1".into(),
        };
        store
            .save_state(
                &key,
                &ReviewedState {
                    head_sha: "abc123".into(),
                    e2e: Some(old_watch.clone()),
                    ..ReviewedState::default()
                },
            )
            .await
            .unwrap();

        // The re-review's watch: every field but `generation` is identical.
        let re_reviewed = ReviewedState {
            head_sha: "abc123".into(),
            e2e: Some(Watch {
                generation: "gen-2".into(),
                ..old_watch.clone()
            }),
            ..ReviewedState::default()
        };
        store.save_state(&key, &re_reviewed).await.unwrap();

        let cleared = store.clear_e2e_watch(&key, &old_watch).await.unwrap();
        assert!(
            !cleared,
            "the stored watch is `gen-2`, not the `gen-1` this call was settling, \
             even though every other field is identical"
        );

        let after = store.load_state(&key).await.unwrap().expect("still there");
        assert_eq!(
            after, re_reviewed,
            "the re-review's watch must survive intact"
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
                        generation: "gen-1".into(),
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

    #[test]
    fn an_over_budget_finding_is_named_in_the_review_body() {
        // The hub is best-effort and is skipped when summaries are disabled, so
        // an over-budget finding must also be named in the review body, which
        // is always submitted.
        let mut over = finding();
        over.title = "Over budget finding".into();
        let mut proposal = proposal("abc123", vec![]);
        proposal.lanes[0].overflow.push(over);

        let body = review_body(&proposal, ReviewEvent::Comment, None, &[]);

        assert!(body.contains("### Over the comment budget"), "{body}");
        assert!(body.contains("Over budget finding"), "{body}");
        assert!(body.contains("`src/main.rs:2`"), "{body}");
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
            aliases: vec![],
            grouped: false,
            review_pass: 1,
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
        let mut proposal = proposal("abc123", vec![]);
        proposal.lanes[0].conclusion = CheckConclusion::Neutral;
        proposal.lanes[0].summary = "Reviewed 8 of 10 files — 2 not reviewed.".into();
        proposal.lanes[0].unanswered = vec!["a.rs".into(), "b.rs".into()];
        proposal.lanes[0].coverage = Some(crate::lanes::FileCoverage {
            reviewed: (0..8).map(|n| format!("src/{n}.rs")).collect(),
            unanswered: vec!["a.rs".into(), "b.rs".into()],
        });
        apply(&forge, &forge, &config(), &proposal, None)
            .await
            .expect("applies");

        let checks = forge.checks();
        assert_eq!(
            checks["tinysweeper/critique"].title,
            "0 findings · 8/10 files"
        );
        assert_eq!(
            checks["tinysweeper/critique"].conclusion,
            Some(CheckConclusion::Neutral)
        );
    }

    #[tokio::test]
    async fn a_closed_or_merged_pull_request_is_not_published_after_review() {
        for (open, merged) in [(false, false), (false, true)] {
            let forge = forge("abc123");
            forge.replace_pull_request(PullRequest {
                number: 7,
                head_sha: "abc123".into(),
                open,
                merged,
                ..PullRequest::default()
            });

            let disposition = apply(&forge, &forge, &config(), &proposal("abc123", vec![]), None)
                .await
                .expect("closed pull requests are declined without publishing");

            assert_eq!(disposition, ApplyDisposition::Closed);

            assert!(
                forge.writes().is_empty(),
                "closed state open={open} merged={merged} must not publish a review"
            );
        }
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
        let disposition = apply(&forge, &forge, &config(), &proposal("abc123", vec![]), None)
            .await
            .expect("returns cleanly");

        assert_eq!(disposition, ApplyDisposition::Superseded);
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
    async fn a_grouped_comment_publishes_and_records_every_identity() {
        let forge = forge("abc123");
        let store = crate::state::memory::MemoryState::default();
        let key = crate::state::key("tinyhumansai/tinysweeper", 7);
        store
            .save_state(
                &key,
                &crate::state::types::ReviewedState {
                    head_sha: "abc123".into(),
                    ..crate::state::types::ReviewedState::default()
                },
            )
            .await
            .unwrap();
        let mut grouped = finding();
        grouped.identity = Some("0123456789abcdef".into());
        grouped.aliases = vec!["1111111111111111".into(), "2222222222222222".into()];

        apply(
            &forge,
            &forge,
            &config(),
            &proposal("abc123", vec![grouped]),
            Some(&store),
        )
        .await
        .expect("applies");

        let body = forge
            .writes()
            .into_iter()
            .find_map(|write| match write {
                Write::Review { comments, .. } => comments.into_iter().next().map(|c| c.body),
                _ => None,
            })
            .expect("an inline comment");
        assert_eq!(
            fingerprints(&body),
            vec!["0123456789abcdef", "1111111111111111", "2222222222222222"]
        );
        let recorded = store.load_state(&key).await.unwrap().unwrap();
        for identity in fingerprints(&body) {
            assert!(
                recorded.fingerprints.contains(&identity),
                "missing {identity}"
            );
        }
    }

    #[test]
    fn requests_changes_agrees_with_the_submitted_verdict() {
        // `tinysweeper/review` is concluded from this predicate. When it and
        // the review disagreed, the check showed a pass beside a Changes
        // Requested review.
        let config = config();
        let blocking = proposal("abc123", vec![finding()]);
        assert!(requests_changes(&config, &blocking));
        assert_eq!(
            review_event(&config, &blocking, None, false),
            ReviewEvent::RequestChanges
        );

        let clean = proposal("abc123", vec![]);
        assert!(!requests_changes(&config, &clean));
        assert_ne!(
            review_event(&config, &clean, None, false),
            ReviewEvent::RequestChanges
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
    async fn mock_forge_finding_rows_keep_backslash_pipe_paths_in_one_cell() {
        let forge = forge("abc123");
        let findings = (1..=3)
            .map(|backslashes| {
                let mut item = finding();
                item.path = format!(
                    "src/a{}|b.rs [click](https://evil.example)",
                    "\\".repeat(backslashes)
                );
                item.title = format!("Path {backslashes}");
                item.line = None;
                item
            })
            .collect();
        apply(
            &forge,
            &forge,
            &config(),
            &proposal("abc123", findings),
            None,
        )
        .await
        .expect("applies");

        let summary = forge
            .checks()
            .remove("tinysweeper/critique")
            .expect("critique check")
            .summary;
        for count in 1..=3 {
            let row = summary
                .lines()
                .find(|line| line.contains(&format!("Path {count}")))
                .expect("finding row");
            assert_eq!(row.split(" | ").count(), 3, "{row}");
            assert!(row.contains(&format!("{}\\|", "\\".repeat(count))), "{row}");
            assert!(row.contains("[click](https://evil.example)"), "{row}");
        }
    }

    #[tokio::test]
    async fn mock_forge_review_body_keeps_adversarial_text_inert() {
        let forge = forge("abc123");
        let mut hostile = finding();
        hostile.line = None;
        hostile.title = "@maintainer <img src=x> [click](https://evil.example)".into();
        hostile.body = "<script>alert(1)</script> @user [link](https://evil.example) https://a.b/[click](https://evil.example) @maintainer `code` **bold** |\\path".into();

        apply(
            &forge,
            &forge,
            &config(),
            &proposal("abc123", vec![hostile]),
            None,
        )
        .await
        .expect("applies");

        let (body, _) = review_of(&forge).expect("review published to the mock forge");
        assert!(!body.contains("@maintainer"), "{body}");
        assert!(!body.contains("<img src=x>"), "{body}");
        assert!(!body.contains("[click](https://evil.example)"), "{body}");
        assert!(!body.contains("<script>"), "{body}");
        assert!(!body.contains("[link](https://evil.example)"), "{body}");
        assert!(!body.contains("https://evil.example"), "{body}");
        assert!(!body.contains("https://a.b"), "{body}");
        assert!(!body.contains("[click]("), "{body}");
        assert!(!body.contains("@maintainer"), "{body}");
        assert!(!body.contains("`code`"), "{body}");
        assert!(!body.contains("**bold**"), "{body}");
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
        f.aliases.push("1111111111111111".into());
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
        assert!(
            comment
                .body
                .contains("Suggested change for the opening observation"),
            "a shared thread must say which rationale its commit button addresses: {}",
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
            body.contains("not an approval") && body.contains("1 unanswered item"),
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
        assert_eq!(crate::markdown::code_span("src/lib.rs"), "`src/lib.rs`");
        let hostile = "x`.rs` **bold**\n# heading";
        let rendered = crate::markdown::code_span(hostile);
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
        // Re-reviewing the same commit must not add another timeline entry.
        let forge = forge("abc123").with_own_review(7, ReviewEvent::Approve);
        apply(&forge, &forge, &config(), &proposal("abc123", vec![]), None)
            .await
            .expect("applies");

        assert!(review_of(&forge).is_none(), "{:#?}", forge.writes());
    }

    #[tokio::test]
    async fn a_clean_changed_head_receives_a_fresh_approval() {
        let forge = forge("previous-head").with_own_review(7, ReviewEvent::Approve);
        forge.push(7, "abc123", vec![]);
        apply(&forge, &forge, &config(), &proposal("abc123", vec![]), None)
            .await
            .expect("applies");
        let (_, event) = review_of(&forge).expect("the changed head gets its own approval");
        assert_eq!(event, ReviewEvent::Approve);
    }

    #[tokio::test]
    async fn a_clean_head_is_approved_once_even_across_repeated_reviews_and_pushes() {
        let forge = forge("abc123");
        for head in ["abc123", "next-head"] {
            forge.push(7, head, vec![]);
            for _ in 0..2 {
                apply(&forge, &forge, &config(), &proposal(head, vec![]), None)
                    .await
                    .expect("applies");
            }
        }
        let approvals = forge
            .writes()
            .into_iter()
            .filter(|write| {
                matches!(
                    write,
                    Write::Review {
                        event: ReviewEvent::Approve,
                        ..
                    }
                )
            })
            .count();
        assert_eq!(approvals, 2, "one real approval per reviewed head");
    }

    #[tokio::test]
    async fn an_approval_without_a_known_reviewed_head_is_refreshed() {
        let mut state = MockState::default();
        state.pull_requests.insert(
            7,
            PullRequest {
                number: 7,
                head_sha: "abc123".into(),
                ..PullRequest::default()
            },
        );
        state.own_reviews.insert(
            7,
            OwnReview {
                event: ReviewEvent::Approve,
                head_sha: None,
            },
        );
        let forge = MockForge::with_state(state);
        apply(&forge, &forge, &config(), &proposal("abc123", vec![]), None)
            .await
            .expect("applies");
        let (_, event) = review_of(&forge).expect("unknown history cannot certify this head");
        assert_eq!(event, ReviewEvent::Approve);
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

        assert!(body.contains("Total spend: $0.0100"), "{body}");
        assert_eq!(body.matches("Total spend:").count(), 1, "{body}");
        assert!(!body.contains("10,000 in"), "{body}");
        assert!(!body.contains("400 out"), "{body}");
        assert!(!body.contains("800 cached (8%)"), "{body}");
        assert!(!body.contains("kimi-k3"), "{body}");
        assert!(body.contains("state v=1 sha=abc123"), "{body}");
        assert!(
            body.contains("usage=cost:0.0100,input:10000,output:400,cached:800,embed:0"),
            "{body}"
        );
    }

    #[tokio::test]
    async fn unchanged_verdict_and_blocking_findings_skip_review_but_update_hub() {
        use crate::state::memory::MemoryState;

        let forge = forge("abc123");
        let store = MemoryState::new();
        for run in 0..2 {
            let mut proposal = proposal("abc123", vec![finding()]);
            if run == 1 {
                let mut informational = finding();
                informational.severity = Severity::Low;
                informational.rule = "informational-note".into();
                informational.title = "A nonblocking note appeared".into();
                proposal.lanes[0].findings.push(informational);
            }
            apply(&forge, &forge, &config(), &proposal, Some(&store))
                .await
                .expect("applies");
        }
        let writes = forge.writes();
        assert_eq!(
            writes
                .iter()
                .filter(|w| matches!(w, Write::Review { .. }))
                .count(),
            1
        );
        assert_eq!(
            writes
                .iter()
                .filter(|w| matches!(w, Write::Comment { .. } | Write::CommentUpdate { .. }))
                .count(),
            2
        );
        assert_eq!(
            writes
                .iter()
                .filter(|w| matches!(w, Write::Check(_) | Write::CheckUpdate { .. }))
                .count(),
            2
        );
    }

    #[tokio::test]
    async fn repeated_incomplete_comments_dedupe_while_a_previous_block_stands() {
        use crate::state::memory::MemoryState;

        let forge = forge("head-a");
        let store = MemoryState::new();
        apply(
            &forge,
            &forge,
            &config(),
            &proposal("head-a", vec![finding()]),
            Some(&store),
        )
        .await
        .expect("publishes initial block");

        for head in ["head-b", "head-c"] {
            forge.push(7, head, vec![]);
            let mut partial = proposal(head, vec![]);
            partial.unreviewed.push("src/withheld.rs".into());
            partial.lanes[0].conclusion = CheckConclusion::Neutral;
            partial.lanes[0].unanswered.push("src/withheld.rs".into());
            apply(&forge, &forge, &config(), &partial, Some(&store))
                .await
                .expect("publishes incomplete comment");
        }

        let reviews: Vec<_> = forge
            .writes()
            .into_iter()
            .filter_map(|write| match write {
                Write::Review { event, .. } => Some(event),
                _ => None,
            })
            .collect();
        assert_eq!(
            reviews,
            vec![ReviewEvent::RequestChanges, ReviewEvent::Comment],
            "the same incomplete comment must dedupe while the old block remains"
        );
        let standing = forge
            .own_review_state(&RepoId::parse("tinyhumansai/tinysweeper").unwrap(), 7)
            .await
            .expect("reads standing review")
            .expect("the request changes still stands");
        assert_eq!(standing.event, ReviewEvent::RequestChanges);
        assert_eq!(standing.head_sha.as_deref(), Some("head-a"));
    }

    #[tokio::test]
    async fn a_failed_review_write_does_not_claim_a_publication_receipt() {
        use crate::state::memory::MemoryState;

        let forge = forge("abc123");
        forge.fail_next_review_publishes(1);
        let store = MemoryState::new();
        let result = apply(
            &forge,
            &forge,
            &config(),
            &proposal("abc123", vec![finding()]),
            Some(&store),
        )
        .await;
        assert!(
            result.is_err(),
            "the failed review must remain an apply error"
        );
        assert!(
            store
                .load_state(&crate::state::key("tinyhumansai/tinysweeper", 7))
                .await
                .expect("state store works")
                .is_none(),
            "a failed write is not a durable publication"
        );
        assert!(
            !forge
                .writes()
                .iter()
                .any(|write| matches!(write, Write::Review { .. }))
        );
    }

    #[tokio::test]
    async fn an_unknown_live_approval_head_cannot_be_certified_by_a_receipt() {
        use crate::state::{PublishedReview, ReviewedState, memory::MemoryState};

        let mut mock_state = MockState::default();
        mock_state.pull_requests.insert(
            7,
            PullRequest {
                number: 7,
                head_sha: "new-head".into(),
                ..PullRequest::default()
            },
        );
        mock_state.own_reviews.insert(
            7,
            crate::forge::types::OwnReview {
                event: ReviewEvent::Approve,
                head_sha: None,
            },
        );
        let forge = MockForge::with_state(mock_state);
        let store = MemoryState::new();
        let key = crate::state::key("tinyhumansai/tinysweeper", 7);
        store
            .save_state(
                &key,
                &ReviewedState {
                    published_review: Some(PublishedReview {
                        event: ReviewEvent::Approve,
                        head_sha: "old-head".into(),
                        blocking_findings: vec![],
                        standing_review: None,
                        coverage_complete: Some(true),
                    }),
                    ..ReviewedState::default()
                },
            )
            .await
            .expect("seeds publication receipt");

        // The mock's live approval has no commit metadata; the durable receipt
        // for an older commit cannot fill that gap.
        apply(
            &forge,
            &forge,
            &config(),
            &proposal("new-head", vec![]),
            Some(&store),
        )
        .await
        .expect("applies");
        assert_eq!(
            forge
                .writes()
                .iter()
                .filter(|write| matches!(write, Write::Review { .. }))
                .count(),
            1,
            "unknown live head metadata must cause a fresh approval"
        );
    }

    #[tokio::test]
    async fn a_deduped_push_updates_hidden_usage_without_rewriting_the_review_receipt() {
        use crate::state::memory::MemoryState;

        let forge = forge("initial");
        let store = MemoryState::new();
        for (head, cost, input) in [("head-one", 0.01, 100), ("head-two", 0.025, 250)] {
            forge.push(
                7,
                head,
                vec![ChangedFile {
                    path: "src/main.rs".into(),
                    patch: Some("@@ -1,1 +1,2 @@\n x\n+y\n".into()),
                    ..ChangedFile::default()
                }],
            );
            let mut proposal = proposal(head, vec![finding()]);
            proposal.cost_usd = cost;
            proposal.input_tokens = input;
            proposal.output_tokens = 40;
            proposal.cached_tokens = 25;
            apply(&forge, &forge, &config(), &proposal, Some(&store))
                .await
                .expect("applies");
        }

        let reviews: Vec<_> = forge
            .writes()
            .into_iter()
            .filter_map(|write| match write {
                Write::Review { body, .. } => Some(body),
                _ => None,
            })
            .collect();
        assert_eq!(
            reviews.len(),
            1,
            "the second head repeats the same decision"
        );
        assert!(reviews[0].contains("sha=head-one"), "{}", reviews[0]);
        assert!(reviews[0].contains("usage=cost:0.0100,input:100,output:40,cached:25,embed:0"));

        let state = store
            .load_state(&crate::state::key("tinyhumansai/tinysweeper", 7))
            .await
            .expect("loads")
            .expect("state retained");
        assert_eq!(
            state
                .latest_usage
                .as_ref()
                .map(|usage| usage.cost_usd.as_str()),
            Some("0.025")
        );
        assert_eq!(
            state.latest_usage.as_ref().map(|usage| usage.input_tokens),
            Some(250)
        );
        assert_eq!(
            state
                .published_review
                .as_ref()
                .map(|review| review.head_sha.as_str()),
            Some("head-one")
        );
    }

    #[tokio::test]
    async fn a_changed_verdict_still_posts_a_review() {
        use crate::state::memory::MemoryState;

        let forge = forge("abc123");
        let store = MemoryState::new();
        let mut config = config();
        apply(
            &forge,
            &forge,
            &config,
            &proposal("abc123", vec![finding()]),
            Some(&store),
        )
        .await
        .expect("first apply");
        config.review.request_changes_at = "off".into();
        apply(
            &forge,
            &forge,
            &config,
            &proposal("abc123", vec![finding()]),
            Some(&store),
        )
        .await
        .expect("second apply");
        assert_eq!(
            forge
                .writes()
                .iter()
                .filter(|w| matches!(w, Write::Review { .. }))
                .count(),
            2
        );
    }

    #[tokio::test]
    async fn a_changed_blocking_identity_posts_a_new_request_changes_review() {
        use crate::state::memory::MemoryState;

        let forge = forge("head-one");
        let store = MemoryState::new();
        let first = proposal("head-one", vec![finding()]);
        apply(&forge, &forge, &config(), &first, Some(&store))
            .await
            .expect("publishes first blocker");

        forge.push(7, "head-two", vec![]);
        let mut second = proposal("head-two", vec![finding()]);
        second.lanes[0].blocking_findings = Some(vec!["different-blocker".into()]);
        apply(&forge, &forge, &config(), &second, Some(&store))
            .await
            .expect("publishes changed blocker");

        assert_eq!(
            forge
                .writes()
                .iter()
                .filter(|write| matches!(
                    write,
                    Write::Review {
                        event: ReviewEvent::RequestChanges,
                        ..
                    }
                ))
                .count(),
            2,
            "the blocker identity is part of the decision signature"
        );
    }

    #[tokio::test]
    async fn recording_a_decision_does_not_replace_state_after_a_load_error() {
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };

        struct UnavailableState(Arc<AtomicUsize>);

        #[async_trait::async_trait]
        impl crate::ports::ReviewStateStore for UnavailableState {
            async fn load_state(
                &self,
                _key: &str,
            ) -> crate::error::Result<Option<crate::state::ReviewedState>> {
                Err(crate::error::Error::Forge("state store unavailable".into()))
            }

            async fn save_state(
                &self,
                _key: &str,
                _state: &crate::state::ReviewedState,
            ) -> crate::error::Result<()> {
                self.0.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }

            async fn clear_e2e_watch(
                &self,
                _key: &str,
                _watch: &crate::lanes::e2e::runs::Watch,
            ) -> crate::error::Result<bool> {
                Ok(false)
            }
        }

        let saves = Arc::new(AtomicUsize::new(0));
        let store = UnavailableState(saves.clone());
        record_apply_state(
            Some(&store),
            &proposal("head", vec![]),
            Some((ReviewEvent::Approve, vec![], None, true)),
        )
        .await;
        assert_eq!(saves.load(Ordering::SeqCst), 0);
    }

    // --- the change map ----------------------------------------------------

    /// A proposal carrying enough changed behaviours to warrant a flow map.
    fn proposal_with_map(head: &str) -> Proposal {
        use crate::evidence::diff::parse_file_patch;
        use crate::index::types::{EdgeKind, GraphEdge, GraphNode, Neighbourhood};

        let diffs = [
            parse_file_patch(
                "src/lanes/critique.rs",
                "@@ -1,1 +1,2 @@ fn review() {\n x\n+y\n",
            ),
            parse_file_patch("src/app/run.rs", "@@ -1,1 +1,2 @@ fn run() {\n x\n+y\n"),
            parse_file_patch("src/app/parse.rs", "@@ -1,1 +1,2 @@ fn parse() {\n x\n+y\n"),
            parse_file_patch(
                "src/app/dispatch.rs",
                "@@ -1,1 +1,2 @@ fn dispatch() {\n x\n+y\n",
            ),
        ];
        let walk = Neighbourhood {
            nodes: vec![
                GraphNode::symbol("o/r", "src/lanes/critique.rs", "review"),
                GraphNode::symbol("o/r", "src/app/run.rs", "run"),
                GraphNode::symbol("o/r", "src/app/parse.rs", "parse"),
                GraphNode::symbol("o/r", "src/app/dispatch.rs", "dispatch"),
            ],
            edges: vec![
                GraphEdge::new(
                    "o/r",
                    "src/app/run.rs#run",
                    "src/lanes/critique.rs#review",
                    EdgeKind::Calls,
                    "src/app/run.rs",
                ),
                GraphEdge::new(
                    "o/r",
                    "src/app/parse.rs#parse",
                    "src/app/run.rs#run",
                    EdgeKind::Calls,
                    "src/app/parse.rs",
                ),
                GraphEdge::new(
                    "o/r",
                    "src/app/dispatch.rs#dispatch",
                    "src/app/parse.rs#parse",
                    EdgeKind::Calls,
                    "src/app/dispatch.rs",
                ),
            ],
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
                    body.contains(crate::summary::MARKER)
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
    async fn mock_forge_publishes_the_compact_hub_golden() {
        let forge = forge("abc123");
        let mut proposal = proposal("abc123", vec![]);
        proposal.summary = Some(crate::summary::ReviewSummary {
            executive_summary: "The change adds a request guard.".into(),
            file_coverage: Some(crate::summary::FileReviewCoverage {
                reviewed: 2,
                total: 3,
                unreviewed: vec!["README.md".into()],
            }),
            ..crate::summary::ReviewSummary::default()
        });
        apply(&forge, &forge, &config(), &proposal, None)
            .await
            .expect("applies");

        let posted = overview_comments(&forge);
        let body = match &posted[0] {
            Write::Comment { body, .. } => body.clone(),
            write => panic!("expected new hub comment, got {write:?}"),
        };
        let public = body.split("<details").next().expect("public hub");
        assert_eq!(
            public,
            format!(
                "{}\n\n# Tiny Sweeper review\n\n**Reviewed head:** `abc123`\n\n**Verdict:** Ready for maintainer review · **Priority:** none\n**Coverage:** Reviewed 2 of 3 files; not reviewed: {}.  \nThe change adds a request guard.\n\n",
                crate::summary::MARKER,
                crate::markdown::code_span("README.md")
            )
        );
        assert!(public.len() <= 800, "{} chars", public.len());
    }

    #[tokio::test]
    async fn mock_forge_marks_degraded_context_notes_as_unverified() {
        let forge = forge("abc123");
        let mut proposal = proposal("abc123", vec![]);
        proposal.lanes[0].context_degraded = true;
        proposal.lanes[0].noted.push(finding());

        apply(&forge, &forge, &config(), &proposal, None)
            .await
            .expect("applies");

        let posted = overview_comments(&forge);
        let body = match &posted[0] {
            Write::Comment { body, .. } => body,
            write => panic!("expected new hub comment, got {write:?}"),
        };
        assert!(
            body.contains("Unverified notes — repository context was unavailable; these are not actionable findings"),
            "{body}"
        );
        assert!(body.contains("Unverified note — critique"), "{body}");
        assert!(body.contains("[`src/main.rs:2`]"), "{body}");
        assert!(body.contains("**Problem:**"), "{body}");
        assert!(body.contains("**Suggested fix:**"), "{body}");
    }

    #[tokio::test]
    async fn mock_forge_publishes_only_citations_on_supplied_head_lines() {
        use crate::harness::mock::MockModel;

        let forge = forge("abc123");
        let config = config();
        let model = MockModel::new().then(serde_json::json!({
            "executive_summary": "The change adds a parser guard.",
            "changes": "The parser now checks its boundary.",
            "features": [
                {
                    "kind": "addition",
                    "name": "Unsupported line",
                    "impact": "This line is outside the supplied diff.",
                    "citations": ["src/main.rs:999999"]
                },
                {
                    "kind": "addition",
                    "name": "Supported line",
                    "impact": "This line was added in the reviewed diff.",
                    "citations": ["src/main.rs:2"]
                }
            ],
            "tests": [],
            "positive_observations": {}
        }));
        let diffs = [parse_file_patch(
            "src/main.rs",
            "@@ -1,1 +1,2 @@\n fn main() {}\n+let i = 0;\n",
        )];
        let pull_request = PullRequest {
            number: 7,
            head_sha: "abc123".into(),
            ..PullRequest::default()
        };
        let (summary, _, _) =
            crate::summary::generate(&model, &config, &pull_request, &diffs, &[], None, &[]).await;
        let mut proposal = proposal("abc123", vec![]);
        proposal.summary = Some(summary);

        apply(&forge, &forge, &config, &proposal, None)
            .await
            .expect("applies");

        let posted = overview_comments(&forge);
        assert_eq!(posted.len(), 1, "{posted:#?}");
        let Write::Comment { body, .. } = &posted[0] else {
            panic!("hub comment was not posted: {posted:#?}");
        };
        assert!(body.contains("Supported line"), "{body}");
        assert!(body.contains("blob/abc123/src/main.rs#L2"), "{body}");
        assert!(!body.contains("Unsupported line"), "{body}");
        assert!(!body.contains("#L999999"), "{body}");
    }

    #[tokio::test]
    async fn mock_forge_render_drops_false_success_claim_for_no_response_lane() {
        use crate::harness::mock::MockModel;

        let forge = forge("abc123");
        let config = config();
        let model = MockModel::new().then(serde_json::json!({
            "executive_summary": "The security lane gives this change a greenlight. Security found no issues. Security is clean. The end-to-end lane confirms all checks are complete. The transaction commits each event once. The transaction commit no longer loses events. This adds a test fixture with no new dependencies. The test fixture maps the empty-state case. The commit reviewer found no issues. The parser now rejects unknown paths.",
            "changes": "All checks report no issues. The parser handles unknown paths safely.",
            "features": [{
                "kind": "addition",
                "name": "Atomic commit mapping",
                "impact": "Transaction writes preserve each event.",
                "citations": ["src/transaction.rs"]
            }],
            "tests": [{
                "kind": "unit test fixture",
                "behavior": "The test fixture maps transaction rollback.",
                "assessment": "The fixture covers the empty-state case.",
                "citations": ["src/transaction.rs"]
            }],
            "positive_observations": {}
        }));
        let lanes: Vec<_> = [
            LaneId::Tests,
            LaneId::Security,
            LaneId::E2e,
            LaneId::Commits,
        ]
        .into_iter()
        .map(|lane| {
            let outcome =
                crate::lanes::LaneOutcome::unanswered(lane, crate::ports::model::Spend::default());
            LaneProposal {
                lane,
                check_name: lane.check_name(),
                conclusion: CheckConclusion::Neutral,
                summary: outcome.summary,
                completed: outcome.completed,
                context_degraded: false,
                findings: vec![],
                blocking_findings: Some(vec![]),
                noted: vec![],
                resolved: vec![],
                pending: vec![],
                skipped: outcome.skipped,
                deduped: 0,
                highest_severity: None,
                usage: Default::default(),
                models: vec![],
                unanswered: outcome.unanswered,
                omissions: outcome.omissions,
                coverage: outcome.coverage,
                overflow: vec![],
            }
        })
        .collect();
        let diffs = [crate::evidence::diff::parse_file_patch(
            "src/transaction.rs",
            "@@ -1,1 +1,2 @@ fn persist()\n old\n+new\n",
        )];
        let (summary, _, _) = crate::summary::generate(
            &model,
            &config,
            &PullRequest {
                number: 7,
                title: "Enforce speech privacy".into(),
                body: "Keep private speech out of logs.".into(),
                head_sha: "abc123".into(),
                ..PullRequest::default()
            },
            &diffs,
            &lanes,
            None,
            &[],
        )
        .await;
        let mut proposal = proposal("abc123", vec![]);
        proposal.lanes = lanes;
        proposal.summary = Some(summary);

        apply(&forge, &forge, &config, &proposal, None)
            .await
            .expect("applies");

        let body = overview_comments(&forge)
            .into_iter()
            .find_map(|write| match write {
                Write::Comment { body, .. } => Some(body),
                _ => None,
            })
            .expect("hub comment");
        let opening = body
            .split("# Tiny Sweeper review\n\n")
            .nth(1)
            .and_then(|text| text.lines().next())
            .expect("executive opening");
        assert!(!body.to_ascii_lowercase().contains("security lane gives"));
        assert!(!body.to_ascii_lowercase().contains("greenlight"));
        assert!(
            !body
                .to_ascii_lowercase()
                .contains("end-to-end lane confirms")
        );
        assert!(!body.contains("Security found no issues"));
        assert!(!body.contains("All checks report no issues"));
        assert!(body.contains("The transaction commit no longer loses events"));
        assert!(body.contains("This adds a test fixture with no new dependencies"));
        assert!(!body.contains("Security is clean"));
        assert!(body.contains("The test fixture maps transaction rollback"));
        assert!(body.contains("unit test fixture"));
        assert!(body.contains("parser now rejects unknown paths"));
        assert!(body.contains("parser handles unknown paths safely"));
        assert!(body.contains("Atomic commit mapping"));
        assert!(body.contains("The transaction commits each event once"));
        assert!(body.contains("The test fixture maps the empty-state case"));
        assert!(!body.contains("The commit reviewer found no issues"));
        assert!(
            body.contains("Pull request purpose (title): Enforce speech privacy."),
            "{body}"
        );
        assert!(opening.starts_with("**Verdict:** Incomplete"), "{opening}");
        assert!(body.contains("**Lane gaps:**"), "{body}");
        assert!(body.contains("reviewers did not respond"), "{body}");
    }

    #[tokio::test]
    async fn mock_forge_hub_links_a_citation_to_the_reviewed_sha() {
        let forge = forge("abc123");
        let mut proposal = proposal("abc123", vec![]);
        proposal.summary = Some(crate::summary::ReviewSummary {
            features: vec![crate::summary::Feature {
                kind: crate::summary::FeatureKind::Addition,
                name: "Saved value".into(),
                impact: "A persisted field.".into(),
                citations: vec!["src/savings_store.rs:42".into()],
            }],
            ..Default::default()
        });

        apply(&forge, &forge, &config(), &proposal, None)
            .await
            .expect("applies");

        let posted = overview_comments(&forge);
        assert_eq!(posted.len(), 1, "{posted:#?}");
        assert!(
            matches!(
                &posted[0],
                Write::Comment { body, .. }
                    if body.contains("[`src/savings_store.rs:42`](https://github.com/tinyhumansai/tinysweeper/blob/abc123/src/savings_store.rs#L42)")
            ),
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

        let comments = overview_comments(&forge);
        assert_eq!(comments.len(), 1, "{:#?}", forge.writes());
        assert!(
            matches!(&comments[0], Write::Comment { body, .. } if !body.contains("```mermaid"))
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

        assert_eq!(
            overview_comments(&forge).len(),
            1,
            "the hub exists without a flow"
        );
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
                noted: false,
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
                noted: false,
            }],
        };

        apply(&forge, &forge, &config(), &proposal, None)
            .await
            .expect("applies");

        assert!(forge.writes().is_empty(), "{:?}", forge.writes());
    }
}
