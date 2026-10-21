# Large pull request review: implementation contract

Issue: <https://github.com/tinyhumansai/tinysweeper/issues/228>, part of #236.
This is an implementation plan, not a claim that the feature exists.
Execute the tasks in [the task sheet](2026-10-19-large-pr-review-tasks.md).
Do not run contributor code, build its dependencies, or execute its tests.

## Source baseline and prerequisites

Research began at TinySweeper epic `818b5a48`; final plan incorporates merged
#233 at `af037206351888435580f8f5e29b64d0f8eca1ef`.
Timing interfaces were inspected on the pending #221 branch, at `8526274b`.
Merge #221 from the canonical epic into this branch before coding;
retain their production fixes, actual-path coverage and compact rendering.
The canonical targets are TinySweeper `epic/236-review-health`, OpenHuman
`ci-test/epic-236-review-health`, and TinyAgents `epic/236-review-health`.
No final rollup to `main` is merged by this plan.

Current source constraints:

- `app/review.rs::review_with_tree_observed_at_head` in #221 owns context,
  scanners, lane calls, summary, threads and wireframes. Lanes currently await
  sequentially. Preserve public entry-point signatures and ordinary result
  compatibility; the new soft scheduling stop deliberately changes long-running
  reviews. Backend-specific physical guarantees are defined below.
- `lanes/fanout.rs::per_unit` eagerly creates every unit future and uses
  `join_all` with eight permits. Timing out that whole future loses completed
  results. Merely limiting its input is insufficient for the soft deadline.
- Critique and security both use `grouping::group`; both have deterministic
  triage/replay/mechanical preparation before model work. Critique's
  `review_group` performs council calls, placement, falsification and adaptive
  passes. One group is not one physical provider call.
- `ModelCapability::{new,with_budget}` makes fresh monetary lane scopes.
  Gateway `scoped_budget` replaces its monetary ledger. A count scope must
  survive these calls; it must not be reconstructed per lane or pass.
- The monetary scope has the same review-wide lifetime requirement. Binding a
  `scoped_budget` once at review entry is insufficient: each
  `ModelCapability::{new,with_budget}` scopes again, and each Gateway
  `scoped_budget` currently starts an independent ledger. After confirming the
  exact merged Embed API, retain a distinct optional review-root `Budget` on
  `GatewayModel`. When bound, derive each lane's budget with `Budget::child`
  using that lane's limits, so root and ancestor reservations/settlement stay
  shared while lane caps remain local. When unbound, keep existing fresh-scope
  behavior, including `a_fresh_budget_scope...` compatibility coverage. Bind
  the review root before paid knowledge extraction; lanes and direct summary,
  thread and wireframe model calls must use root-bound models. This records the
  reviewed integration constraint; exact wrapper/API details remain subject to
  the dependency review and must not be guessed from an unmerged API.
- The strong integration review recommends a separate optional neutral model
  hook, `Model::with_review_budget(limit_usd) -> Option<Arc<dyn Model>>`, with
  default `None`; do not couple the physical-dispatch control trait to Embed's
  `Budget`. Gateway root binding creates one fresh Embed `Budget`, preserves
  `review_budget` through clones, and derives `Budget::child(SpendLimits)` for
  lane caps when bound. Unbound `scoped_budget` keeps its current fresh-budget
  behavior. Factor USD-to-Embed-limit conversion so root and child limits share
  the same rounding. `ObservedModel` and `OwnedModel` forward root binding
  without losing timing or ownership. Cassette Record forwards it while
  retaining its existing lookup/storage state and physical capability flags;
  Replay makes no live money reservation or physical event, and any logical
  replay admission is a separate policy.

  Tests must respect Embed reservation semantics: dropping an unsettled money
  reservation conservatively charges its requested spend, unlike dropping an
  uncommitted physical permit, which refunds the call slot. Reuse
  `GatewayModel::admission_fixture`: bind a `$1` review root, hold exactly
  `1_000_000` micros, then poll two actual `ModelCapability` scopes and a direct
  base `complete` concurrently; none may reach the provider until the hold is
  settled with `Spend::default()`. This catches leakage from old fresh lane
  budgets. With root cap `2R - 1`, two lane caps at least `R`, and a direct root
  call, hold the first provider response and require exactly one dispatch;
  after settling paid `R`, the others must refuse at the root budget. A tighter
  lane cap must refuse without consuming the sibling or root allowance, and a
  second fresh review must remain isolated. Keep the existing independent
  fresh-scope test unchanged. Detailed fixture evidence is in the private
  source-preparation log; confirm exact API behavior against merged Embed code.
