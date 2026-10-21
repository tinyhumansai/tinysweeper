# Large-PR review: dependency and TDD task sheet

Read [the implementation contract](2026-10-19-large-pr-review.md) first.
Every task below is a red/green slice. Capture raw command output, exit status,
full HEAD and uncommitted source hashes. A test compiling badly is not the red
signal; first compile its fixture against an old production path, then assert
the intended behavior fails. Auto-commit checkpoints are retained unchanged.
No Cargo starts without the orchestrator's build slot. Use `-j1`, no temporary
target directory and no `CARGO_TARGET_DIR` override.

## 1. Neutral upstream physical admission

Required because existing physical observers are terminal accounting hooks,
not admission hooks. Keep these dependency PRs separate from TinySweeper #228.
Use a fresh Luna implementer for each repository, a strong independent review,
and canonical epic targets. Branch from fetched main with `worktree`, then
normal-merge the existing epic; never edit a checked-out vendor submodule here.
The task is not complete until dependency PRs are merged and pins are coherent.

### TinyInference: final pre-dispatch check

The existing `BudgetedModel::admit` can await monetary capacity. Consequently
checking the soft clock only before that wait admits a late provider call.
Add a neutral optional synchronous `BeforeDispatch` hook in
`crates/tinyinference-llm/src/model/budget.rs`, with a public builder on
`BudgetedModel` and a default absent hook preserving all existing callers.
Exact new interface (in the existing budget module):

```rust
pub trait BeforeDispatch: Send + Sync {
    fn check(&self) -> crate::Result<()>;
}
// New field: Option<Arc<dyn BeforeDispatch>>, initialized to None by new().
// New builder on BudgetedModel<State>:
pub fn before_dispatch(mut self, hook: Arc<dyn BeforeDispatch>) -> Self {
    self.before_dispatch = Some(hook);
    self
}
```

Keep the existing constructor, ModelBudget struct literal fields and serde
spend types unchanged; Debug must not reveal the hook internals. Invoke
it after monetary `admit` and immediately before the inner model invocation,
for both invoke and stream construction. If it refuses, explicitly settle the
already-owned monetary reservation with `Spend::default()`; return before
creating physical telemetry or calling the provider. A failed preflight proves
no dispatch, unlike a generic provider error whose reservation stays charged.
Do not add a calls dimension or change normal settlement arithmetic.

Red/green tests in sibling `budget_tests.rs` and `budget_wait_tests.rs`:
block monetary capacity, advance the host clock past its soft deadline, free
capacity, then reject in the hook; assert provider count zero and the new
reservation leaves zero spent/reserved money. Test hook success, stream
refusal and unchanged unknown-dispatched cancellation charging. Run focused
`cargo test -j1 -p tinyinference-llm model::budget`, check, all-target Clippy and
fmt. Open/merge the canonical TinyInference epic dependency PR first, update
TinyAgents gitlink/dependency pins coherently, then continue below.

### TinyAgents

Files under `crates/tinyagents-harness/src/`:

- Add `cost/dispatch_admission.rs` and sibling `dispatch_admission_tests.rs`.
- Export from `cost/mod.rs`; integrate in `cost/model_budget.rs` and its tests.
- Keep `observability/physical_dispatch/{mod.rs,types.rs}` telemetry-only.

Public neutral interface (no repo, PR, secret, prompt, or TinySweeper types):

```rust
#[async_trait]
pub trait PhysicalDispatchAdmission: Send + Sync {
    async fn acquire(&self) -> Result<Arc<dyn PhysicalDispatchPermit>, DispatchRefusal>;
}
pub trait PhysicalDispatchPermit: Send + Sync {
    fn mark_dispatched(&self) -> Result<(), DispatchRefusal>;
}
```

`DispatchRefusal` has a typed `DispatchRefusalReason` enum:
`Deadline`, `CallLimit`, `Cancelled`. It implements Error/Display; displays no
request content. `mark_dispatched` is idempotent and synchronous, rechecking the absolute clock
and cancellation before committing the reserved count slot; releasing
capacity happens in Drop of the host-owned permit. All exported items get docs.
No admission value means the existing unbounded behavior.

Add a wrapper builder accepting optional monetary `ModelBudget`, optional
observer and optional admission, keeping old builder APIs delegating to it.
For each invoke acquire the count/concurrency permit before monetary admission.
Build an ordinary BudgetedModel with the new pre-dispatch hook calling that
same permit's `mark_dispatched`. Store a typed refusal in a call-local outcome
slot if commit fails; the hook returns the engine error and BudgetedModel
settles its zero-dispatch reservation to zero. Then inspect that slot when
returning the controlled result. Without money policy, call commit directly
immediately before the observed/normalized provider invocation. The order is:

