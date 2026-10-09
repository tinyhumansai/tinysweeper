//! Concern-identity tests, against findings tinysweeper actually posted.
//!
//! Every fixture below is a real comment from October 2026, copied with its
//! lane, file, line, rule and wording, from the pull requests where the same
//! concern was posted over and over: `openhuman#7129`, `openhuman#7079`,
//! `openhuman#7127`, `tinyagents#341` and `tinyskills#24`. The thresholds in
//! `concern.rs` were tuned on these, so a change that breaks one of these
//! tests is a change to what a maintainer sees, not to an implementation
//! detail.
//!
//! The negative cases matter as much as the positive ones: each is two
//! genuinely different concerns, close together, that both had to survive.

use super::*;

use crate::config::types::{LaneId, Severity};
use crate::findings::types::Finding;

/// One posted comment, reduced to what concern identity reads.
struct Posted {
    lane: LaneId,
    path: &'static str,
    line: u64,
    rule: &'static str,
    title: &'static str,
    body: &'static str,
}

impl Posted {
    fn finding(&self) -> Finding {
        Finding {
            lane: self.lane,
            severity: Severity::Medium,
            confidence: 0.8,
            path: self.path.into(),
            line: Some(self.line),
            end_line: None,
            rule: self.rule.into(),
            title: self.title.into(),
            body: self.body.into(),
            suggestion: None,
            applicable: None,
            late: false,
            identity: None,
            aliases: Vec::new(),
            grouped: false,
            review_pass: 1,
            corroboration: 1,
        }
    }

    fn concern(&self) -> Concern {
        Concern::of(&self.finding())
    }
}

fn same(a: &Posted, b: &Posted) -> bool {
    // Symmetric by construction; asserting both directions keeps it that way.
    let forward = a.concern().same_as(&b.concern());
    let backward = b.concern().same_as(&a.concern());
    assert_eq!(forward, backward, "{} / {}", a.title, b.title);
    forward
}

// ---------------------------------------------------------------------------
// openhuman#7129 — one concern posted 8 times, by three lanes, in five files.
// ---------------------------------------------------------------------------

const OH7129_TESTS_155: Posted = Posted {
    lane: LaneId::Tests,
    path: "crates/openhuman-core/src/config/schema/memory.rs",
    line: 155,
    rule: "untested-invariant",
    title: "Test the write-again-without-actor fallback the change promises",
    body: "The doc comment on `observed_actor` (and the matching paragraph in `docs/specs/memory-v2.md`) states that a write CortexDB refuses because of the actor is retried without it. Nothing in the diff exercises that path: the engine tests only check that toggling the setting rebuilds the engine, and the composio tests only check meta mapping. If the retry regresses or was never wired, users' writes would fail silently while the docs promise they succeed. Add a test with a stub/failing endpoint that refuses actor-attributed writes and asserts the record still lands without the actor.",
};

const OH7129_CRITIQUE_152: Posted = Posted {
    lane: LaneId::Critique,
    path: "crates/openhuman-core/src/config/schema/memory.rs",
    line: 152,
    rule: "missing-contract-test",
    title: "Test the write-again-without-actor fallback the change promises",
    body: "The new test only checks deserialization and omission of the default field; it does not exercise the behavior claimed here when CortexDB refuses a write containing `observed_actor`. A refusal path could still lose the write or surface an error instead of retrying without the actor. Add a test that makes the first write fail for this reason and verifies that the retry succeeds without `observed_actor`.",
};

const OH7129_SECURITY_154: Posted = Posted {
    lane: LaneId::Security,
    path: "crates/openhuman-core/src/config/schema/memory.rs",
    line: 154,
    rule: "missing-regression-test",
    title: "Test the write-again-without-actor fallback the change promises",
    body: "The added test only verifies deserialization and serialization of `observed_actor`; it does not exercise the stated behavior where a CortexDB write rejected for `observed_actor` is retried without it. Add a unit test covering both the rejected first write and the fallback write, including the case where no actor is available.",
};

const OH7129_SIBLING_TEST_260: Posted = Posted {
    lane: LaneId::Security,
    path: "crates/openhuman-core/src/config/schema/memory_tests.rs",
    line: 260,
    rule: "missing-behavior-test",
    title: "Test the write-again-without-actor fallback",
    body: "This test verifies only the default, serialization omission, and TOML parsing. It does not exercise the documented behavior that a CortexDB write rejected because of `observed_actor` is retried without that attribute, so a regression could silently drop writes or fail the operation despite the configuration contract. Add a unit test covering the rejected write and the retry payload without `observed_actor`.",
};

