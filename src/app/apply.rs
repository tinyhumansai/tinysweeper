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
use crate::error::{Error, Result};
use crate::forge::types::{CheckRun, RepoId, ReviewComment};
use crate::ports::forge::{ForgeRead, ForgeWrite};
use crate::{MARKER_PREFIX, VERSION};

/// Publish a proposal.
pub async fn apply(
    read: &dyn ForgeRead,
    write: &dyn ForgeWrite,
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

    let comments = inline_comments(proposal);
    if !comments.is_empty() {
        write
            .create_review(&repo, proposal.number, &review_body(proposal), comments)
            .await?;
    }

    Ok(())
}

fn title_for(findings: usize, summary: &str) -> String {
    match findings {
        0 => summary.chars().take(80).collect(),
        1 => "1 finding".into(),
        n => format!("{n} findings"),
    }
}

fn render_lane_summary(lane: &crate::app::review::LaneProposal) -> String {
    let mut out = String::new();
    out.push_str(&lane.summary);
    out.push_str("\n\n");

    if lane.findings.is_empty() {
        out.push_str("No findings.\n");
    } else {
        for finding in &lane.findings {
            out.push_str(&format!("### {}\n\n`{}`", finding.title, finding.path));
            if let Some(line) = finding.line {
                out.push_str(&format!(":{line}"));
            }
            out.push_str(&format!(
                " · **{}** · confidence {:.0}%\n\n{}\n\n",
                finding.severity,
                finding.confidence * 100.0,
                finding.body
            ));
        }
    }

    out.push_str(&format!("\n<sub>tinysweeper {VERSION}</sub>\n"));
    out
}

fn review_body(proposal: &Proposal) -> String {
    let blocking = proposal
        .lanes
        .iter()
        .filter(|l| l.conclusion.blocks())
        .count();
    let mut body = if blocking == 0 {
        "tinysweeper found nothing blocking.".to_string()
    } else {
        format!("tinysweeper: {blocking} lane(s) blocking.")
    };

    // Cost and cache-hit rate go in the body deliberately: prompt-cache hit
    // rate is the difference between a cheap re-review and a ruinous one, and
    // nobody looks at a metric they cannot see.
    body.push_str(&format!(
        "\n\n<sub>${:.3} · {} cached prompt tokens</sub>",
        proposal.cost_usd, proposal.cached_tokens
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
                    "**{}**\n\n{}\n\n<sub>{} · {} · <!-- {MARKER_PREFIX}fp={} --></sub>",
                    finding.title,
                    finding.body,
                    finding.lane,
                    finding.severity,
                    finding.fingerprint(&finding.title),
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
            cached_tokens: 800,
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
        apply(&forge, &forge, &proposal("abc123", vec![]))
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
        apply(&forge, &forge, &proposal("abc123", vec![]))
            .await
            .expect("returns cleanly");

        assert!(forge.wrote_nothing(), "{:#?}", forge.writes());
    }

    #[tokio::test]
    async fn findings_become_inline_comments_carrying_a_fingerprint() {
        let forge = forge("abc123");
        apply(&forge, &forge, &proposal("abc123", vec![finding()]))
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
        apply(&forge, &forge, &proposal("abc123", vec![]))
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

    #[tokio::test]
    async fn the_review_body_reports_cost_and_cache_hits() {
        let forge = forge("abc123");
        apply(&forge, &forge, &proposal("abc123", vec![finding()]))
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

        assert!(body.contains("$0.010"), "{body}");
        assert!(body.contains("800 cached prompt tokens"), "{body}");
    }
}
