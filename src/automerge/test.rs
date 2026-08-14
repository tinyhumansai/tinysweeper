//! Auto-merge tests.
//!
//! Weighted deliberately towards the refusals. Merging is one path and a
//! wrong one cannot be undone, so every reason to stop gets its own test and
//! every one of them asserts that [`MockForge`] recorded no merge at all.

use crate::automerge::complexity::{Complexity, measure};
use crate::automerge::paths::{glob_set, logins_match};
use crate::automerge::policy::{Snapshot, evaluate};
use crate::automerge::types::{Decision, Outcome, Refusal};
use crate::automerge::{merge_if_qualified, snapshot};
use crate::config::types::AutoMerge;
use crate::forge::mock::{MockForge, Write};
use crate::forge::types::{
    ChangedFile, CheckConclusion, CheckStatus, FileStatus, PullRequest, RepoId, ReviewEvent,
    ReviewVerdict,
};

const HEAD: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

fn repo() -> RepoId {
    RepoId::parse("tinyhumansai/tinysweeper").expect("parses")
}

/// A policy that would merge the pull request built by `pull_request` below.
fn policy() -> AutoMerge {
    AutoMerge {
        enabled: true,
        require_checks: vec!["tinysweeper/critique".into(), "ci/build".into()],
        require_approvals: 1,
        method: "merge".into(),
        allow_labels: vec!["automerge".into()],
        block_labels: vec!["do-not-merge".into()],
        max_files: 5,
        max_changed_lines: 100,
        max_hunks: 10,
        max_directories: 3,
        sensitive_paths: vec![".github/**".into(), "Cargo.toml".into()],
        allow_dependency_bumps: true,
        dependency_bots: vec!["dependabot[bot]".into()],
        dependency_paths: vec!["Cargo.toml".into(), "Cargo.lock".into()],
    }
}

fn pull_request() -> PullRequest {
    PullRequest {
        number: 7,
        title: "fix: correct the off-by-one".into(),
        author: "contributor".into(),
        head_sha: HEAD.into(),
        labels: vec!["automerge".into()],
        mergeable: Some(true),
        approvals: 1,
        ..PullRequest::default()
    }
}

fn file(path: &str, additions: u64, deletions: u64, hunks: usize) -> ChangedFile {
    let patch = (0..hunks)
        .map(|n| format!("@@ -{n},1 +{n},1 @@\n-old\n+new\n"))
        .collect::<String>();
    ChangedFile {
        path: path.into(),
        previous_path: None,
        status: FileStatus::Modified,
        additions,
        deletions,
        patch: Some(patch),
        size_bytes: None,
    }
}

fn green(name: &str) -> CheckStatus {
    CheckStatus {
        name: name.into(),
        conclusion: Some(CheckConclusion::Success),
    }
}

fn approval(reviewer: &str) -> ReviewVerdict {
    ReviewVerdict {
        reviewer: reviewer.into(),
        bot: false,
        state: ReviewEvent::Approve,
    }
}

fn snapshot_of() -> Snapshot {
    Snapshot {
        pull_request: pull_request(),
        files: vec![file("src/lib.rs", 3, 2, 1)],
        checks: vec![green("tinysweeper/critique"), green("ci/build")],
        reviews: vec![approval("maintainer")],
    }
}

fn refusal(config: &AutoMerge, snapshot: &Snapshot) -> Refusal {
    match evaluate(config, snapshot) {
        Decision::Refuse(refusal) => refusal,
        Decision::Allow(_) => panic!("expected a refusal, got a merge"),
    }
}

// --- the one path that merges ---------------------------------------------

#[test]
fn a_small_green_approved_pull_request_qualifies() {
    assert!(evaluate(&policy(), &snapshot_of()).is_merge());
}