const OH7129_COMPOSIO_TESTS_541: Posted = Posted {
    lane: LaneId::Critique,
    path: "crates/openhuman-core/src/memory/sources/composio_tests.rs",
    line: 541,
    rule: "missing-regression-test",
    title: "Exercise the write-again-without-actor fallback",
    body: "This only verifies that a record with no sender produces no actor on its first conversion. It does not cover the promised fallback when the same item is written once with an actor and then written again without one, so a regression could leave the previous actor attached or otherwise mishandle the replacement. Add a test that performs both writes through the relevant storage path and asserts the second version has no observed actor.",
};

const OH7129_COMPOSIO_102: Posted = Posted {
    lane: LaneId::Security,
    path: "crates/openhuman-core/src/memory/sources/composio.rs",
    line: 102,
    rule: "missing-test",
    title: "Test the write-again-without-actor fallback the change promises",
    body: "The tests only build each `StoreItem` independently. They do not write a record with an actor and then write the same record without a sender, so they cannot verify that the second write clears or otherwise safely handles the previously stored actor rather than retaining stale metadata. Add an integration-style unit test covering that update sequence and its fallback behavior.",
};

const OH7129_ENGINE_412: Posted = Posted {
    lane: LaneId::Critique,
    path: "crates/openhuman-core/src/memory/engine.rs",
    line: 412,
    rule: "missing-regression-test",
    title: "Test the write-again-without-actor fallback the change promises",
    body: "The earlier concern still stands: this change adds cache separation for `observed_actor`, but the accompanying test only switches the setting while resolving a CortexDB engine. It does not exercise the write-again-without-actor fallback, so a regression in the promised fallback behavior would still pass. Add a test that performs the relevant write twice without an actor and asserts the second write uses the intended fallback behavior.",
};

#[test]
fn three_lanes_saying_one_thing_near_one_line_are_one_concern() {
    assert!(same(&OH7129_TESTS_155, &OH7129_CRITIQUE_152));
    assert!(same(&OH7129_TESTS_155, &OH7129_SECURITY_154));
    assert!(same(&OH7129_CRITIQUE_152, &OH7129_SECURITY_154));
}

#[test]
fn the_same_concern_in_the_sibling_test_file_is_a_repeat() {
    // `memory.rs` and `memory_tests.rs`, 100 lines apart in different files:
    // the shortened title is wholly contained in the longer one.
    assert!(same(&OH7129_TESTS_155, &OH7129_SIBLING_TEST_260));
    // `composio.rs` and `composio_tests.rs`, with "exercise" for "test".
    assert!(same(&OH7129_COMPOSIO_TESTS_541, &OH7129_COMPOSIO_102));
}

#[test]
fn the_same_title_in_an_unrelated_file_is_not_merged() {
    // Deliberate. An identical title in a file that is neither this one nor
    // its test sibling is as often a second real site as a repeat: on
    // `openhuman#7127` "Avoid logging the full module error" was raised on
    // `browser_task.rs` and again on `browser_sites.rs`, and both logged.
    assert!(!same(&OH7129_TESTS_155, &OH7129_ENGINE_412));
}

// ---------------------------------------------------------------------------
// openhuman#7079 — "visible before click" ×3 and "element-helpers" ×4.
// ---------------------------------------------------------------------------

const OH7079_VISIBLE_23: Posted = Posted {
    lane: LaneId::Security,
    path: "app/test/e2e/specs/onboarding-modes.spec.ts",
    line: 23,
    rule: "e2e-interaction-validity",
    title: "Require visible elements before clicking",
    body: "This helper now treats any non-disabled matching element as clickable and invokes `element.click()` without checking visibility, layout, or whether the element is actually interactable. A hidden or stale duplicate element can therefore advance the flow while the user-facing control is unavailable, allowing the E2E test to pass falsely. Restore an interaction check or use the repository's element helper so clicks only succeed on visible, interactable controls.",
};