```text
acquire permit (reserved count slot, not an attempted call)
  BudgetedModel: reserve money, optionally wait
    before_dispatch: recheck deadline, commit count, or settle zero on refusal
      existing turn/explicit observers
        GatewayChargeModel
          concrete provider
```

There is no await between successful commit and invoking the inner observed
provider. A money refusal never commits; a late clock refusal settles money
to zero and refunds the uncommitted count slot on Drop. Once committed, the
call count is never refunded on failure, cancellation or unknown usage.
The existing monetary reservation normalizes/settles exactly once. There is
no duplicate ledger or copied monetary estimation/settlement algorithm.

For streams, retain the permit in the returned guarded stream through terminal
item, clean EOF, error or Drop. Fuse terminal streams so late polling cannot
reacquire/commit another call. Release on failed stream construction too.
Delegate profile, supports-input, cache identity, deferred/poll methods to the
inner model; test the delegate paths so a capability is not silently removed.

Typed refusal must survive the boundary. Give the wrapper a per-invocation
refusal outcome slot owned by that invocation, not one global last-error slot.
The lower model error can remain its existing engine error type; the wrapper's
controlled call returns `ControlledDispatchError::{Admission(DispatchRefusal),
Model(engine_error)}`. Retry/fallback callers can distinguish refusal without
parsing strings. No shared refusal slot may attribute one concurrent call's
failure to a different call.

First red tests: `admission_refuses_before_provider`,
`money_refusal_does_not_commit_admission`,
`cancel_while_waiting_does_not_commit_admission`,
`failed_invocation_commits_once`, `stream_eof_releases_admission`,
`cancel_stream_releases_admission`, `concurrent_refusals_remain_call_local`.
Use a fake ChatModel with atomic invocation count and barrier-controlled
stream. Old path deliberately ignores admission and makes the forbidden call.
Green uses the wrapper above; prove known paid usage is unchanged, not doubled.

Focused gates from the TinyAgents worktree:

```sh
cargo test -j1 -p tinyagents-harness dispatch_admission
cargo test -j1 -p tinyagents-harness model_budget
cargo check -j1 -p tinyagents-harness
cargo clippy -j1 -p tinyagents-harness --all-targets -- -D warnings
cargo fmt --all -- --check
```

### OpenHuman / Embed

After TinyAgents merges, update its canonical gitlink and dependency pins.
Research starting pin is TinyAgents `7e28d3aa`; OpenHuman epic starts at
`44b8f3c5`. Do not replace the newer main compatibility fixes with old pins.

Change these source files, with focused sibling tests:

- `crates/openhuman-core/src/agent/tinyagents/budget.rs`: re-export neutral
  admission types; forward admission independently of optional monetary budget.
  Keep existing `ModelBudget` literal callers source-compatible. All added host-environment
  fields get absent defaults; update internal literals explicitly. No serialized
  wire payload gains a trait object or a required field.
- `crates/openhuman-core/src/inference/host_runtime/ops/complete_once.rs`:
  add a controlled entry point with typed admission errors; old
  `complete_once{,_observed}` delegate and retain their String error API.
- `crates/openhuman-core/src/inference/host_runtime/ops.rs`: export new entry.
- `crates/openhuman-embed/src/complete.rs`: add a COW policy slot/builder
  `Completer::dispatch_admission(Arc<dyn PhysicalDispatchAdmission>)`;
  call the controlled entry point on every structured repair, route fallback
  and truncation rung. Typed refusal stops the ladder immediately.
- `crates/openhuman-embed/src/error.rs`: typed refusal variant; keep accumulated
  earlier paid usage in a structured failure envelope when a later call is
  refused. Do not replace it with a usage-free generic error.
- `crates/openhuman-embed/src/turn.rs`: `.dispatch_admission(...)` builder.
- `crates/openhuman-embed/src/turn_control.rs`,
  `crates/openhuman-core/src/agent/turn_env.rs`,
  `crates/openhuman-core/src/agent/tinyagents/host/run_context.rs`,
  `crates/openhuman-core/src/agent/tinyagents/turn_exec.rs` and
  `payload_summarizer.rs`: carry the Arc explicitly into worker/child contexts,
  concrete routes and summarizer. Derive/Debug excludes the trait object's
  internals. It is a host capability, not a serialized TurnRequest field.