- #221 observes physical dispatches inside Embed and TinyAgents, separately
  from logical completion usage. Its observer callbacks cannot refuse a call.
  Existing `ModelBudget` bounds tokens and money, not physical call count.
- `LaneOutcome.coverage` / `LaneProposal.coverage` contain actual reviewed and
  unanswered paths. #233 adds host-derived summary coverage; whole-PR lanes
  must not imply they read every file. `Proposal.complete()` already rejects
  unreviewed/unanswered paths, and #225 keeps unanswered lanes Neutral.
- The server's hard deadline is 900 seconds and includes checkout/setup. It
  starts after a queued review receives a worker permit, so durable queue wait
  is recorded separately and does not consume the active-review deadline. #223
  keeps authoritative-head hard timeouts terminal, never requeued from zero.
  The publishing deadline and lease fencing are separate and remain so.

A small application-neutral dependency change is required for strict physical
admission. Implement it upstream, with independent tests and PRs, then bump
TinySweeper's gitlink, Cargo revision, lockfile and patch entries together.
Do not implement a logical-call counter and call it the physical budget.
The dependency design and exact files are in the task sheet.

## Configuration and deterministic planning

Add `Review.large_pr: LargePr` (serde-defaulted, deny unknown fields), with
operator-owned `[review.large_pr]` defaults:

| Key | Default | Meaning |
| --- | --- | --- |
| `enabled` | `true` | Enable size-based large mode |
| `files` | `30` | Large at this many changed files or more |
| `changed_lines` | `2000` | Large at this many additions + deletions or more |
| `groups` | `24` | Large at this many critique/security candidate groups total |
| `max_groups` | `12` | Maximum selected model groups, across both lanes |
| `max_calls` | `48` | Maximum admitted physical calls, across the whole review |
| `soft_deadline_percent` | `80` | Stop admitting new groups/calls at 720/900 seconds |
| `drain_seconds` | `30` | Maximum time to finish already running groups |

All numeric settings must be positive; percent is 1..=95. `max_calls` is an
independent hard count ceiling, not an estimated dollars-to-calls conversion.
Existing 500-file / 50,000-line operator refusal guards still apply first.
Do not add these keys to `config/remote.rs::OVERRIDABLE_KEYS`: a target PR
must not buy more calls or extend the server deadline by editing its config.
Update `known_keys`, validation, defaults-loading and unknown-key tests.

Create `src/lanes/large_pr.rs` with module docs and offline unit tests. Types:

- `ReviewMode::{Normal,Large}`.
- `ReviewEstimate { files: usize, changed_lines: u64, groups: usize }`.
- `PlannedGroup { lane: LaneId, group: FileGroup, risk: RiskScore }`.
- `ReviewPlan { mode, estimate, selected: Vec<PlannedGroup>, omitted:
  BTreeMap<LaneId, Vec<String>> }`.
- `RiskScore`: an ordered tuple of scanner flag, prior-finding flag, sensitive
  path/sink signal, public-API signal, capped changed-line count. Compare
  descending, then path ascending, then security before critique on exact ties.