const OH7079_VISIBLE_25: Posted = Posted {
    lane: LaneId::Tests,
    path: "app/test/e2e/specs/onboarding-modes.spec.ts",
    line: 25,
    rule: "click-without-visibility",
    title: "Require visible elements before clicking",
    body: "The click helper only checks that the element exists and is not disabled; a zero-size or hidden element still reports a successful click, so `clickTestId` can return true while the click never reached an interactive control. The previous revision checked the bounding rect; that guard was dropped in the rewrite.",
};

const OH7079_VISIBLE_24: Posted = Posted {
    lane: LaneId::E2e,
    path: "app/test/e2e/specs/onboarding-modes.spec.ts",
    line: 24,
    rule: "invisible-click",
    title: "Require visible elements before clicking",
    body: "The previous version of this helper checked getBoundingClientRect for a zero-size box before dispatching the click; the rewrite dropped that check, so a mounted-but-hidden control now counts as clicked and the spec can pass while the user-visible button never actually received input. Still unfixed from the prior review.",
};

const OH7079_HELPERS_24: Posted = Posted {
    lane: LaneId::Tests,
    path: "app/test/e2e/specs/onboarding-modes.spec.ts",
    line: 24,
    rule: "e2e-raw-element-types",
    title: "Use element-helpers instead of raw platform element types in E2E specs",
    body: "The rewritten spec introduces a local `clickTestId` helper that does `browser.execute` with `document.querySelector<HTMLElement>` and `HTMLButtonElement` casts directly. The repository rule states E2E code must use `app/test/e2e/helpers/element-helpers.ts`, not raw platform element types. The same new helper is duplicated in runtime-picker-login.spec.ts. Move the click/existence helpers into the shared element-helpers module (or use existing helpers there) so the platform handling stays in one place.",
};

const OH7079_HELPERS_23: Posted = Posted {
    lane: LaneId::Tests,
    path: "app/test/e2e/specs/onboarding-modes.spec.ts",
    line: 23,
    rule: "raw-platform-elements",
    title: "Use the shared element helpers for E2E interactions",
    body: "The repository rule requires E2E code to use `app/test/e2e/helpers/element-helpers.ts`, not raw platform element types. The rewritten specs reintroduce local `clickTestId`/`hasTestId` helpers built on `browser.execute` and `document.querySelector` rather than the shared helpers, in three files. This was raised before and still stands.",
};

#[test]
fn an_identical_title_from_another_lane_a_line_away_is_one_concern() {
    assert!(same(&OH7079_VISIBLE_23, &OH7079_VISIBLE_25));
    assert!(same(&OH7079_VISIBLE_23, &OH7079_VISIBLE_24));
    assert!(same(&OH7079_VISIBLE_25, &OH7079_VISIBLE_24));
}

#[test]
fn a_reworded_title_with_a_synonym_rule_is_one_concern() {
    // "the shared element helpers" / "element-helpers instead of raw
    // platform element types", rules `raw-platform-elements` and
    // `e2e-raw-element-types`.
    assert!(same(&OH7079_HELPERS_23, &OH7079_HELPERS_24));
}

#[test]
fn two_concerns_on_one_helper_both_survive() {
    // Both on the same `clickTestId` helper, on the same line, from the same
    // lane family. One is about visibility, one about which helper module to
    // use — fixing either leaves the other standing.
    assert!(!same(&OH7079_VISIBLE_24, &OH7079_HELPERS_24));
    assert!(!same(&OH7079_VISIBLE_23, &OH7079_HELPERS_23));
}

// ---------------------------------------------------------------------------
// tinyagents#341 — `resource-budget`, `budget-bound`, `unbounded-budget`.
// ---------------------------------------------------------------------------

const TA341_RESOURCE_BUDGET_350: Posted = Posted {
    lane: LaneId::Critique,
    path: "crates/tinyagents-harness/src/agent_loop/response_recovery.rs",
    line: 350,
    rule: "resource-budget",
    title: "Do not return a cap that exceeds the clock budget",
    body: "When a retry is skipped because it is unaffordable, the code first stores `plan.affordable_cap()` in `boosted_max_tokens`, but this later assignment overwrites it with `plan.halved_cap()`. For example, with a current cap of 16,000 tokens and only enough remaining time for 3,000 tokens, `affordable_cap()` is 3,000 while `halved_cap()` is 8,000; the next call is therefore scheduled above the clock budget. Preserve the affordable cap when it exists instead of unconditionally replacing it with the halved cap.",
};

