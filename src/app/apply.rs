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
use crate::{MARKER_PREFIX, VERSION};

/// Publish a proposal.
pub async fn apply(
    read: &dyn ForgeRead,
    write: &dyn ForgeWrite,
    config: &Config,
    proposal: &Proposal,
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

    // An Approve is submitted even with nothing to say, because its entire job
    // is to clear the previous block.
    if !comments.is_empty() || event == ReviewEvent::Approve {
        write
            .create_review(
                &repo,
                proposal.number,
                &review_body(proposal, event),
                comments,
                event,
            )
            .await?;
    }

    Ok(())
}

/// Decide how to submit the review.
fn review_event(config: &Config, proposal: &Proposal, blocking_now: bool) -> ReviewEvent {
    let Some(threshold) = config.request_changes_at() else {
        return ReviewEvent::Comment;
    };

    let blocking_findings = proposal.findings().any(|f| f.severity >= threshold);
    if blocking_findings {
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
            out.push_str(&format!("- {title}\n"));
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
        apply(&forge, &forge, &config(), &proposal("abc123", vec![]))
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
        apply(&forge, &forge, &config(), &proposal("abc123", vec![]))
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
        apply(&forge, &forge, &config(), &proposal("abc123", vec![]))
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
        )
        .await
        .expect("applies");

        let (body, event) = review_of(&forge).expect("a review was posted");
        assert_eq!(event, ReviewEvent::RequestChanges);
        assert!(body.contains("Requesting changes"), "{body}");
        assert!(body.contains("**high**"), "{body}");
    }

    #[tokio::test]
    async fn a_finding_below_the_threshold_only_comments() {
        let mut low = finding();
        low.severity = Severity::Medium;

        let forge = forge("abc123");
        apply(&forge, &forge, &config(), &proposal("abc123", vec![low]))
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
        apply(&forge, &forge, &config(), &proposal("abc123", vec![]))
            .await
            .expect("applies");

        let (body, event) = review_of(&forge).expect("an approval was posted");
        assert_eq!(event, ReviewEvent::Approve);
        assert!(body.contains("Clearing the changes request"), "{body}");
    }

    #[tokio::test]
    async fn a_clean_pull_request_that_was_never_blocked_stays_silent() {
        // No approval to hand out: approving every green pull request would be
        // a bot rubber-stamping work it did not really vouch for.
        let forge = forge("abc123");
        apply(&forge, &forge, &config(), &proposal("abc123", vec![]))
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