The generic budget model wiring in TinyAgents stays the only wrapping engine.
Unbudgeted completions also honor admission. Agentic retries and tool turns
share the same Arc; a child cannot create a fresh count ceiling. Existing
turn-scope physical telemetry is neither removed nor counted twice.
If a new controlled call bypasses the old function, preserve endpoint checks,
reserved-option refusal, read-only host authority and gateway normalization.

Red/green tests use Embed's existing local fake gateway, not external models:
`repair_cannot_exceed_dispatch_limit` (malformed JSON wants a second call,
limit one; server receives exactly one), `fallback_cannot_bypass_admission`,
`truncation_retry_consumes_dispatch_slot`,
`agent_second_turn_refusal_preserves_first_turn_usage`, and
`cancel_before_dispatch_has_zero_physical_events`.
A pre-admission rejection emits zero physical attempts and zero physical
usage/cost, plus one host policy-refusal counter when a policy actually refuses;
a canceled waiter emits no fabricated refusal. Forward one refusal accounting
event without synthesizing a second logical fallback/physical attempt.
A 400 compatibility retry
inside one concrete ChatModel invocation retains #221's one-call definition.

Focused gates: Embed completer/controlled-admission tests, core touched
host-runtime and turn-context tests, `cargo check -j1 -p openhuman-embed`,
`cargo clippy -j1 -p openhuman-embed --all-targets -- -D warnings`, and touched
core crate Clippy/fmt. Record independently reproduced unrelated baseline
failures rather than waiving a new error. Merge dependency PR with pinned HEAD
and keep every commit. Only then advance TinySweeper's pin and run its pin
assertion. No final `main` dependency PR merge is authorized here.

## 2. TinySweeper configuration and pure plan

Change `src/config/{types.rs,mod.rs,validate.rs,defaults.toml}` and tests;
add `src/lanes/large_pr.rs`, export from `src/lanes/mod.rs`.
Expose existing risk predicates narrowly from `src/lanes/triage.rs`.
Extract reusable candidate preparation from critique/security; preserve replay,
scanner-forced paths, mechanical proof and grouping bounds exactly.

Red fixtures: construct sixty distinct `src/file_00.rs` through `file_59.rs`
changes with one added public function each (no related-file names/graph edges),
plus a small ordinary fixture. Assert large versus normal mode, estimates,
12 selected groups total, stable ties, and deterministic omitted membership.
Set `max_groups = 2` and compare an auth/session file against a 5,000-line docs
file; auth wins. Add prior-finding and scanner priority tests; add deletion-only
and missing-patch diffstat tests. No fake model output is needed for pure plans.
Prove old planning chooses all sixty and fails the selected-set assertion.

Green implements the contract's exact tuple ordering and threshold defaults.
Test unknown nested keys, zero/invalid limits, old config loading and remote
config refusing to override every new budget/deadline field.

```sh
cargo test -j1 --locked lanes::large_pr
cargo test -j1 --locked config
```

## 3. Review-wide count/deadline scope and model propagation

Add `src/ports/model_execution.rs` with public object-safe control, permit and
typed refusal types; export from `src/ports/mod.rs`. Add the default
`Model::with_execution_control` method. Add
`src/app/review_execution.rs` and its in-module tests, wire from app/mod.rs.
Gateway's adapter translates this port to the neutral upstream capability.

One locked state contains committed calls, reserved slots and stop reason.
`acquire` checks absolute soft deadline and `committed + reserved < max_calls`
atomically. If only live reservations fill the count, wait on a notification
until a permit commits/drops or deadline elapses; recheck under the lock to
avoid a lost wakeup. If all allowed calls are committed, return CallLimit.
`mark_dispatched` first rechecks the absolute deadline/cancellation; if not
already committed, it then moves reserved to committed exactly once. A failed
commit leaves the slot uncommitted and the upstream hook settles money to zero.
An already committed permit returns success without counting twice. Drop refunds only
uncommitted reservation; committed calls remain spent. In normal mode there
is no count ceiling, but the absolute soft clock is still enforced. #229 later
adds the separate eight-slot semaphore to the same acquire/permit lifetime.

Change `src/harness/{embed.rs,mock.rs,cassette.rs}`, `harness/agentic/mod.rs`,
`src/app/review.rs::ObservedModel` and `src/server/routes.rs::OwnedModel` to
preserve/forward this scope through clones, monetary re-scoping and worker
moves. Add the explicit control to agentic `ReviewResources`, call the Embed
Turn builder before the task is spawned, and capture #221 timing context too.
MockModel admits immediately before recording/consuming a canned response;
a refusal consumes no cassette and increments no dispatch count. A production
Gateway call is gated only upstream, not once again at its outer complete API.