const TA341_BUDGET_BOUND_329: Posted = Posted {
    lane: LaneId::Critique,
    path: "crates/tinyagents-harness/src/agent_loop/response_recovery.rs",
    line: 329,
    rule: "budget-bound",
    title: "Do not let the minimum cap exceed the affordable budget",
    body: "The policy-nudge branch is allowed solely because the policy counter has room, without requiring `another_nudge_fits()`. With a current cap of 4,096 and a low remaining clock where the 2,048-token halved cap would exceed half the remaining budget, the condition is still true while the policy nudge count is below its limit, and the subsequent call is scheduled at 2,048 tokens. Require the clock affordability check whenever a clock is present, while retaining the policy-only behavior when no clock is configured.",
};

const TA341_UNBOUNDED_BUDGET_139: Posted = Posted {
    lane: LaneId::Security,
    path: "crates/tinyagents-harness/src/agent_loop/turn_recovery.rs",
    line: 139,
    rule: "unbounded-budget",
    title: "Bound nudges by the affordable cap",
    body: "`halved_cap` always applies the 2048-token floor, even when the remaining clock can afford fewer tokens. `response_recovery` checks `another_nudge_fits()` only for nudges allowed beyond the configured nudge budget; ordinary policy-authorized nudges still use this cap without that check. Once `affordable_cap()` returns `None`, a later nudge can therefore schedule a 2048-token call that is expected to exceed the remaining wall-clock budget. Make the nudge cap honor the same affordable budget, or reject the nudge whenever its minimum cap cannot fit.",
};

const TA341_UNBOUNDED_RETRY_330: Posted = Posted {
    lane: LaneId::Critique,
    path: "crates/tinyagents-harness/src/agent_loop/response_recovery.rs",
    line: 330,
    rule: "unbounded-retry",
    title: "Bound clock-only nudges before scheduling them",
    body: "When the policy nudge budget is exhausted, `clock_allows_another_nudge` admits an additional nudge based only on the estimated duration of `plan.halved_cap()`. On the first such clock-only nudge, `repeat_cap` is still `None` because it is only computed when `truncated_empty_nudges_used > 1`, so the request receives no reduced output cap. This is especially reachable when `truncated_empty_nudges` is zero: a short truncated call with a configured wall-clock budget can trigger an extra unbounded retry. Ensure every clock-only nudge applies a concrete bounded cap, or do not allow it when no such cap can be established.",
};

#[test]
fn synonym_rule_names_collapse_on_their_shared_token() {
    assert!(rules_agree("resource-budget", "budget-bound"));
    assert!(rules_agree("budget-bound", "unbounded-budget"));
    assert!(rules_agree("unbounded-budget", "resource-budget"));
    // `bound` is a category word every limit-shaped rule carries; sharing it
    // says nothing about which limit.
    assert!(!rules_agree("unbounded-retry", "budget-bound"));
    assert!(!rules_agree("", ""));
}

#[test]
fn the_budget_concern_under_three_rule_names_is_one_concern() {
    assert!(same(&TA341_RESOURCE_BUDGET_350, &TA341_BUDGET_BOUND_329));
}

#[test]
fn the_budget_concern_in_another_module_is_left_alone() {
    // `turn_recovery.rs` is not `response_recovery.rs` and not its test file.
    // The third rule name is still recognised as a synonym — the location is
    // what keeps these apart.
    assert!(!same(
        &TA341_UNBOUNDED_BUDGET_139,
        &TA341_RESOURCE_BUDGET_350
    ));
}

#[test]
fn a_different_concern_one_line_away_survives() {
    // Line 329 is about the policy-nudge cap, line 330 about clock-only
    // nudges. Adjacent, same lane, same function, different fixes.
    assert!(!same(&TA341_BUDGET_BOUND_329, &TA341_UNBOUNDED_RETRY_330));
}

// ---------------------------------------------------------------------------
// openhuman#7127 — `[INVALID_FLOW]` from critique and security.
// ---------------------------------------------------------------------------

const OH7127_CRITIQUE_INVALID_FLOW: Posted = Posted {
    lane: LaneId::Critique,
    path: "crates/openhuman-core/src/modules/browser_task.rs",
    line: 271,
    rule: "structured-error-matching",
    title: "Match the structured flow error code exactly",
    body: "This treats any error containing `[INVALID_FLOW]` as a rejected saved flow, including an unrelated module error whose message or hint contains that text. For example, an error with code `PLANNER_UNAVAILABLE` and message `[INVALID_FLOW]` will cause the task to be retried without its flow, potentially hiding the original failure and issuing a duplicate `StartTask`. Match the structured error-code prefix emitted by `unwrap_response` rather than searching the entire formatted error.",
};

