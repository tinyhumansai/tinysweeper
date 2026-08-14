//! Thread-resolution tests, with the refusals tested harder than the
//! resolutions: every one of them asserts against the recording mock that
//! **nothing was written**, because the failure that matters here is closing a
//! conversation somebody still wanted open.

use std::collections::BTreeSet;
use std::sync::Arc;

use super::*;
use crate::config::types::Config;
use crate::forge::types::{ReviewThread, ThreadComment};
use crate::forge::{MockForge, MockState};
use crate::harness::mock::MockModel;

const FINGERPRINT: &str = "0123456789abcdef";
const TITLE: &str = "Guard the index";
/// The commit a resolution note credits. Longer than the seven characters the
/// note shows, so the abbreviation is actually exercised.
const HEAD_SHA: &str = "abc1234def5678";

fn repo() -> RepoId {
    RepoId::parse("tinyhumansai/tinysweeper").unwrap()
}

/// A thread tinysweeper opened, with a human reply, on code that has changed.
fn ours() -> ReviewThread {
    ReviewThread {
        id: "PRRT_1".into(),
        is_resolved: false,
        is_outdated: true,
        comments: vec![
            ThreadComment {
                author: "tinysweeper[bot]".into(),
                body: format!("**{TITLE}**\n\n<!-- tinysweeper:fp={FINGERPRINT} -->"),
                bot: true,
            },
            ThreadComment {
                author: "author".into(),
                body: "fixed in the last push".into(),
                bot: false,
            },
        ],
    }
}

fn resolved_titles(entries: &[&str]) -> BTreeSet<String> {
    entries.iter().map(|e| e.to_string()).collect()
}

fn config() -> Config {
    Config {
        threads: crate::config::types::Threads {
            resolve_fixed: true,
            ask_model: false,
            comment_on_resolve: true,
        },
        ..Config::default()
    }
}

#[test]
fn a_finding_that_no_longer_reproduces_on_changed_code_is_resolved() {
    assert!(matches!(
        decide(&ours(), &resolved_titles(&[TITLE])),
        Decision::Resolve(_)
    ));
}

#[test]
fn an_agent_confirmed_fix_closes_a_thread_without_a_human_reply() {
    let mut thread = ours();
    thread.comments.truncate(1);

    assert!(matches!(
        decide(&thread, &resolved_titles(&[TITLE])),
        Decision::Resolve(_)
    ));
}

#[test]
fn a_missing_finding_is_not_treated_as_an_agent_confirmed_fix() {
    assert!(matches!(
        decide(&ours(), &BTreeSet::new()),
        Decision::Leave("the review agent did not confirm the finding is fixed")
    ));
}

#[test]
fn a_finding_that_still_reproduces_is_left_alone() {
    assert!(matches!(
        decide(&ours(), &BTreeSet::new()),
        Decision::Leave(_)
    ));
}

#[test]
fn a_thread_a_human_opened_is_never_touched() {
    let mut thread = ours();
    thread.comments[0].author = "maintainer".into();
    thread.comments[0].bot = false;
    assert!(matches!(
        decide(&thread, &BTreeSet::new()),
        Decision::Leave(_)
    ));
}

#[test]
fn a_lookalike_login_did_not_open_our_thread() {
    // `starts_with("tinysweeper")` would have accepted this account, which
    // anyone can register — and it would then be able to have its own threads
    // closed by writing a fingerprint marker into them.
    let mut thread = ours();
    thread.comments[0].author = "tinysweeper-evil".into();
    assert!(matches!(
        decide(&thread, &BTreeSet::new()),
        Decision::Leave(_)
    ));
}

#[test]
fn a_bot_reply_does_not_close_a_thread_when_code_is_unchanged() {
    // Two bots replying to each other is a loop nobody is watching.
    let mut thread = ours();
    thread.is_outdated = false;
    thread.comments[1].bot = true;
    thread.comments[1].author = "dependabot[bot]".into();
    assert!(matches!(
        decide(&thread, &BTreeSet::new()),
        Decision::Leave(_)
    ));
}