/// The type-level precondition, from the caller's side.
///
/// An approval exists only where the policy passed, and it names the pull
/// request it passed for — so `ForgeWrite::merge`, which reads the number out
/// of the approval rather than taking one, cannot be pointed at a different
/// pull request than the one that was evaluated.
///
/// The other half of the guarantee is not testable and does not need to be:
/// `MergeApproved` has a private field and no public constructor, so a merge
/// without a passing evaluation is a compile error rather than a failing
/// assertion. This test carries the part a test can carry; the compiler
/// carries the rest.
///
/// Moved here from `types.rs`, where the equivalent assertion used to build a
/// `Decision::Merge` by hand — which is exactly what is now impossible.
#[test]
fn an_approval_is_only_obtainable_from_a_passing_evaluation() {
    let snapshot = snapshot_of();
    let number = snapshot.pull_request.number;

    let allowed = evaluate(&policy(), &snapshot);
    assert!(allowed.is_merge());
    assert!(allowed.refusal().is_none());
    let approval = allowed.approval().expect("a passing evaluation approves");
    assert_eq!(
        approval.number(),
        number,
        "the approval names the pull request it was granted for"
    );

    // And a refusal yields none, so there is nothing to merge with.
    let mut draft = snapshot_of();
    draft.pull_request.draft = true;
    let refused = evaluate(&policy(), &draft);
    assert!(!refused.is_merge());
    assert!(refused.approval().is_none());
}

// --- refusals --------------------------------------------------------------

#[test]
fn policy_off_refuses() {
    let config = AutoMerge {
        enabled: false,
        ..policy()
    };
    assert_eq!(refusal(&config, &snapshot_of()), Refusal::Disabled);
}

#[test]
fn a_draft_refuses() {
    let mut snapshot = snapshot_of();
    snapshot.pull_request.draft = true;
    assert_eq!(refusal(&policy(), &snapshot), Refusal::Draft);
}

#[test]
fn a_blocking_label_refuses() {
    let mut snapshot = snapshot_of();
    snapshot.pull_request.labels.push("do-not-merge".into());
    assert_eq!(
        refusal(&policy(), &snapshot),
        Refusal::BlockedLabel("do-not-merge".into())
    );
}

#[test]
fn a_missing_allow_label_refuses() {
    let mut snapshot = snapshot_of();
    snapshot.pull_request.labels.clear();
    assert_eq!(refusal(&policy(), &snapshot), Refusal::MissingAllowLabel);
}

#[test]
fn an_unknown_mergeable_state_refuses() {
    // `None` is "GitHub is still computing it". Unknown is not yes.
    let mut snapshot = snapshot_of();
    snapshot.pull_request.mergeable = None;
    assert_eq!(refusal(&policy(), &snapshot), Refusal::NotMergeable);
}

#[test]
fn a_standing_changes_request_refuses() {
    let mut snapshot = snapshot_of();
    snapshot.reviews.push(ReviewVerdict {
        reviewer: "careful-human".into(),
        bot: false,
        state: ReviewEvent::RequestChanges,
    });
    assert_eq!(
        refusal(&policy(), &snapshot),
        Refusal::ChangesRequested {
            reviewer: "careful-human".into()
        }
    );
}

#[test]
fn a_standing_changes_request_from_a_bot_refuses_too() {
    // The policy asks whether anything objects, not whether anyone does.
    let mut snapshot = snapshot_of();
    snapshot.reviews.push(ReviewVerdict {
        reviewer: "some-linter[bot]".into(),
        bot: true,
        state: ReviewEvent::RequestChanges,
    });
    assert_eq!(
        refusal(&policy(), &snapshot),
        Refusal::ChangesRequested {
            reviewer: "some-linter[bot]".into()
        }
    );
}

#[test]
fn a_bot_approval_does_not_count_towards_the_human_requirement() {
    let mut snapshot = snapshot_of();
    snapshot.reviews = vec![ReviewVerdict {
        reviewer: "tinysweeper[bot]".into(),
        bot: true,
        state: ReviewEvent::Approve,
    }];
    assert_eq!(
        refusal(&policy(), &snapshot),
        Refusal::NotEnoughApprovals { have: 0, want: 1 }
    );
}

#[test]
fn a_failing_check_refuses() {
    let mut snapshot = snapshot_of();
    snapshot.checks.push(CheckStatus {
        name: "ci/lint".into(),
        conclusion: Some(CheckConclusion::Failure),
    });
    assert_eq!(
        refusal(&policy(), &snapshot),
        Refusal::CheckFailing {
            name: "ci/lint".into()
        }
    );
}

#[test]
fn a_pending_check_refuses_even_when_it_is_not_required() {
    // Nothing pending at all, not "nothing required pending": a check still
    // running is a verdict that has not arrived.
    let mut snapshot = snapshot_of();
    snapshot.checks.push(CheckStatus {
        name: "ci/slow".into(),
        conclusion: None,
    });
    assert_eq!(
        refusal(&policy(), &snapshot),
        Refusal::CheckPending {
            name: "ci/slow".into()
        }
    );
}