/// The security lane's version of the same finding, as it read in the
/// grouped thread on `openhuman#7127`.
const OH7127_SECURITY_INVALID_FLOW: Posted = Posted {
    lane: LaneId::Security,
    path: "crates/openhuman-core/src/modules/browser_task.rs",
    line: 273,
    rule: "error-code-spoofing",
    title: "Do not treat any error mentioning [INVALID_FLOW] as a refused flow",
    body: "The refusal check searches the whole formatted error for `[INVALID_FLOW]`, so a module error whose message or hint merely contains that text is treated as a rejected saved flow and the task is retried without it, hiding the original failure. Compare against the structured error code that `unwrap_response` emits instead of the entire message.",
};

const OH7127_TERMINAL_341: Posted = Posted {
    lane: LaneId::Critique,
    path: "crates/openhuman-core/src/modules/browser_sites.rs",
    line: 341,
    rule: "stale-task-identity",
    title: "Guard against terminal reports for a reused task ID",
    body: "`TaskId` is reused when the module is set up again, and `follow` only keys entries by that ID. If a terminal view from an earlier task arrives after a new task with the same ID has been followed, `followed_as` returns the new task's `Following`; this line then removes the new task, and the old report is learned under the new task's site, goal, and facts. Preserve a generation or task-instance identity alongside the ID, or otherwise reject stale terminal views before stopping and learning.",
};

const OH7127_LEARNING_360: Posted = Posted {
    lane: LaneId::Critique,
    path: "crates/openhuman-core/src/modules/browser_sites.rs",
    line: 360,
    rule: "stale-configuration-check",
    title: "Recheck learning before persisting a fetched report",
    body: "This check occurs before the asynchronous `fetch(view.id).await`. If `note_switch(workspace, false)` runs while the report is being fetched, the code continues into `update` and saves the report's hints and plan even though learning has been turned off. Recheck the current learning state immediately before changing and saving site memory, and discard the followed task without learning when it has been disabled during the fetch.",
};

#[test]
fn the_cross_lane_invalid_flow_finding_is_one_concern() {
    assert!(same(
        &OH7127_CRITIQUE_INVALID_FLOW,
        &OH7127_SECURITY_INVALID_FLOW
    ));
}

#[test]
fn two_concerns_in_one_function_both_survive() {
    // Nineteen lines apart in the same terminal-view handler, and they share
    // the rule token `stale`. One is task identity, one is the learning
    // switch.
    assert!(!same(&OH7127_TERMINAL_341, &OH7127_LEARNING_360));
}

// ---------------------------------------------------------------------------
// tinyskills#24 — "free of live network sockets" ×6, and a decline.
// ---------------------------------------------------------------------------

const TS24_SOCKETS_58: Posted = Posted {
    lane: LaneId::Security,
    path: "crates/tinyskills/tests/registry_loopback.rs",
    line: 58,
    rule: "network-in-tests",
    title: "Keep integration tests free of live network sockets",
    body: "This integration test starts a real TCP listener and exercises the registry over a loopback socket. The repository requires tests to avoid network access so they remain deterministic and reliable across environments. Use a deterministic in-memory `RegistryTransport` test double for these cases, or move socket-level transport testing outside the deterministic test suite.",
};

const TS24_TRANSPORT_58: Posted = Posted {
    lane: LaneId::Security,
    path: "crates/tinyskills/tests/registry_loopback.rs",
    line: 58,
    rule: "live-network-test",
    title: "Use a deterministic transport instead of loopback sockets",
    body: "The test starts a real TCP listener and exercises the registry over a live socket. Repository rules require deterministic tests without network access; this can fail due to port allocation, scheduling, or environment restrictions. Use an in-memory `RegistryTransport` test double for these scenarios.",
};

const TS24_TRANSPORT_13: Posted = Posted {
    lane: LaneId::Security,
    path: "crates/tinyskills/tests/registry_loopback.rs",
    line: 13,
    rule: "live-network-test",
    title: "Use a deterministic transport instead of loopback sockets",
    body: "The test starts a real TCP listener and exercises network I/O, but repository rules require deterministic tests with no network access. Test the public API with an in-process `RegistryTransport` implementation instead of `Server` and `SocketTransport`.",
};