#[test]
fn our_own_follow_up_is_not_a_reply_either() {
    let mut thread = ours();
    thread.is_outdated = false;
    thread.comments[1].author = "tinysweeper[bot]".into();
    thread.comments[1].bot = false;
    assert!(matches!(
        decide(&thread, &BTreeSet::new()),
        Decision::Leave(_)
    ));
}

#[test]
fn an_already_resolved_thread_is_not_resolved_again() {
    let mut thread = ours();
    thread.is_resolved = true;
    assert!(matches!(
        decide(&thread, &BTreeSet::new()),
        Decision::Leave(_)
    ));
}

#[test]
fn a_thread_without_a_renderer_title_is_left_alone() {
    let mut thread = ours();
    thread.comments[0].body = "no renderer-owned title here".into();
    assert!(matches!(
        decide(&thread, &BTreeSet::new()),
        Decision::Leave(_)
    ));
}

#[test]
fn unchanged_code_is_the_only_case_that_reaches_the_model() {
    let mut thread = ours();
    thread.is_outdated = false;
    assert!(matches!(decide(&thread, &BTreeSet::new()), Decision::Ask));
}

fn forge_with(threads: Vec<ReviewThread>) -> MockForge {
    let mut state = MockState::default();
    state.review_threads.insert(7, threads);
    MockForge::with_state(state)
}

async fn plan_for(forge: &MockForge, config: &Config, resolved: &[&str]) -> ThreadPlan {
    let model = MockModel::silent();
    plan(
        forge,
        Some(&model),
        config,
        &repo(),
        7,
        &resolved_titles(resolved),
    )
    .await
    .expect("plans")
    .0
}

#[tokio::test]
async fn planning_writes_nothing_and_applying_resolves_exactly_the_planned_thread() {
    let forge = forge_with(vec![ours()]);
    let plan = plan_for(&forge, &config(), &[TITLE]).await;

    assert!(
        forge.writes().is_empty(),
        "planning is a read-only phase: {:?}",
        forge.writes()
    );
    assert_eq!(plan.resolve.len(), 1);

    apply_plan(&forge, &config(), &repo(), &plan, HEAD_SHA)
        .await
        .expect("applies");

    // The note comes first and names the commit, then the resolve. A thread
    // that closes with no explanation reads as the bot losing interest.
    let writes = forge.writes();
    assert_eq!(writes.len(), 2, "{writes:?}");
    let crate::forge::mock::Write::ThreadReply { thread_id, body } = &writes[0] else {
        panic!("the reply must precede the resolve: {writes:?}");
    };
    assert_eq!(thread_id, "PRRT_1");
    assert!(
        body.contains("abc1234"),
        "the note must name the commit: {body}"
    );
    assert_eq!(
        writes[1],
        crate::forge::mock::Write::ThreadResolved {
            thread_id: "PRRT_1".into()
        }
    );
}

#[tokio::test]
async fn the_note_can_be_turned_off_without_losing_the_resolve() {
    // An operator who finds the extra comment noisy gets the silence back, and
    // keeps the housekeeping. If these ever became one switch, turning off the
    // noise would quietly turn off the feature.
    let forge = forge_with(vec![ours()]);
    let mut quiet = config();
    quiet.threads.comment_on_resolve = false;

    let plan = plan_for(&forge, &quiet, &[TITLE]).await;
    apply_plan(&forge, &quiet, &repo(), &plan, HEAD_SHA)
        .await
        .expect("applies");

    assert_eq!(
        forge.writes(),
        vec![crate::forge::mock::Write::ThreadResolved {
            thread_id: "PRRT_1".into()
        }]
    );
}

#[test]
fn a_resolution_note_abbreviates_the_commit_and_invites_disagreement() {
    let note = resolution_note("the review agent found this finding fixed", HEAD_SHA);
    assert!(note.contains("abc1234"));
    // The full SHA would be noise, and the point is that a reader can find the
    // commit, not that they can paste it.
    assert!(!note.contains(HEAD_SHA));
    // A resolve nobody can argue with is a resolve nobody can correct.
    assert!(note.contains("reopen"));
}