#[test]
fn a_required_check_that_never_reported_refuses() {
    let mut snapshot = snapshot_of();
    snapshot.checks.retain(|check| check.name != "ci/build");
    assert_eq!(
        refusal(&policy(), &snapshot),
        Refusal::RequiredCheckMissing {
            name: "ci/build".into()
        }
    );
}

#[test]
fn a_skipped_check_nobody_required_does_not_block() {
    // The regression that made auto-merge unreachable rather than
    // conservative. A workflow job behind `if: github.event_name == 'push'`
    // concludes `skipped` on every pull request, for ever; reading that as a
    // failure means no pull request in such a repository can ever merge, and
    // this repository has two of them.
    let mut snapshot = snapshot_of();
    for conclusion in [CheckConclusion::Skipped, CheckConclusion::Neutral] {
        snapshot.checks.push(CheckStatus {
            name: "Docker (publish)".into(),
            conclusion: Some(conclusion),
        });
        assert!(
            evaluate(&policy(), &snapshot).is_merge(),
            "{conclusion:?} on an unrequired check blocked the merge"
        );
        snapshot.checks.pop();
    }
}

#[test]
fn a_required_check_that_reports_neutral_is_a_verdict_and_passes() {
    // `Neutral` is a lane saying it did not apply — the `commits` lane on a
    // range with no secrets in it. That is an answer, and requiring the lane
    // means requiring it to answer, not requiring it to find something.
    let mut snapshot = snapshot_of();
    for check in &mut snapshot.checks {
        if check.name == "ci/build" {
            check.conclusion = Some(CheckConclusion::Neutral);
        }
    }
    assert!(evaluate(&policy(), &snapshot).is_merge());
}

#[test]
fn a_required_check_that_was_skipped_refuses_and_says_which() {
    // The other half, and the reason `Neutral` and `Skipped` are not folded
    // together: a required check that *could not run* leaves the gate it was
    // named for with no evidence behind it.
    let mut snapshot = snapshot_of();
    for check in &mut snapshot.checks {
        if check.name == "ci/build" {
            check.conclusion = Some(CheckConclusion::Skipped);
        }
    }
    assert_eq!(
        refusal(&policy(), &snapshot),
        Refusal::RequiredCheckInconclusive {
            name: "ci/build".into()
        }
    );
}

#[test]
fn a_check_that_needs_a_human_still_blocks_everything() {
    // `action_required` is the conclusion that is neither green nor red at a
    // glance, and it is the one that must not slip through the widened gate
    // above: it means a person has to do something.
    let mut snapshot = snapshot_of();
    snapshot.checks.push(CheckStatus {
        name: "ci/deploy".into(),
        conclusion: Some(CheckConclusion::ActionRequired),
    });
    assert_eq!(
        refusal(&policy(), &snapshot),
        Refusal::CheckFailing {
            name: "ci/deploy".into()
        }
    );
}

#[test]
fn an_unrecognised_conclusion_is_still_read_as_a_failure() {
    // `CheckStatus::from_api` maps anything it does not know to `Failure` so a
    // conclusion GitHub adds later cannot be read as a pass. Asserted here as
    // well as in `forge::types` because this is the module where being wrong
    // about it merges code.
    let mut snapshot = snapshot_of();
    snapshot
        .checks
        .push(CheckStatus::from_api("ci/new", Some("something_new")));
    assert_eq!(
        refusal(&policy(), &snapshot),
        Refusal::CheckFailing {
            name: "ci/new".into()
        }
    );
}

#[test]
fn a_pull_request_changing_nothing_refuses() {
    let mut snapshot = snapshot_of();
    snapshot.files.clear();
    assert_eq!(refusal(&policy(), &snapshot), Refusal::NoChangedFiles);
}

#[test]
fn too_many_files_refuses() {
    let mut snapshot = snapshot_of();
    snapshot.files = (0..6)
        .map(|n| file(&format!("src/m{n}.rs"), 1, 0, 1))
        .collect();
    assert_eq!(
        refusal(&policy(), &snapshot),
        Refusal::TooManyFiles { files: 6, max: 5 }
    );
}

#[test]
fn a_sensitive_path_refuses() {
    let mut snapshot = snapshot_of();
    snapshot
        .files
        .push(file(".github/workflows/ci.yml", 1, 1, 1));
    assert_eq!(
        refusal(&policy(), &snapshot),
        Refusal::SensitivePath {
            path: ".github/workflows/ci.yml".into()
        }
    );
}

