//! `local-review`, end to end, against a real git repository and a canned model.
//!
//! The point of these is the seam: a range on disk becomes the same
//! `PullRequestContext` a pull request produces, so the lanes cannot tell the
//! difference. Anything that only worked because the forge filled a field in is
//! a bug this catches.

use std::sync::Arc;

use serde_json::json;

use super::*;
use crate::config::types::Config;
use crate::evidence::git::fixture::Repo;
use crate::harness::mock::MockModel;

/// The built-in defaults, which is what a repository with no config gets.
fn config() -> Config {
    crate::config::DEFAULTS
        .parse::<toml::Table>()
        .unwrap()
        .try_into()
        .unwrap()
}

/// Only the critique lane, so one canned response covers the whole run.
fn critique_only() -> Config {
    let mut config = config();
    config.review.lanes = vec!["critique".to_string()];
    config
}

/// The whole working tree, against `main`.
fn worktree() -> LocalInput {
    LocalInput {
        range: Range {
            base: "main".to_string(),
            head: None,
        },
        title: None,
        body: None,
    }
}

fn finding(path: &str, existing_code: &str) -> serde_json::Value {
    json!({
        "summary": "One thing to fix.",
        "findings": [{
            "path": path,
            "existing_code": existing_code,
            "rule": "unchecked-index",
            "title": "Guard the index before dereferencing",
            "body": "`items[i]` panics when `items` is empty.",
            "severity": "high",
            "confidence": 0.9,
        }],
    })
}

#[test]
fn the_title_is_the_newest_commits_subject() {
    let range = ResolvedRange {
        commits: vec![
            crate::forge::types::Commit {
                message: "first: oldest\n\nbody".into(),
                ..Default::default()
            },
            crate::forge::types::Commit {
                message: "second: newest\n\nbody".into(),
                ..Default::default()
            },
        ],
        ..ResolvedRange::default()
    };
    assert_eq!(default_title(&range), "second: newest");
}

#[test]
fn a_range_with_no_commits_says_so_rather_than_inventing_a_title() {
    assert_eq!(
        default_title(&ResolvedRange::default()),
        "Uncommitted changes"
    );
}

#[test]
fn the_local_number_cannot_collide_with_a_pull_request() {
    // GitHub numbers items from 1, so 0 is unreachable by construction.
    assert_eq!(LOCAL_NUMBER, 0);
}

#[tokio::test]
async fn a_finding_on_a_local_range_is_anchored_the_same_way_a_pull_request_is() {
    let repo = Repo::new();
    repo.write(
        "src/lib.rs",
        "fn first(items: &[u8]) -> u8 {\n    items[0]\n}\n",
    );

    let model = Arc::new(MockModel::panel(finding("src/lib.rs", "items[0]")));
    let (proposal, context) = local_review(repo.path(), &worktree(), model, &critique_only())
        .await
        .expect("reviews");

    let critique = proposal
        .lanes
        .iter()
        .find(|lane| lane.check_name == "tinysweeper/critique")
        .expect("the critique lane ran");
    assert_eq!(critique.findings.len(), 1, "{:?}", critique.findings);

    let found = &critique.findings[0];
    assert_eq!(found.path, "src/lib.rs");
    // Anchored by the code it quoted, not by a line number it guessed — the
    // same rule `src/position` applies on the forge path.
    assert_eq!(found.line, Some(2));
    assert!(found.identity.is_some(), "fingerprinted for dedupe");
    assert!(context.range.dirty);
}

#[tokio::test]
async fn a_local_review_holds_no_write_handle_and_writes_nothing() {
    let repo = Repo::new();
    repo.write("src/lib.rs", "fn one() {}\n");

    let model = Arc::new(MockModel::silent());
    let (proposal, _) = local_review(repo.path(), &worktree(), model, &critique_only())
        .await
        .expect("reviews");

    // There is no `ForgeWrite` on this path at all — the assertion that matters
    // is the type, and this is the observable half of it.
    assert_eq!(proposal.number, LOCAL_NUMBER);
    assert!(proposal.head_sha.len() == 40, "{}", proposal.head_sha);
    let status = repo.git(&["status", "--porcelain"]);
    assert!(
        !status.contains("README.md"),
        "the review touched the checkout: {status}"
    );
}