Estimate file/line size from the bounded forge context's complete diffstat,
not only parsed added lines (deletion-only and missing patches count). Count
candidate groups after the existing deterministic per-lane triage, replay and
mechanical checks, using the same `GroupBounds` and graph as execution. Extract
that preparation into shared lane-local helpers; do not maintain a second
approximation which can select different groups. Planning makes no model call.
Load host prior findings, run scanners and read the existing graph before the
first paid model call, then finalize this plan. `knowledge::gather` currently
reads pinned documents and calls `Extractor::extract` in the same function.
Split that production path into a pure pinned-document/metadata read followed
by explicit paid extraction; keep `gather` as a compatibility delegator for
other callers. Neither candidate planning nor its configuration lookup calls
the extractor. Freeze mode/count ceiling, reject unsupported Large adapters,
and install the shared scope before invoking paid extraction, retrieval models,
lanes, tests, summary, threads or wireframes. No consumer gets an early bypass.
Normal mode retains all groups, existing passes/council policy and ordering.

For large mode, stable-sort all eligible critique/security groups together by
risk and take the first `max_groups`; top-K means groups across the two lanes,
not K in each lane. A group's risk is its highest member score, with total
changed lines as an additional tie-breaker. Never split a bounded related-file
group to fit K. Grouping bounds may already have converted an oversized group
to singletons. Omitted member paths remain explicitly unanswered in that lane.

Risk inputs are host-derived only:

- Scanner-forced paths always sort first; scanner findings remain published
  even if no model group fits. Previously posted finding paths (`PriorReview.anchors`) rank next. This is
  a host priority hint; do not infer paths from free-form model titles.
- Reuse the security-path/sink/supply-chain predicates in `lanes/triage.rs`;
  expose a `pub(crate)` scoring helper rather than copy their lists.
- Public API means changed Rust `pub` declarations, JS/TS `export` declarations,
  Python `__all__`, public headers, or API/schema paths. This is a priority
  hint, not a claim to parse every language. Read added/removed diff lines as
  untrusted data; no external parser or process runs.
- Size is additions + deletions, capped at 1,000 per file for ordering. A huge
  docs file must not outrank an authorization boundary solely by its size.
- Documentation remains eligible for critique; existing security triage may
  intentionally exclude it. Excluded-by-policy differs from omitted-by-budget.

Large mode sets effective `review.passes = 1`; no adaptive pass, retry of an
entire group, or second council reviewer. Use the configured primary reviewer
for the first pass, retaining placement and falsification safety checks.
Physical schema repair, route fallback, truncation retry and agent turns still
consume the shared count ceiling. A partially answered, unfalsified group does
not become reviewed merely because its first response arrived.

## Execution, deadlines and ownership

Add `src/app/review_execution.rs`, exposing crate-local `ReviewExecution` with
an `Arc` inner, absolute Tokio clock instants, mode, gate, stop reason and
completed physical usage ledger. Construction takes the already established
server hard deadline; derive soft deadline from its start and 80% duration.
Do not start a new 720-second timer after checkout. Offline/local public entry
points use a fresh 900-second execution window. Keep the server constant as
its authority and pass a `ReviewExecutionOptions` value into a new internal
entry point; old entry points delegate with default options.

`ReviewStopReason::{LargePrLimit,CallLimit,SoftDeadline}` are successful partial
completion reasons. They are distinct from `ReviewSuperseded`, ownership loss,
closed PR, generic provider failures and #223's terminal hard timeout. Gate
refusals carry a typed reason through dependency errors; never infer one from
an arbitrary provider's text. After a call refusal, preserve already answered
units; do not make another fallback call hoping to bypass the gate.

