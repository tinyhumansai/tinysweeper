# Corpus recording and 48-hour observation

This runbook covers two separate measurements: whether the labelled historical
misses are found by the real review path, and whether the deployed review queue
continues to meet its operating limits after a rollout. A successful replay
does not establish production health, and production metrics do not establish
corpus recall.

## Record the historical cases

The five `recall` cases in `evals/cases/` are frozen to externally reviewed
heads. `evals/README.md` lists their short IDs and evidence. The expected
findings are evaluator-only; never copy case TOML, expected labels, fix
explanations, or score output into model input. The recording command loads the
historical fixture and reaches the ordinary read-only review path. Use the
matching archived source checkout for any additional source lookups.

Before recording, verify the operator-owned local provider credential is
available without printing or copying its value. Use a recording-only config
that pins one known provider and model, disables model and provider fallback,
disables agentic reviewers, disables reasoning, and sets a bounded output
ceiling. Confirm its price schedule and the model's current context/output
limits from provider documentation. The CLI's `--max-cost-usd` only limits
admission between cases; it is not a provider invoice cap. Each case also has
a `budget.max_cost_usd` label. The runner binds the smallest of the operator
per-review budget, the case budget, and the run budget divided across all
selected cases to the shared review budget root. An unreported-usage refusal
stops later cases because its charge is unknown.

The first `tm-0102-memory-pending-debug-state` capture used
[`evals/recording.deepinfra.toml`](../../../evals/recording.deepinfra.toml),
DeepSeek V3.2 at pinned DeepInfra. It exhausted its eight-call initial limit:
the proposal marked the review partial, left ten files unanswered, and did not
complete a file. Treat its missed label as a cap-interrupted partial baseline,
not a completed recall measurement. Its cassette set is archived separately,
and its proposal remains in the original output directory; rerunning Record
mode makes new provider calls and can replace cassette data.

An explicitly authorized GPT-OSS 120B/Novita continuation completed the same
case's critique lane on all ten eligible added or modified files, with no
unanswered eligible files. The fixture has eleven changed paths; its
deletion-only `crates/tinymcp/src/registry/oauth/types.rs` has no changed head
lines and is excluded by critique policy. The continuation made 30 successful
calls, reported $0.011998 known cost, and missed the sole expected label (0/1);
three other findings were unscored. The four other labelled cases were also
recorded: all five completed eligible reviews missed their expected label
(0/5), with 43 of 44 fixture paths reviewed and one deletion-only path excluded.
The first capture and continuation use separate model identities, cassettes,
and output directories.

The bounded continuation candidate is
[`evals/recording.gptoss-novita.toml`](../../../evals/recording.gptoss-novita.toml).
It pins GPT-OSS 120B to the Novita endpoint, disables all fallback and agentic
review, uses JSON structured output, sets a 12000-token starting limit, and
requests low reasoning effort. Revalidate the exact endpoint, native output
limit, and rates immediately before any call; this candidate does not
authorize a call by itself.

For any paid recording, calculate the worst-case dispatch cost from the
configured input ceiling and the largest output allowed by retries, then
multiply by the maximum physical dispatch count. Include every selected case
and known prior attempts in the shared total. Proceed only when that bound is
at most $0.50 per case and $2.50 for this work. Do not treat a gateway balance,
an API key's account limit, a corpus label, or the CLI admission threshold as
authorization to exceed those limits. If usage is unknown or the bound cannot
be established, stop paid work and preserve the case as unrecorded.

The first case's initial V3.2 run used eight calls. Its authorized Novita
continuation reserved 32 and committed 30, leaving a cumulative audit of
38/40. Reconcile the ledger with the parent before using either remaining slot.
The other four cases may each reserve at most 32 attempts. Never delete or
reset these ledgers. Using the verified GPT-OSS Novita endpoint bounds, one
worst-case dispatch (131072 input and 32768 native output tokens) is at most
$0.014746 after micro-dollar rounding. The five-case upper bound, including
the first case's known initial $0.011331, is $2.370691. Recheck the live
endpoint and recalculate these bounds immediately before use; stop if any
usage is unknown or the bound changes.

Record the remaining cases individually so failures and spend are attributable.
The command used for the first Novita continuation is recorded here for audit;
it must not be repeated without a new ledger reconciliation and explicit
authorization from the parent:

```sh
cargo run --locked --features harness --bin tinysweeper -- eval run \
  --record --case tm-0102-memory-pending-debug-state \
  --config evals/recording.gptoss-novita.toml --max-cost-usd 0.48 \
  --max-physical-calls-per-case 40 \
  --out evals/runs/issue235/gptoss-novita/tm-0102-memory-pending-debug-state
```

Use `--max-physical-calls-per-case 32` for each of the other four cases, with
an output directory under `evals/runs/issue235/gptoss-novita/` named for that
case. Keep the initial V3.2 proposal and run output in its original directory.

Repeat with each of the four `oh-...` cases listed in `evals/README.md`, using
only the matching historical source material when lookups are needed. Never
pass one checkout for a different repository or head. Capture each
command's source identity, config digest, exit status, raw output hash, model
and provider answered, reported usage, and known cost. Keep credential values
and request/response payloads private. Stop immediately after any provider
error that lacks usage information. The recorder persists a call-budget audit
at `evals/recording-attempts/<case-id>/recording-attempts.json`; it reserves
capacity before dispatch and records actual committed physical calls after
success or failure. A process crash leaves its full reservation charged, but
the OS releases its advisory lock when the process exits, so a bounded rerun
can reserve only the remaining allowance. The persistent lock file is never
removed: unlinking it while another process holds the old inode could let a
second process lock a replacement inode concurrently. Do not delete either
file to obtain a fresh allowance. Reconcile the existing audit and continue
using the same case output and canonical corpus root. Atomic replacement does
not promise durability across power loss.

For offline verification, run the exact selected group with its matching
recording profile. This replays only saved cassettes and makes no provider
calls. Keep output directories distinct from the live-recording outputs:

```sh
cargo run --locked -j1 --features harness,github --bin tinysweeper -- \
  eval run --config evals/recording.gptoss-novita.toml \
  --case tm-0102-memory-pending-debug-state \
  --case oh-8061-standalone-local-only-policy \
  --case oh-8061-speech-redirect-origin \
  --case oh-8061-privacy-docs-contract-mismatch \
  --case oh-8076-credential-read-failure-migration \
  --out evals/runs/replay-check/historical-gptoss-novita
python3 scripts/check-eval-scorecard.py \
  evals/runs/replay-check/historical-gptoss-novita/scorecard.json \
  --expected tm-0102-memory-pending-debug-state \
  --expected oh-8061-standalone-local-only-policy \
  --expected oh-8061-speech-redirect-origin \
  --expected oh-8061-privacy-docs-contract-mismatch \
  --expected oh-8076-credential-read-failure-migration
cargo test --locked -j1 eval
```

The legacy three cases use `src/config/defaults.toml` and a separate replay
directory. Do not compare either profile group against the full-corpus
baseline. For every scorecard, report expected labels, matches, misses,
unscored findings, strict replay completion, known provider cost, selected case
IDs, and persisted execution limits. A strict miss or review error is not a
hit. Recall is `matched expected labels / all expected labels`; show each
selected group's aggregate and each case separately.

## Observe the rollout for 48 hours

Start the observation window at the recorded deployment timestamp. Use the
server's existing queue/status telemetry and the deployment's normal metrics
source; do not infer production values from local tests or corpus runs. Record
the source query or dashboard, deployment identifier, window start/end in UTC,
sample count, numerator and denominator for each percentage, and the raw
export's digest. Keep user payloads and secrets out of the report.

Record these metrics at the start and at 24 and 48 hours:

| Metric | Measurement | Target |
| --- | --- | --- |
| Push-to-start latency | p50 seconds from accepted push to review start | under 5 minutes |
| Deadline hits | deadline-hit reviews / reviews with a terminal status | under 5% |
| Budget refusals | count of review starts refused for budget | approximately zero; report the exact count |
| Success with unanswered files | successful lanes that left requested files unanswered | zero |

For each sample, state missing telemetry explicitly. A missing denominator is
`not measured`, never zero. Investigate every deadline hit, refusal, and
successful lane with unanswered files against its trace and queue record;
report counts and causes without copying private request content. Compare the
48-hour window with the immediately preceding 48-hour window under the same
definitions and query. If definitions or deployment versions differ, mark the
comparison non-comparable and retain both raw values.

The final report must contain the actual before and after values, time windows,
sample counts, data source, deployment identifier, and the query/export hash.
If the 48-hour window has not elapsed, the report must say `pending` and must
not claim the rollout acceptance criteria are complete. Keep publication and
rollout decisions with the parent issue owner; this page is an operating
procedure, not deployment authorization.