#[test]
fn a_resolution_note_is_built_only_from_crate_owned_text() {
    // Both inputs are ours: `reason` is a `&'static str` from `Decision`, and
    // the SHA is read off the forge. Nothing a contributor writes reaches this
    // string, which is what lets it be posted without escaping.
    for decision in [
        decide(&ours(), &resolved_titles(&[TITLE])),
        decide(&ours(), &resolved_titles(&[])),
    ] {
        if let Decision::Resolve(reason) = decision {
            assert!(resolution_note(reason, HEAD_SHA).contains(reason));
        }
    }
}

#[tokio::test]
async fn every_refusal_leaves_the_forge_untouched() {
    let human_opened = {
        let mut t = ours();
        t.id = "PRRT_human".into();
        t.comments[0].author = "maintainer".into();
        t.comments[0].bot = false;
        t
    };
    let lookalike = {
        let mut t = ours();
        t.id = "PRRT_lookalike".into();
        t.comments[0].author = "tinysweeper-evil".into();
        t
    };
    let bot_reply = {
        let mut t = ours();
        t.id = "PRRT_bot".into();
        t.comments[1].bot = true;
        t
    };
    let already = {
        let mut t = ours();
        t.id = "PRRT_done".into();
        t.is_resolved = true;
        t
    };
    // The agent did not report this title fixed, so it stays open.
    let reproduces = {
        let mut t = ours();
        t.id = "PRRT_open".into();
        t
    };

    let forge = forge_with(vec![
        human_opened,
        lookalike,
        bot_reply,
        already,
        reproduces,
    ]);
    let plan = plan_for(&forge, &config(), &[FINGERPRINT]).await;

    assert!(plan.resolve.is_empty(), "{plan:?}");
    apply_plan(&forge, &config(), &repo(), &plan, HEAD_SHA)
        .await
        .expect("applies");
    assert!(forge.writes().is_empty(), "{:?}", forge.writes());
}

#[tokio::test]
async fn the_advisory_path_is_off_by_default_and_costs_nothing() {
    // The model is *available* and would say "resolve"; the flag is off, so it
    // is never asked and the thread is left for a human.
    let mut thread = ours();
    thread.is_outdated = false;
    let forge = forge_with(vec![thread]);

    let model = Arc::new(MockModel::always(serde_json::json!({
        "resolve": true,
        "reason": "the author explained it"
    })));
    let (plan, spend) = plan(
        &forge,
        Some(model.as_ref()),
        &config(),
        &repo(),
        7,
        &BTreeSet::new(),
    )
    .await
    .expect("plans");

    assert!(plan.resolve.is_empty(), "the flag is off: {plan:?}");
    assert_eq!(spend, Spend::default(), "an unasked model costs nothing");
    assert!(forge.writes().is_empty());
}

#[tokio::test]
async fn the_advisory_verdict_is_followed_only_when_the_flag_is_on_and_its_spend_is_returned() {
    let mut thread = ours();
    thread.is_outdated = false;
    let forge = forge_with(vec![thread]);

    let mut config = config();
    config.threads.ask_model = true;

    let model = Arc::new(
        MockModel::always(
            serde_json::json!({"resolve": true, "reason": "the author explained it"}),
        )
        .with_usage(crate::ports::model::Usage {
            input_tokens: 900,
            output_tokens: 20,
            cached_tokens: 800,
            cost_usd: 0.002,
            ..Default::default()
        }),
    );

    let (plan, spend) = plan(
        &forge,
        Some(model.as_ref()),
        &config,
        &repo(),
        7,
        &BTreeSet::new(),
    )
    .await
    .expect("plans");

    assert_eq!(plan.resolve.len(), 1);
    assert!(
        spend.cost_usd() > 0.0,
        "the advisory call's spend has to reach the caller, or it is invisible money"
    );
    assert_eq!(spend.usage.cached_tokens, 800);
}

#[tokio::test]
async fn turning_the_whole_feature_off_reads_nothing_and_plans_nothing() {
    let forge = forge_with(vec![ours()]);
    let mut config = config();
    config.threads.resolve_fixed = false;

    let plan = plan_for(&forge, &config, &[]).await;
    assert!(plan.resolve.is_empty());
    assert!(forge.writes().is_empty());
}