#[test]
fn a_reworded_title_on_the_same_line_is_one_concern() {
    // The titles share one word. The explanations are the same paragraph.
    assert!(same(&TS24_SOCKETS_58, &TS24_TRANSPORT_58));
}

#[test]
fn a_declined_concern_reworded_and_moved_stays_declined() {
    // Forty-five lines away in the same file, reworded. Against an ordinary
    // earlier comment that is not enough evidence to stay quiet; against one
    // a maintainer already declined, it is.
    let declined = TS24_SOCKETS_58.concern();
    let reworded = TS24_TRANSPORT_13.concern();
    assert!(!reworded.same_as(&declined));
    assert!(reworded.same_as_declined(&declined));
}

#[test]
fn a_declined_concern_does_not_swallow_a_different_one() {
    let declined = OH7079_VISIBLE_24.concern();
    assert!(!OH7079_HELPERS_24.concern().same_as_declined(&declined));
    assert!(
        !TA341_UNBOUNDED_RETRY_330
            .concern()
            .same_as_declined(&TA341_BUDGET_BOUND_329.concern())
    );
}

// ---------------------------------------------------------------------------
// Normalisation.
// ---------------------------------------------------------------------------

#[test]
fn wording_normalises_case_punctuation_stopwords_and_suffixes() {
    let words = tokens("Use element-helpers instead of the RAW platform types!");
    for expected in ["element", "helper", "raw", "platform"] {
        assert!(
            words.contains(expected),
            "{expected} missing from {words:?}"
        );
    }
    for dropped in ["use", "instead", "of", "the"] {
        assert!(!words.contains(dropped), "{dropped} kept in {words:?}");
    }

    assert_eq!(tokens("exceeds"), tokens("exceeded"));
    assert_eq!(tokens("exceed"), tokens("exceeding"));
    assert_eq!(tokens("caches"), tokens("caching"));
    assert_eq!(tokens("emits"), tokens("emitting"));
    assert_eq!(tokens("Exercise"), tokens("test"));
}

#[test]
fn siblings_are_a_file_and_its_own_tests() {
    assert!(siblings(
        "src/config/memory.rs",
        "src/config/memory_tests.rs"
    ));
    assert!(siblings("src/a/composio.rs", "src/a/composio_test.rs"));
    assert!(siblings("app/src/Panel.tsx", "app/src/Panel.test.tsx"));
    assert!(siblings("crate/src/store.rs", "crate/tests/store.rs"));
    assert!(siblings("pkg/util.py", "pkg/tests/test_util.py"));

    assert!(!siblings("src/a/mod.rs", "src/b/mod.rs"));
    assert!(!siblings("src/memory/engine.rs", "src/config/memory.rs"));
    assert!(
        !siblings("src/a/foo.rs", "src/a/foo.rs"),
        "a file is not its own sibling"
    );
    assert!(
        !siblings("src/a/foo.ts", "src/a/foo.tsx"),
        "neither is a test"
    );
}

#[test]
fn an_unplaced_finding_needs_the_whole_file_bar() {
    let mut unplaced = OH7129_TESTS_155.finding();
    unplaced.line = None;
    // Identical title, no line: still the same text in the same file.
    assert!(Concern::of(&unplaced).same_as(&OH7129_CRITIQUE_152.concern()));

    // The reworded helper finding only clears the nearby bar, which an
    // unplaced finding has no position to claim.
    let mut unplaced = OH7079_HELPERS_23.finding();
    unplaced.line = None;
    assert!(!Concern::of(&unplaced).same_as(&OH7079_HELPERS_24.concern()));
}

// ---------------------------------------------------------------------------
// Polarity and placement boundaries.
// ---------------------------------------------------------------------------

#[test]
fn opposite_guidance_on_the_same_wording_is_not_a_repeat() {
    // The same content words, opposite instructions. `Concern` used to drop
    // "not" as a stopword, so these matched at similarity 1.0 and the new,
    // contradictory finding was suppressed as already raised.
    let allow = Concern::new(
        "src/parser.rs",
        Some((10, 10)),
        "Allow empty values in the parser",
        "",
        "",
    );
    let deny = Concern::new(
        "src/parser.rs",
        Some((10, 10)),
        "Do not allow empty values in the parser",
        "",
        "",
    );
    assert!(!allow.same_as(&deny));
    assert!(!deny.same_as(&allow));
    assert!(
        !allow.same_as_declined(&deny),
        "a decline is not a reversal"
    );
}