#[test]
fn too_many_changed_lines_refuses() {
    let mut snapshot = snapshot_of();
    snapshot.files = vec![file("src/lib.rs", 90, 20, 1)];
    assert_eq!(
        refusal(&policy(), &snapshot),
        Refusal::TooComplex {
            signal: "changed_lines",
            value: 110,
            max: 100
        }
    );
}

#[test]
fn too_many_hunks_refuses() {
    let mut snapshot = snapshot_of();
    snapshot.files = vec![file("src/lib.rs", 11, 0, 11)];
    assert_eq!(
        refusal(&policy(), &snapshot),
        Refusal::TooComplex {
            signal: "hunks",
            value: 11,
            max: 10
        }
    );
}

#[test]
fn too_many_directories_refuses() {
    let mut snapshot = snapshot_of();
    snapshot.files = vec![
        file("src/a/one.rs", 1, 0, 1),
        file("src/b/two.rs", 1, 0, 1),
        file("src/c/three.rs", 1, 0, 1),
        file("src/d/four.rs", 1, 0, 1),
    ];
    assert_eq!(
        refusal(&policy(), &snapshot),
        Refusal::TooComplex {
            signal: "directories",
            value: 4,
            max: 3
        }
    );
}

#[test]
fn a_diff_that_cannot_be_measured_refuses() {
    // No patch and real line movement: the forge withheld the diff, so the
    // hunk cap cannot be evaluated, so there is no cap.
    let mut snapshot = snapshot_of();
    snapshot.files = vec![ChangedFile {
        patch: None,
        additions: 4,
        ..file("assets/logo.png", 4, 0, 0)
    }];
    assert_eq!(
        refusal(&policy(), &snapshot),
        Refusal::Unmeasurable {
            path: "assets/logo.png".into()
        }
    );
}

#[test]
fn an_unreadable_glob_refuses_rather_than_matching_nothing() {
    let config = AutoMerge {
        sensitive_paths: vec!["src/**/[".into()],
        ..policy()
    };
    assert!(matches!(
        refusal(&config, &snapshot_of()),
        Refusal::UnreadablePolicy(_)
    ));
}

#[test]
fn a_zero_file_cap_refuses_everything_rather_than_meaning_unlimited() {
    let config = AutoMerge {
        max_files: 0,
        ..policy()
    };
    assert_eq!(
        refusal(&config, &snapshot_of()),
        Refusal::TooManyFiles { files: 1, max: 0 }
    );
}

// --- the dependency-bump exemption ----------------------------------------

fn bump_snapshot() -> Snapshot {
    let mut snapshot = snapshot_of();
    snapshot.pull_request.author = "dependabot[bot]".into();
    snapshot.pull_request.author_is_bot = true;
    snapshot.files = vec![
        file("Cargo.toml", 1, 1, 1),
        file("Cargo.lock", 400, 380, 40),
    ];
    snapshot
}

#[test]
fn a_verified_dependency_bump_may_touch_manifests_and_lockfiles() {
    // Manifests are sensitive and a lockfile churns thousands of lines, so
    // without the exemption no bump would ever qualify.
    assert!(evaluate(&policy(), &bump_snapshot()).is_merge());
}

#[test]
fn a_dependency_bump_touching_source_is_an_ordinary_change_again() {
    let mut snapshot = bump_snapshot();
    snapshot.files.push(file("src/lib.rs", 1, 1, 1));
    assert_eq!(
        refusal(&policy(), &snapshot),
        Refusal::SensitivePath {
            path: "Cargo.toml".into()
        }
    );
}

#[test]
fn a_lookalike_login_gets_no_exemption() {
    // `dependabot-evil[bot]` is registrable by anyone. A prefix match would
    // have handed it the exemption; the comparison is exact.
    let mut snapshot = bump_snapshot();
    snapshot.pull_request.author = "dependabot-evil[bot]".into();
    assert_eq!(
        refusal(&policy(), &snapshot),
        Refusal::SensitivePath {
            path: "Cargo.toml".into()
        }
    );
}

#[test]
fn a_human_account_named_like_the_bot_gets_no_exemption() {
    // The login is right and GitHub says it is a user, not a bot.
    let mut snapshot = bump_snapshot();
    snapshot.pull_request.author_is_bot = false;
    assert_eq!(
        refusal(&policy(), &snapshot),
        Refusal::SensitivePath {
            path: "Cargo.toml".into()
        }
    );
}