#[tokio::test]
async fn base_branch_commits_are_not_reviewed_as_this_ranges_work() {
    let repo = Repo::new();
    repo.git(&["checkout", "-b", "feature"]);
    repo.write("src/mine.rs", "fn mine() {}\n");
    repo.commit("feat: mine");
    repo.git(&["checkout", "main"]);
    repo.write("src/theirs.rs", "fn theirs() {}\n");
    repo.commit("feat: theirs");
    repo.git(&["checkout", "feature"]);

    // A finding against a file only the *base* branch changed. If the range
    // were a two-dot diff, the file would be in it and the finding would post
    // against work this branch's author never did.
    let model = Arc::new(MockModel::panel(finding("src/theirs.rs", "fn theirs() {}")));
    let input = LocalInput {
        range: Range {
            base: "main".to_string(),
            head: Some("HEAD".to_string()),
        },
        title: None,
        body: None,
    };

    let (proposal, _) = local_review(repo.path(), &input, model, &critique_only())
        .await
        .expect("reviews");

    let critique = proposal
        .lanes
        .iter()
        .find(|lane| lane.check_name == "tinysweeper/critique")
        .expect("ran");
    assert!(
        critique.findings.is_empty(),
        "a finding on a file this range never touched must be dropped: {:?}",
        critique.findings
    );
}

#[tokio::test]
async fn the_repository_rules_are_read_from_the_working_tree_the_diff_came_from() {
    let repo = Repo::new();
    // Two markers with no substring relationship: "uncommitted rules" contains
    // "committed rules", so the obvious pair cannot tell the two apart.
    repo.write("AGENTS.md", "rules-as-of-the-commit\n");
    repo.commit("docs: rules");
    repo.write("AGENTS.md", "rules-as-of-the-working-tree\n");
    repo.write("src/lib.rs", "fn one() {}\n");

    // One answer that satisfies every schema in the run: what is being asserted
    // is which bytes reached the extraction prompt, not how many calls it took
    // to get there.
    let model = Arc::new(MockModel::panel(
        json!({"summary": "Nothing to report.", "findings": [], "rules": [], "rejected": []}),
    ));
    let recorder = model.clone();

    local_review(repo.path(), &worktree(), model, &critique_only())
        .await
        .expect("reviews");

    let prompts = recorder.requests();
    let extraction = prompts
        .first()
        .expect("the extraction call happened")
        .messages
        .iter()
        .map(|m| m.content.as_str())
        .collect::<String>();

    // The diff under review contains the uncommitted edit, so the rules the
    // model is shown have to be the uncommitted ones. Reading the committed
    // file would make the reviewer argue with rules the author has already
    // changed.
    assert!(
        extraction.contains("rules-as-of-the-working-tree"),
        "{extraction}"
    );
    assert!(
        !extraction.contains("rules-as-of-the-commit"),
        "{extraction}"
    );
}

#[tokio::test]
async fn a_range_with_no_changes_spends_nothing() {
    let repo = Repo::new();

    // A model with nothing queued errors on the first call, so this passing at
    // all proves no call was made.
    let model = Arc::new(MockModel::new());
    let (proposal, _) = local_review(repo.path(), &worktree(), model, &critique_only())
        .await
        .expect("reviews");

    assert_eq!(proposal.cost_usd, 0.0);
    assert_eq!(proposal.input_tokens, 0);
}

#[tokio::test]
async fn an_explicit_description_reaches_the_description_lane() {
    let repo = Repo::new();
    repo.write("src/lib.rs", "fn one() {}\n");

    let mut config = config();
    config.review.lanes = vec!["description".to_string()];
    // The knowledge pass runs first even with one lane enabled.
    let model = Arc::new(MockModel::panel(
        json!({"summary": "Accurate.", "findings": [], "rules": [], "rejected": []}),
    ));
    let recorder = model.clone();

    let input = LocalInput {
        title: Some("feat: add one".to_string()),
        body: Some("Adds a function called one.".to_string()),
        ..worktree()
    };

    local_review(repo.path(), &input, model, &config)
        .await
        .expect("reviews");

    let prompts = recorder.requests();
    let all = prompts
        .iter()
        .flat_map(|r| r.messages.iter())
        .map(|m| m.content.as_str())
        .collect::<String>();
    assert!(all.contains("feat: add one"), "the title is evidence");
    assert!(
        all.contains("Adds a function called one."),
        "so is the body"
    );
}