#[test]
fn a_negated_title_still_repeats_the_same_negated_title() {
    // Polarity only separates opposites; two "do not" findings can repeat.
    let first = Concern::new(
        "src/parser.rs",
        Some((10, 10)),
        "Do not allow empty values in the parser",
        "",
        "",
    );
    let again = Concern::new(
        "src/parser.rs",
        Some((12, 12)),
        "Do not allow empty values in the parser",
        "",
        "",
    );
    assert!(first.same_as(&again));
}

#[test]
fn lines_strictly_between_two_anchors_are_counted() {
    // Lines 1 and 32 have thirty lines between them: exactly the nearby limit.
    assert_eq!(gap((1, 1), (32, 32)), NEAR_LINES);
    assert_eq!(gap((32, 32), (1, 1)), NEAR_LINES, "symmetric");
    assert_eq!(gap((1, 1), (33, 33)), NEAR_LINES + 1);
    assert_eq!(
        gap((1, 1), (2, 2)),
        0,
        "adjacent lines have nothing between"
    );
    assert_eq!(gap((5, 9), (7, 7)), 0, "overlapping ranges");
}

#[test]
fn a_different_status_code_is_a_different_concern() {
    // Every word matches; only the status code differs, and that is the whole
    // difference between the two branches.
    let four_oh_one = Concern::new(
        "src/api.rs",
        Some((10, 10)),
        "Handle HTTP 401 responses",
        "",
        "",
    );
    let four_oh_three = Concern::new(
        "src/api.rs",
        Some((10, 10)),
        "Handle HTTP 403 responses",
        "",
        "",
    );
    assert!(!four_oh_one.same_as(&four_oh_three));
    let again = Concern::new(
        "src/api.rs",
        Some((12, 12)),
        "Handle HTTP 401 responses carefully",
        "",
        "",
    );
    assert!(
        four_oh_one.same_as(&again),
        "the same code is the same branch"
    );
}

#[test]
fn avoid_is_negative_guidance() {
    let avoid = Concern::new(
        "src/log.rs",
        Some((10, 10)),
        "Avoid logging secrets",
        "",
        "",
    );
    let allow = Concern::new(
        "src/log.rs",
        Some((10, 10)),
        "Allow logging secrets",
        "",
        "",
    );
    assert!(!avoid.same_as(&allow));
    assert!(!allow.same_as(&avoid));
}

#[test]
fn a_bare_test_marker_file_is_the_sibling_of_its_source() {
    assert!(siblings("src/config/memory.rs", "src/config/memory.test"));
    assert!(siblings("app/src/Panel.ts", "app/src/Panel.spec"));
}

#[test]
fn placement_uses_the_line_github_pins_the_comment_to() {
    // A quoted span reaching line 40 is published as a pin on line 10, so the
    // concern is placed at 10, not at the far end of the quote.
    let mut finding = OH7129_TESTS_155.finding();
    finding.line = Some(10);
    finding.end_line = Some(40);
    assert_eq!(Concern::of(&finding).range, Some((10, 10)));
}

#[test]
fn an_antonym_imperative_reverses_the_request() {
    // "Reject" and "Allow" share every content word; only the verb differs.
    let allow = Concern::new(
        "src/parser.rs",
        Some((10, 10)),
        "Allow empty values in the parser",
        "",
        "",
    );
    let reject = Concern::new(
        "src/parser.rs",
        Some((10, 10)),
        "Reject empty values in the parser",
        "",
        "",
    );
    assert!(!allow.same_as(&reject));
    assert!(!reject.same_as(&allow));
}

#[test]
fn a_contraction_is_negation() {
    // "Can't" splits into "can" and "t"; neither is a negator on its own.
    let cant = Concern::new(
        "src/log.rs",
        Some((10, 10)),
        "Can't allow logging secrets",
        "",
        "",
    );
    let allow = Concern::new(
        "src/log.rs",
        Some((10, 10)),
        "Allow logging secrets",
        "",
        "",
    );
    assert!(!cant.same_as(&allow));
    assert!(!allow.same_as(&cant));
}