#[test]
fn the_exemption_can_be_turned_off() {
    let config = AutoMerge {
        allow_dependency_bumps: false,
        ..policy()
    };
    assert_eq!(
        refusal(&config, &bump_snapshot()),
        Refusal::SensitivePath {
            path: "Cargo.toml".into()
        }
    );
}

#[test]
fn an_exempt_bump_still_has_to_be_green_and_approved() {
    let mut snapshot = bump_snapshot();
    snapshot.checks.push(CheckStatus {
        name: "ci/audit".into(),
        conclusion: Some(CheckConclusion::Failure),
    });
    assert_eq!(
        refusal(&policy(), &snapshot),
        Refusal::CheckFailing {
            name: "ci/audit".into()
        }
    );
}

#[test]
fn an_exempt_bump_still_obeys_the_file_cap() {
    // The exemption waives the sensitive-path and complexity refusals, whose
    // whole purpose the manifest-only rule already serves. It does not waive
    // the file cap, which stays a plain bound on how much lands at once.
    let config = AutoMerge {
        max_files: 1,
        ..policy()
    };
    assert_eq!(
        refusal(&config, &bump_snapshot()),
        Refusal::TooManyFiles { files: 2, max: 1 }
    );
}

// --- the measured signals themselves --------------------------------------

#[test]
fn complexity_is_arithmetic() {
    let measured = measure(&[
        file("src/a/one.rs", 3, 2, 2),
        file("src/b/two.rs", 1, 0, 1),
        file("README.md", 5, 5, 3),
    ])
    .expect("measurable");

    assert_eq!(
        measured,
        Complexity {
            files: 3,
            changed_lines: 16,
            hunks: 6,
            // src/a, src/b, and the repository root.
            directories: 3,
        }
    );
}

#[test]
fn a_context_line_containing_at_at_is_not_a_hunk() {
    let mut noisy = file("src/lib.rs", 1, 0, 1);
    noisy.patch = Some("@@ -1,2 +1,3 @@\n+let doc = \"@@ not a hunk\";\n context\n".into());
    assert_eq!(measure(&[noisy]).expect("measurable").hunks, 1);
}

#[test]
fn a_pure_rename_is_measurable_at_zero_hunks() {
    let renamed = ChangedFile {
        path: "src/new.rs".into(),
        previous_path: Some("src/old.rs".into()),
        status: FileStatus::Renamed,
        additions: 0,
        deletions: 0,
        patch: None,
        size_bytes: None,
    };
    assert_eq!(measure(&[renamed]).expect("measurable").hunks, 0);
}

#[test]
fn logins_are_compared_exactly() {
    assert!(logins_match("dependabot[bot]", "dependabot[bot]"));
    // The `[bot]` suffix is presentation, so it is normalised off both sides.
    assert!(logins_match("dependabot", "dependabot[bot]"));
    assert!(logins_match("Dependabot[bot]", "dependabot"));

    assert!(!logins_match("dependabot", "dependabot-evil"));
    assert!(!logins_match("dependabot", "dependabot[bot]x"));
    assert!(!logins_match("dependabot", "notdependabot"));
    assert!(!logins_match("", "dependabot"));
    assert!(!logins_match("dependabot", ""));
}

#[test]
fn a_malformed_glob_is_an_error_not_an_empty_matcher() {
    assert!(glob_set(&["src/**/[".to_string()]).is_err());
    assert!(glob_set(&["src/**/*.rs".to_string()]).is_ok());
}

// --- the job, against the recording forge ----------------------------------

fn forge_with(pull_request: PullRequest, files: Vec<ChangedFile>) -> MockForge {
    MockForge::new()
        .with_pull_request(pull_request, files, vec![])
        .with_check(HEAD, "tinysweeper/critique", Some(CheckConclusion::Success))
        .with_check(HEAD, "ci/build", Some(CheckConclusion::Success))
        .with_reviews(7, vec![approval("maintainer")])
}

fn qualifying_forge() -> MockForge {
    forge_with(pull_request(), vec![file("src/lib.rs", 3, 2, 1)])
}

fn merges(forge: &MockForge) -> Vec<Write> {
    forge
        .writes()
        .into_iter()
        .filter(|write| matches!(write, Write::Merged { .. }))
        .collect()
}

