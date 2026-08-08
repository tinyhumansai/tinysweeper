//! `tinysweeper apply` — the write half.
//!
//! Reads a proposal produced by `review` and publishes it. It holds no model
//! key and makes no model call: the verdict was already reached, and this
//! module's only job is to put it on GitHub without changing it.
//!
//! Before writing anything it re-fetches live state and checks the head SHA
//! still matches. A review of a commit nobody is looking at any more is worse
//! than no review — it reports on code that has already been replaced.

use crate::app::review::Proposal;
use crate::config::types::{Config, Severity};
use crate::error::{Error, Result};
use crate::forge::types::{CheckRun, RepoId, ReviewComment, ReviewEvent};
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

    for lane in &proposal.lanes {
        write
            .publish_check(
                &repo,
                CheckRun {
                    name: lane.check_name.clone(),
                    head_sha: proposal.head_sha.clone(),
                    conclusion: lane.conclusion,
                    title: title_for(lane.findings.len(), &lane.summary),
                    summary: render_lane_summary(lane),
                },
            )
            .await?;
    }

    // Whether we are already blocking this pull request. GitHub keeps only the
    // latest review per reviewer, so this is also how a fixed pull request gets
    // unblocked: without an explicit clearing verdict a stale objection blocks
    // the merge button until a human dismisses it by hand.
    let blocking_now = previously_blocked(read, &repo, proposal.number).await;
    let event = review_event(config, proposal, blocking_now);
    let comments = inline_comments(proposal);

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
    let newly_posted: Vec<String> = proposal
        .findings()
        .filter(|finding| finding.line.is_some())
        .filter_map(|finding| finding.identity.clone())
        .collect();

    // Submit a review if:
    // - there are inline comments to post, or
    // - the verdict is Approve (clears a previous block), or
    // - the verdict is RequestChanges (blocks the merge, even if only in the summary).
    // Blocking verdicts must be submitted even without inline comments, because
    // findings that could not be anchored to lines still appear in the summary
    // and need the blocking verdict on GitHub to enforce the gate.
    if !comments.is_empty() || event == ReviewEvent::Approve || event == ReviewEvent::RequestChanges
    {
        write
            .create_review(
                &repo,
                proposal.number,
                &review_body(proposal, event),
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

    Ok(())
}

/// Decide how to submit the review.
fn review_event(config: &Config, proposal: &Proposal, blocking_now: bool) -> ReviewEvent {
    let Some(threshold) = config.request_changes_at() else {
        return ReviewEvent::Comment;
    };

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
    let severe_enough = proposal.has_severity_at_or_above(threshold);
    if proposal.blocked() && severe_enough {
        ReviewEvent::RequestChanges
    } else if blocking_now {
        // Clean now, blocked before: clear it. Anything else leaves the author
        // stuck behind an objection that no longer applies.
        ReviewEvent::Approve
    } else {
        ReviewEvent::Comment
    }
}

/// Whether tinysweeper's own last review on this pull request requested changes.
///
/// Read from the forge rather than remembered, so it stays correct across a
/// restart, a redeploy, and a human dismissing the review by hand.
async fn previously_blocked(read: &dyn ForgeRead, repo: &RepoId, number: u64) -> bool {
    match read.own_review_state(repo, number).await {
        Ok(state) => state == Some(ReviewEvent::RequestChanges),
        Err(err) => {
            // Failing closed here would mean never clearing a block. Failing
            // open at worst skips a redundant approval.
            tracing::warn!(%err, "could not read the previous review state");
            false
        }
    }
}

fn title_for(findings: usize, summary: &str) -> String {
    match findings {
        0 => summary.chars().take(80).collect(),
        1 => "1 finding".into(),
        n => format!("{n} findings"),
    }
}

fn render_lane_summary(lane: &crate::app::review::LaneProposal) -> String {
    let mut out = crate::findings::render::lane_summary(&lane.summary, &lane.findings, VERSION);

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

fn review_body(proposal: &Proposal, event: ReviewEvent) -> String {
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
        ReviewEvent::Approve => {
            "The previously-blocking findings are resolved. Clearing the changes request."
                .to_string()
        }
        ReviewEvent::Comment if blocking == 0 => "tinysweeper found nothing blocking.".to_string(),
        ReviewEvent::Comment => format!("tinysweeper: {blocking} lane(s) blocking."),
    };

    // The full token breakdown goes in the body deliberately. Cache hit rate is
    // the difference between a cheap re-review and a ruinous one, and nobody
    // tunes a number they cannot see.
    body.push_str(&format!(
        "\n\n<sub>{}</sub>",
        crate::findings::render::cost_line(
            proposal.cost_usd,
            proposal.input_tokens,
            proposal.output_tokens,
            proposal.cached_tokens,
            &proposal.models,
        )
    ));
    body.push_str(&format!(
        "\n<!-- {MARKER_PREFIX}state v=1 sha={} -->",
        proposal.head_sha
    ));
    body
}

/// Inline comments for findings that name a specific line.
fn inline_comments(proposal: &Proposal) -> Vec<ReviewComment> {
    proposal
        .findings()
        .filter_map(|finding| {
            let line = finding.line?;
            Some(ReviewComment {
                path: finding.path.clone(),
                line,
                start_line: None,
                // The forge assigns the author on the way in; on the way out it
                // is what tells dedupe whether a marker is ours.
                author: String::new(),
                body: format!(
                    "{} **{}**\n\n{}\n\n<sub>{} · {} · <!-- {MARKER_PREFIX}fp={} --></sub>",
                    crate::findings::render::badge(finding.severity),
                    finding.title,
                    finding.body,
                    finding.lane,
                    crate::findings::render::confidence_badge(finding.confidence),
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
    use crate::forge::types::{CheckConclusion, PullRequest};
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
            version: 1,
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
                resolved: vec![],
                deduped: 0,
                highest_severity,
            }],
            cost_usd: 0.01,
            input_tokens: 10_000,
            output_tokens: 400,
            cached_tokens: 800,
            models: vec!["moonshotai/kimi-k3".into()],
        }
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
            late: false,
            identity: None,
        }
    }

    fn forge(head: &str) -> MockForge {
        let mut state = MockState::default();
        state.pull_requests.insert(
            7,
            PullRequest {
                number: 7,
                head_sha: head.into(),
                ..PullRequest::default()
            },
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
            CheckConclusion::Success
        );
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
        assert_eq!(review[0].line, 2);
        assert!(
            review[0].body.contains("tinysweeper:fp="),
            "{}",
            review[0].body
        );
    }

    #[tokio::test]
    async fn a_clean_review_posts_no_inline_comments_at_all() {
        let forge = forge("abc123");
        apply(&forge, &forge, &config(), &proposal("abc123", vec![]), None)
            .await
            .expect("applies");

        assert!(
            !forge
                .writes()
                .iter()
                .any(|w| matches!(w, Write::Review { .. })),
            "silence should be silent"
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
    async fn a_clean_pull_request_that_was_never_blocked_stays_silent() {
        // No approval to hand out: approving every green pull request would be
        // a bot rubber-stamping work it did not really vouch for.
        let forge = forge("abc123");
        apply(&forge, &forge, &config(), &proposal("abc123", vec![]), None)
            .await
            .expect("applies");

        assert!(review_of(&forge).is_none(), "{:#?}", forge.writes());
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
}