Cassette is an explicit integration task, not an assumed generic wrapper:
change `src/harness/cassette.rs` and `cassette_test.rs` after merging the #235
repository-aware recording implementation (or coordinate against that branch).
Do not wait for all of #235 or its production health criteria: if its wrapper
work has not landed, extract/land that narrowly scoped prerequisite first, or
implement the same forwarding against the current wrapper and require #235
to preserve it when its repository-aware review method lands.
Record forwards `with_execution_control` to its inner, rebuilding Cassette with
that controlled inner while sharing state, takes, ordered responses, directory
and prompt-recording policy. Its `scoped_budget` clones retain the same control.
Record delegates `complete_reports_physical_calls` and
`review_reports_physical_calls` independently and exactly to the effective
inner; Strict/Loose replay return false for both, regardless of stored usage.
`review` Record still uses QueryRecordingTree and records actual queries, but
calls the controlled inner.review; no fallback bypass or fresh control scope.
Unsupported Record+Large fails before calling the inner even for extraction.
Replay consumes no provider count/events; an explicit logical replay adapter
can enforce deterministic scheduling separately from historic usage.

Cassette red/green tests: Record with a repair-capable fake Gateway and limit
one receives exactly one HTTP request; Record repository-aware review with a
two-turn fake and limit one preserves the first paid turn and performs no
second request; record tree queries still match the real review path. Repeat
after `scoped_budget` and execution-control wrapping in both orders; assert
shared ceiling, exact complete/review physical flags, shared records, no
logical/physical double count. Test Strict/Loose replay flags remain false and
emit zero physical events, and unsupported Record+Large performs zero inner
calls or instruction extraction. Run `cargo test -j1 --locked cassette` and
`cargo test -j1 --locked --features harness cassette_dispatch`.

Before first paid consumer, split `src/knowledge/mod.rs::gather` into pure
pinned-document metadata loading and a subsequent explicit extraction helper;
retain old gather as a delegator for other entry points. Move review planning
between those two phases, install control, then permit extraction. Keep pinned
knowledge availability if extraction is refused. Tests in knowledge/mod.rs and
`app/review_large_pr_test.rs` enable AGENTS extraction, summary and retrieval:
unsupported Large must fail before any corresponding model invocation, while
a supported scope counts extraction against the same global max_calls ceiling.

Normal compatibility test uses an opaque custom Model returning None from
with_execution_control: an already-started logical future can remain pending,
but the scheduler starts no group/logical request after soft time, preserves
answered siblings, cancels pending work at drain cutoff and publishes truthful
partial Neutral. Do not assert physical invocations inside that opaque future.
A separate production Gateway+Record test proves the stronger physical cutoff,
including hidden repair/agent turns. Document the distinction on the public
model trait and options without forcing every existing Normal test model to
implement the new optional method. Unknown Large is always fail closed.

Use a separate usage accumulator in ReviewExecution and forward the existing
#221 physical terminal observer into it. Keep it available with timing disabled.
At proposal finalization use one snapshot for model totals and add only the
separate embedding total. Cover a completed paid first turn plus cancellation
of a pending second turn, and two concurrent groups where one completes and
one is dropped. Do not lose the completed group's known cost or add it twice.

Red/green tests: race 100 acquisitions against limit three (actual provider
counter <=3), scope survives three `.scoped_budget()` calls and both model
wrappers, all permits released after cancellation, refusal unconsumed cassette,
no callback fallback double count. Use paused Tokio time only with the existing
feature setup or explicit test-util dev dependency; default suite stays offline.

```sh
cargo test -j1 --locked review_execution
cargo test -j1 --locked --features harness dispatch_limit
cargo test -j1 --locked --features serve execution_control
```

## 4. Lazy fan-out retaining partial answers

Change `src/lanes/fanout.rs` and tests. Add the bounded scheduler described in
the contract; keep existing `per_unit` behavior through a default policy.
Factor `Critique`/`Security` preparation and group execution into crate-local
helpers used both by the normal run and the large-mode shared dispatcher.
The dispatcher owns the sorted selected work list, lazily starts at most eight
group futures (existing group cap), and folds results by lane. No waiter is
allowed to wait for a later lane future which the sequential loop cannot poll.

Extend `FanOut` with omitted `(paths, reason)` entries separate from provider
failures; its into_outcome keeps actual completed groups and lists every omitted
member unanswered. Do not use an error string pretending a skipped group was
sent to the provider. Model call-limit/soft-stop errors become typed omissions;
real provider errors keep their existing failure classifications.