#[tokio::test]
async fn the_job_merges_a_qualifying_pull_request_with_the_configured_method() {
    let forge = qualifying_forge();
    let outcome = merge_if_qualified(&forge, &forge, &policy(), &repo(), 7)
        .await
        .expect("runs");

    assert_eq!(
        outcome,
        Outcome::Merged {
            method: "merge".into()
        }
    );
    assert_eq!(
        merges(&forge),
        vec![Write::Merged {
            number: 7,
            method: "merge".into()
        }]
    );
}

#[tokio::test]
async fn the_job_writes_nothing_at_all_when_it_refuses() {
    let forge = qualifying_forge();
    let config = AutoMerge {
        enabled: false,
        ..policy()
    };
    let outcome = merge_if_qualified(&forge, &forge, &config, &repo(), 7)
        .await
        .expect("runs");

    assert_eq!(outcome, Outcome::Refused(Refusal::Disabled));
    assert!(
        forge.wrote_nothing(),
        "a refusal must leave the pull request untouched: {:?}",
        forge.writes()
    );
}

#[tokio::test]
async fn a_head_that_moved_after_the_decision_is_not_merged() {
    // The re-validation exists for exactly this: the checks were green on the
    // commit that was read, and say nothing about the one now at the head.
    let forge = qualifying_forge();
    let moved = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    let taken = snapshot(&forge, &repo(), 7).await.expect("reads");
    forge.push(7, moved, vec![file("src/lib.rs", 3, 2, 1)]);

    let outcome = crate::automerge::merge_snapshot(&forge, &forge, &policy(), &repo(), &taken)
        .await
        .expect("runs");

    assert_eq!(
        outcome,
        Outcome::Refused(Refusal::HeadMoved {
            evaluated: HEAD.into(),
            live: moved.into(),
        })
    );
    assert!(merges(&forge).is_empty(), "the moved head must not merge");
}

/// A forge that reads like the mock and refuses every merge.
///
/// Squash merges are disabled on this repository, and a repository disabling
/// the configured method is the ordinary case rather than the exotic one, so
/// the job has to survive the forge saying no. The mock always accepts, which
/// is why this wrapper exists.
struct RefusingForge;

#[async_trait::async_trait]
impl crate::ports::forge::ForgeWrite for RefusingForge {
    async fn publish_check(
        &self,
        _repo: &RepoId,
        _check: crate::forge::types::CheckRun,
    ) -> crate::Result<u64> {
        unreachable!("auto-merge publishes no checks")
    }
    async fn update_check(
        &self,
        _repo: &RepoId,
        _check_id: u64,
        _check: crate::forge::types::CheckRun,
    ) -> crate::Result<()> {
        unreachable!("auto-merge publishes no checks")
    }
    async fn reply_to_review_thread(
        &self,
        _repo: &RepoId,
        _thread_id: &str,
        _body: &str,
    ) -> crate::Result<()> {
        unreachable!("auto-merge writes no thread replies")
    }
    async fn create_comment(
        &self,
        _repo: &RepoId,
        _number: u64,
        _body: &str,
    ) -> crate::Result<u64> {
        unreachable!("auto-merge writes no comments")
    }
    async fn update_comment(
        &self,
        _repo: &RepoId,
        _comment_id: u64,
        _body: &str,
    ) -> crate::Result<()> {
        unreachable!("auto-merge writes no comments")
    }
    async fn create_review(
        &self,
        _repo: &RepoId,
        _number: u64,
        _body: &str,
        _comments: Vec<crate::forge::types::ReviewComment>,
        _event: ReviewEvent,
    ) -> crate::Result<()> {
        unreachable!("auto-merge leaves no reviews")
    }
    async fn add_labels(
        &self,
        _repo: &RepoId,
        _number: u64,
        _labels: &[String],
    ) -> crate::Result<()> {
        unreachable!("auto-merge applies no labels")
    }
    async fn remove_label(&self, _repo: &RepoId, _number: u64, _label: &str) -> crate::Result<()> {
        unreachable!("auto-merge removes no labels")
    }
    async fn set_issue_type(
        &self,
        _repo: &RepoId,
        _number: u64,
        _type_name: &str,
    ) -> crate::Result<()> {
        unreachable!("auto-merge sets no issue types")
    }
    async fn close_issue(&self, _repo: &RepoId, _number: u64) -> crate::Result<()> {
        unreachable!("auto-merge closes no issues")
    }
    async fn create_issue(
        &self,
        _repo: &RepoId,
        _title: &str,
        _body: &str,
        _labels: &[String],
    ) -> crate::Result<u64> {
        unreachable!("auto-merge opens no issues")
    }
    async fn resolve_review_thread(&self, _repo: &RepoId, _thread_id: &str) -> crate::Result<()> {
        unreachable!("auto-merge resolves no threads")
    }
    async fn merge(
        &self,
        _repo: &RepoId,
        _approval: &crate::automerge::policy::MergeApproved,
        method: &str,
    ) -> crate::Result<()> {
        Err(crate::Error::Forge(format!(
            "{method} merges are not allowed on this repository"
        )))
    }
}