Use a new `fanout::per_unit_bounded` over a `FuturesUnordered` (or equivalent
buffered stream) that admits ranked groups lazily. Keep `per_unit` for existing
normal callers/tests, delegating to the unlimited scheduling policy. Return
completed `FileReview`s immediately into the retained result accumulator.
Before polling a new group, check the shared soft deadline and selected set.
At soft deadline, stop scheduling new groups in every mode/backend. Production
Gateway paths additionally freeze every physical dispatch through the gate;
the opaque custom-backend compatibility limit is defined below.
Drain existing groups for at most `min(drain_seconds, hard_deadline - now -
5 seconds)`; drop unfinished group futures at that cutoff, naming every member
unanswered. If there is no safe remaining interval, cancel immediately.
Keep a deterministic clock abstraction or use Tokio paused time in tests.
Do not sleep for 720 real seconds in a test.

The soft rule applies in normal mode too: a small PR with slow providers must
publish partial answers instead of waiting for the server's hard timeout.
Large mode additionally limits the selected set and physical calls. For
already-running production Gateway groups, no new physical provider call starts
after soft deadline; a currently admitted provider call may finish during the
drain interval. Opaque custom backends retain the bounded cancellation contract
below, without a false claim about their internal provider invocations.
A stopped lane does not discard the group's successfully completed siblings.

The lane loop may remain sequential in #228. Schedule selected critique and
security groups in global risk order through a shared ranked admission queue;
if a lane's next unit is not next globally, a cross-lane dispatcher must launch
that unit rather than wait on an unpolled later lane. Therefore execute the two
per-group model lanes through this small dispatcher in large mode, collecting
per-lane outcomes, without making all six lanes concurrent. Normal-mode
all-lane concurrency and the shared eight-physical-call semaphore belong to
#229. Keep the scheduling policy separate from the per-dispatch gate.

After the stop, still run deterministic scanner publication/filtering, finding
caps, continuity and proposal construction. Use `summary::deterministic_with_paths`
with full PR metadata and actual lane outcomes; do not spend a last summary
call after the deadline. Skip new model thread-resolution/wireframe work,
retain previous continuity, and explicitly omit new wireframes. Deterministic
thread decisions may proceed. No postprocessing path may bypass the gate.
If setup itself crosses the soft deadline, publish a zero-reviewed Neutral
proposal, with all eligible paths unanswered, without starting model work.
The hard server timeout remains a genuine final safety boundary.

Incremental and manual Full reviews both use size/deadline policy. Full still
means no replay shortcut, not unlimited money/time, and remains a distinct
admission job under #223. Never add a requeue, clear a terminal head marker or
reset the existing per-head paid-attempt/deadline ledger for a partial result.
Publish partial proposals through the existing deterministic apply/fenced path.
A successfully published partial report is terminal `Published`, not `Deadline`.
It cannot approve or automerge a partially reviewed PR.

## Honest coverage, partial accounting and compatibility

Add serde-defaulted host-only `Proposal.partial_review: Option<PartialReview>`
with mode, stop reason, estimate, selected groups and omitted path/reason pairs.
This is not model output and contains no secret values or prompt content.
Keep `Proposal.unreviewed` as actual paths, unioning genuinely uninspected
forge files and intentionally omitted/unanswered large-PR paths. Lane coverage
contains only units fully answered, placed and falsified, plus independently
verified replay/mechanical paths. All selected-but-interrupted unit paths are
unanswered. A whole-PR description/tests response cannot erase these gaps.

Extend summary rendering deterministically: display `not reviewed (large PR)`
for size/count selection; use `not reviewed (time limit)` for deadline paths.
Keep reason and full list in the existing collapsed Diagnostics / coverage
section; retain #233's compact visible summary and #172's `Unverified` advisory
labels. An interrupted model lane with no qualifying blocking finding concludes Neutral.
Preserve the existing `LaneOutcome::conclusion` precedence: a real finding at
the blocking floor still gives Failure, followed by Neutral for unanswered
work; never weaken a known defect merely because coverage is incomplete.
Keep that finding and the overall request-changes policy, but never claim
complete coverage or emit approval. Existing complete lanes
may retain their truthful individual result. Preserve deterministic scanner
findings independently of a stopped model lane.