Red: barrier fixture returns group A, leaves B pending, queues C. Advance time
to soft deadline, release B only after drain cutoff. Old join_all either loses
A or waits until hard timeout. New result contains A only, names B/C, preserves
A's spend, never invokes C, has Neutral conclusion and completed=true because
A actually answered. A fully canceled lane has completed=false. Test a
multi-file B names every member, and no new falsifier/repair call starts after
soft cutoff. All pre-existing fanout/grouping/replay/mechanical tests stay green.

```sh
cargo test -j1 --locked lanes::fanout
cargo test -j1 --locked lanes::critique
cargo test -j1 --locked lanes::security
```

## 5. Application integration and published golden regressions

Change `src/app/review.rs`, add sibling `review_large_pr_test.rs`, update
`src/server/routes.rs` internal execution-options wiring, and summary
`types.rs`, `generate.rs`, `render.rs` for host partial reasons. Keep tests
in-crate. Serialize the new host-only proposal metadata with serde defaults;
add a legacy proposal decode test. Preserve #172 advisory filtering and #233
actual-path coverage. For tests below use MockForge, real production review,
then existing deterministic apply; assert the actual recorded checks/hub and
GitHub review event, not just a helper's returned string.

Sixty-file fixture: settings `max_groups=6`, `max_calls=8`, passes=3 and council
configured with extra reviewers. Supply valid empty findings with representative
usage via MockModel; assert chosen Large, actual MockModel.calls() <=8, no
adaptive/council extra requests, selected risk order, all unselected eligible
paths named, aggregate reviewed N/M truthful, and no approving review/automerge.
Enable summary/threads/wireframe so a hidden postprocessing call cannot escape.
Add a finding variant forcing placement and falsification, and harness fake
provider variants needing JSON repair/fallback; assert real request count <=8.

Soft-time fixture: one answered group with known usage, one blocked model
response, remaining groups pending. Use a 100-second synthetic hard window,
80-second soft time and 5-second drain. At 85 seconds produce a proposal and
publish it before hard time. Assert Neutral stopped lanes, exact coverage,
retained first group's finding and usage, deterministic partial summary, no
approval, no retry/requeue of the head, and previous good report preserved if
publishing itself fails. Test zero-model setup crossing the soft cutoff too.

Server integration: supplied hard start includes simulated 75 seconds of
checkout; with a 100-second review window, only five seconds remain before
soft cutoff, not a fresh 80 seconds. A genuinely exceeded hard deadline stays
#223's terminal failure. Supersession during drain drops work and prevents old
head publication; Full uses a separate job with the same size/count policy.
Run targeted existing lease/status/backoff tests after wiring this option.
The Neutral acceptance fixture has no blocking finding; a separate fixture
proves an actual qualifying finding still yields Failure under existing #225
precedence, while overall coverage stays partial and approval stays forbidden.

## 6. Docs, required gates, review and PR

Update `docs/modules/lanes/README.md` with a linked focused `large-pr.md`,
`docs/modules/app/README.md`, `docs/modules/server/README.md`, and config
reference if present. Document defaults, large-mode selection, exact physical
boundary, normal-mode scheduling/cancellation stop, production physical cutoff and opaque
custom-backend limitation, reasons/coverage, Full behavior and retained
hard deadline. Every Markdown file remains <=500 lines. No GitBook site or
preview URL is introduced.

Required gates after current epic merge and dependency pin update:

```sh
scripts/assert-openhuman-pin.sh
cargo test -j1 --locked lanes
cargo test -j1 --locked review
cargo test -j1 --locked
cargo test -j1 --locked --features serve review_large_pr
cargo check -j1 --locked --all-features --all-targets
cargo clippy -j1 --locked --all-targets -- -D warnings
cargo clippy -j1 --locked --features harness,github --all-targets -- -D warnings
cargo clippy -j1 --locked --all-targets --all-features -- -D warnings
cargo fmt --package tinysweeper -- --check
git diff --check
```

Read all output/exits; no handwritten success summary replaces raw logs. Run
an independent strong source/test review before pushing the canonical branch.
Open ready PR with `Closes #228`, `Part of #236`, dependency PR links, controlled
red evidence, actual gate results and any independently established baseline.
Wait for actual head CI, at most two actionable feedback rounds, and merge
internally with `--merge --match-head-commit`. Close #228 after verified merge;
update the epic checklist and ledger. Final upstream rollups remain separate.
