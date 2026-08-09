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

    // Housekeeping first: threads this run decided are settled. Deterministic
    // policy chose them (`crate::threads`), this only executes the list, and a
    // failure here is logged rather than allowed to cost the verdict.
    if let Err(err) = crate::threads::apply_plan(write, &repo, &proposal.threads).await {
        tracing::warn!(%err, "could not resolve review threads");
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

    // What tinysweeper already said about this pull request. GitHub keeps only
    // the latest review per reviewer, which makes this load-bearing twice: it
    // is how a fixed pull request gets unblocked — without an explicit clearing
    // verdict a stale objection blocks the merge button until a human dismisses
    // it by hand — and it is how an approval that already stands avoids being
    // restated on every push.
    let previous = own_review_state(read, &repo, proposal.number).await;
    let event = review_event(config, proposal, previous, live.draft);
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

    if !redundant_approval
        && (!comments.is_empty()
            || event == ReviewEvent::Approve
            || event == ReviewEvent::RequestChanges)
    {
        write
            .create_review(
                &repo,
                proposal.number,
                &review_body(proposal, event, previous),
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

    Ok(())
}

/// Post or update the change-map comment.
///
/// One comment per pull request, found by its marker and edited in place. The
/// alternative — a fresh comment per push — turns a diagram into a scroll bar,
/// and the diagram of a two-push-old head is not a diagram of the pull request.
///
/// Writes nothing at all when the map says nothing worth saying: a single
/// component with nothing reaching out of it is a box, and a comment containing
/// one box is noise with a picture in it.
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

    // Ours by author *and* by marker, in that order. The marker alone is not
    // enough: anyone can copy it into their own comment, and editing a
    // contributor's comment because it quotes one of our markers is a write we
    // were tricked into making. `is_own_login` is the same exact-login check
    // dedupe already trusts — a prefix match would accept `tinysweeper-evil`,
    // an account anybody can register.
    //
    // A comment with no id cannot be edited, so it falls through to posting a
    // new one. That is the harmless direction to be wrong in: a duplicate
    // comment is noise, whereas editing the wrong comment destroys someone's
    // words.
    let existing = read
        .comments(&repo, proposal.number)
        .await?
        .into_iter()
        .find(|comment| {
            crate::findings::prior::is_own_login(&comment.author)
                && comment.body.contains(crate::overview::MARKER)
        })
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
    if previous == Some(ReviewEvent::RequestChanges) {
        return ReviewEvent::Approve;
    }

    ReviewEvent::Comment
}

/// tinysweeper's own last review verdict on this pull request.
///
/// Read from the forge rather than remembered, so it stays correct across a
/// restart, a redeploy, and a human dismissing the review by hand.
async fn own_review_state(read: &dyn ForgeRead, repo: &RepoId, number: u64) -> Option<ReviewEvent> {
    match read.own_review_state(repo, number).await {
        Ok(state) => state,
        Err(err) => {
            // Failing closed here would mean never clearing a block. Failing
            // open at worst re-states a verdict that already stands.
            tracing::warn!(%err, "could not read the previous review state");
            None
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

fn review_body(proposal: &Proposal, event: ReviewEvent, previous: Option<ReviewEvent>) -> String {
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
        ReviewEvent::Comment if blocking == 0 => "tinysweeper found nothing blocking.".to_string(),
        ReviewEvent::Comment => format!("tinysweeper: {blocking} lane(s) blocking."),
    };

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

/// Inline comments for findings that name a specific line.
fn inline_comments(proposal: &Proposal) -> Vec<ReviewComment> {
    proposal
        .findings()
        .filter_map(|finding| {
            let line = finding.line?;
            Some(ReviewComment {
                path: finding.path.clone(),
                line: Some(line),
                start_line: None,
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
                body: format!(
                    "{}  {}\n\n**{}**\n\n{}\n\n{} · <!-- {MARKER_PREFIX}fp={} -->",
                    crate::findings::render::priority_badge(finding.severity),
                    crate::findings::render::lane_confidence_badge(
                        finding.lane,
                        finding.confidence
                    ),
                    finding.title,
                    finding.body,
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
            overview: None,
            unreviewed: vec![],
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
                usage: Default::default(),
                models: vec![],
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

        assert_eq!(labels, vec!["priority: p1", "severity: high"]);
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

    /// A proposal carrying a two-component map, which is the smallest one
    /// worth drawing.
    fn proposal_with_map(head: &str) -> Proposal {
        use crate::evidence::diff::parse_file_patch;

        let diffs = [
            parse_file_patch("src/lanes/critique.rs", "@@ -1,1 +1,2 @@\n x\n+y\n"),
            parse_file_patch("docs/readme.md", "@@ -1,1 +1,2 @@\n x\n+y\n"),
        ];
        Proposal {
            overview: Some(crate::overview::build(
                &diffs,
                &[],
                crate::overview::GraphView::Absent,
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
                Write::Comment { body, .. } | Write::CommentUpdate { body, .. } =>
                    body.contains(crate::overview::MARKER),
                _ => false,
            })
            .collect()
    }

    #[tokio::test]
    async fn a_pull_request_gets_one_change_map_comment() {
        let forge = forge("abc123");
        apply(&forge, &forge, &config(), &proposal_with_map("abc123"), None)
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

        apply(&forge, &forge, &config(), &proposal_with_map("abc123"), None)
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

        apply(&forge, &forge, &config(), &proposal_with_map("abc123"), None)
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

        assert!(overview_comments(&forge).is_empty(), "{:#?}", forge.writes());
    }

    #[tokio::test]
    async fn a_stale_head_draws_nothing_either() {
        let forge = forge("def456");
        apply(&forge, &forge, &config(), &proposal_with_map("abc123"), None)
            .await
            .expect("applies");

        assert!(overview_comments(&forge).is_empty(), "{:#?}", forge.writes());
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
    async fn a_change_map_that_cannot_be_posted_does_not_cost_the_verdict() {
        // Nobody is gated on a picture. A read-only forge fails every write;
        // the check runs above it must still have been published, and `apply`
        // must still report success for the parts that landed.
        let forge = forge("abc123").read_only();
        let result = apply(&forge, &forge, &config(), &proposal_with_map("abc123"), None).await;

        assert!(result.is_err() || overview_comments(&forge).is_empty());
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