Use a review-wide physical usage accumulator separate from optional timing.
Capture the #221 observer/dispatch context before worker moves and forward its
terminal events to both timing and the accumulator; legacy offline models get
one logical terminal fallback only where #221 already establishes that fact.
At finalization use this accumulator as the source of paid model totals, plus
separately recorded non-model embedding spend. Do not sum those totals again
with lane-returned `Spend`. Keep per-lane spend for diagnostics, not a second
charge. A paid first agent turn survives cancellation of its next turn and an
answered sibling survives another group's cancellation. Unknown usage remains
unknown/conservatively reserved in the monetary ledger; never invent a zero
bill or fabricate provider token counts. Add safe status metadata if needed
so a partial report cannot label unknown cost as fully settled.

The per-review scope survives GatewayModel, OwnedModel and ObservedModel
clones and every `scoped_budget` call. Add an object-safe
`Model::with_execution_control(Arc<dyn ModelExecutionControl>) -> Option<Arc<dyn Model>>`
with default `None`. Define the public control/permit/refusal port types in
`src/ports/model_execution.rs`; `ReviewExecution` implements that port rather
than exposing a private app type through a public trait. Production wrappers
forward it, Gateway installs the
physical capability, MockModel implements the same admission at its actual
canned-response dispatch. Cassette recording must forward control and the exact inner complete/review
physical capability flags through `scoped_budget` and every derived clone;
recording repository queries must not create an ungated `inner.review` route.
Both Strict and Loose replay report physical capability false: historical
response usage is replayed data, not current physical provider dispatch. Replay
may use an explicit deterministic logical admission adapter to exercise offline
scheduling while retaining those false physical flags. Record mode whose inner
cannot honor control must fail closed in Large before any paid extraction or
model request; it must never fall back from missing `review` support to an
ungated live completion. Recording and playback share cassette storage/cursors,
not separate execution ceilings or invented physical events.

For production Gateway, all modes require the physical control to survive all
wrappers, including Cassette Record, OwnedModel and ObservedModel. An adapter
which advertises physical support but loses control is a configuration error,
not an invitation to fall back to counting logical calls. Do not count again at
the outer wrapper. A test-only backend with a proven one-dispatch contract can
use the explicit logical adapter; its scope counts that dispatch exactly once.

Unknown opaque custom models returning `None` from `with_execution_control`
remain compatible in Normal mode: new group admission stops at the absolute
soft deadline, completed answers are retained, and unfinished logical futures
are canceled at the bounded drain cutoff. No new host logical model request
(including postprocessing) is started after soft time. Because a preexisting
opaque backend may internally retry or delegate while its already-started
logical future drains, do not claim a physical cutoff/count guarantee inside
it. Document that limitation on the public model port and execution options;
unknown backend telemetry remains unknown rather than fabricated physical
counts. The Normal scheduling/partial-publication guarantee is unchanged.
Unknown Large always fails closed before any paid work, including knowledge
extraction and summary. Public signatures and default trait implementations
stay compatible; ordinary Normal custom models need no new required method.

## Interfaces reserved for adjacent leaves

#229 reuses one `ReviewExecution` and gate across all lanes, adds an eight-slot
semaphore inside `PhysicalDispatchGate::acquire`, and makes the normal lane loop
concurrent. It must not create another monetary/call ledger per task. #228
already shares the count scope, but does not redesign dollar allocations.
#230 reads `ReviewPlan.mode`; Large unconditionally stays one pass regardless
of adaptive settings. Normal mode retains its policy until #230 changes it.
#231 may change checkout reuse, but must preserve this absolute deadline start.
#234 consumes host partial/coverage facts; partial publication never becomes an
approval merely because the visible findings match a previous complete run.
#235 records fixture budgets/coverage and actual physical call counters.

No new write surface is introduced. Gate/lanes hold no `ForgeWrite` or write
token. Existing read-only tree redaction, safe prompts, checkout mutation
checks, before/after model ownership fences and deterministic apply survive.