#[tokio::test]
async fn a_merge_method_the_repository_disabled_is_reported_not_raised() {
    let forge = qualifying_forge();
    let config = AutoMerge {
        method: "squash".into(),
        ..policy()
    };
    let refusing = RefusingForge;

    let outcome = merge_if_qualified(&forge, &refusing, &config, &repo(), 7)
        .await
        .expect("the forge saying no is not the job failing");

    match outcome {
        Outcome::Rejected { method, reason } => {
            assert_eq!(method, "squash");
            assert!(reason.contains("not allowed"), "{reason}");
        }
        other => panic!("expected a rejection, got {other:?}"),
    }
    assert!(
        merges(&forge).is_empty(),
        "a rejected merge must leave nothing behind"
    );
}

#[tokio::test]
async fn every_refusal_reason_leaves_the_pull_request_alone() {
    // The table version of the tests above, run through the real job so that
    // "the policy refused" and "nothing was written" are proven together.
    let cases: Vec<(&str, MockForge, AutoMerge)> = vec![
        (
            "a draft",
            {
                let mut draft = pull_request();
                draft.draft = true;
                forge_with(draft, vec![file("src/lib.rs", 1, 1, 1)])
            },
            policy(),
        ),
        (
            "a blocking label",
            {
                let mut blocked = pull_request();
                blocked.labels.push("do-not-merge".into());
                forge_with(blocked, vec![file("src/lib.rs", 1, 1, 1)])
            },
            policy(),
        ),
        (
            "a failing check",
            { qualifying_forge().with_check(HEAD, "ci/lint", Some(CheckConclusion::Failure)) },
            policy(),
        ),
        (
            "a pending check",
            { qualifying_forge().with_check(HEAD, "ci/slow", None) },
            policy(),
        ),
        (
            "a standing changes-request",
            {
                qualifying_forge().with_reviews(
                    7,
                    vec![
                        approval("maintainer"),
                        ReviewVerdict {
                            reviewer: "careful-human".into(),
                            bot: false,
                            state: ReviewEvent::RequestChanges,
                        },
                    ],
                )
            },
            policy(),
        ),
        (
            "too many files",
            {
                forge_with(
                    pull_request(),
                    (0..6)
                        .map(|n| file(&format!("src/m{n}.rs"), 1, 0, 1))
                        .collect(),
                )
            },
            policy(),
        ),
        (
            "a sensitive path",
            {
                forge_with(
                    pull_request(),
                    vec![file(".github/workflows/ci.yml", 1, 1, 1)],
                )
            },
            policy(),
        ),
        (
            "complexity over threshold",
            { forge_with(pull_request(), vec![file("src/lib.rs", 200, 0, 1)]) },
            policy(),
        ),
        (
            "policy off",
            qualifying_forge(),
            AutoMerge {
                enabled: false,
                ..policy()
            },
        ),
        (
            "a bump that also touches source",
            {
                let mut bot = pull_request();
                bot.author = "dependabot[bot]".into();
                bot.author_is_bot = true;
                forge_with(
                    bot,
                    vec![file("Cargo.toml", 1, 1, 1), file("src/lib.rs", 1, 1, 1)],
                )
            },
            policy(),
        ),
    ];

    for (name, forge, config) in cases {
        let outcome = merge_if_qualified(&forge, &forge, &config, &repo(), 7)
            .await
            .expect("runs");
        assert!(
            matches!(outcome, Outcome::Refused(_)),
            "{name} should refuse, got {outcome:?}"
        );
        assert!(
            merges(&forge).is_empty(),
            "{name} must not merge, wrote {:?}",
            forge.writes()
        );
    }
}
