# `src/eval`

Measuring review quality against a labelled corpus. Always compiled; only
recording needs a key.

Before this, nothing in tinysweeper could say whether a change to a prompt, a
rule document, a threshold or a lane made the review better. The test suite
proves the machinery behaves — it cannot prove the reviewer is any good, and the
two questions are unrelated. Every document in `presets/rules/` was written from
judgement and validated by reading output on live pull requests, which measures
the reviewer against the memory of whoever last looked at it.

## Three commands, separated by files

```text
eval run    corpus + model ──► proposals + cassettes on disk   costs money
eval score  proposals + labels ──► scorecard                   free, offline
eval report scorecard (+ baseline) ──► markdown / json         free, offline
```

The split is the same one `review` and `apply` have, for the same reason: the
expensive, irreversible half happens once and everything downstream is a pure
function of what it wrote. A matching rule gets rewritten ten times before it is
right, and welding it to the run would price every rewrite at another live
corpus.

## The cassette

`src/harness/cassette.rs` is a decorator over the `Model` port — the port has one
method, so wrapping it is free, and it buys the whole offline story.

The key covers the model id, the schema name, the token ceiling and every
message. So **any prompt change invalidates every cassette that prompt
produced**, and that is the point: a scoring run that silently fell back to a
stale recording would report the old prompt's quality under the new prompt's
name. Strict replay says so and stops. `--loose` falls back to call order for a
cosmetic edit, and the count is stamped into the report so a loose run is never
mistaken for a strict one.

Usage and cost replay **verbatim** rather than being re-derived through
`src/harness/pricing.rs`, so an offline re-score reproduces the dollars the live
run actually paid — including for a model the price table has since changed.

## Lookups are frozen with the fixture

A reviewer may read the repository (`docs/modules/lanes/lookup.md`), and a
replay has no repository. `eval run --record --tree <checkout>` answers
lookups from the checkout and writes every outcome into the fixture's
`lookups`, keyed by the lookup; a replay serves them back from there, so the
follow-up prompt is byte-identical to the one recorded and the cassette
still hits. Only what was asked is frozen, which keeps the fixture reviewable
in a diff. A recording without `--tree` answers every lookup *not found*,
deterministically, which is a review that could not read — say so in the
case.

## Scoring, in two stages

**Stage one is structural**: same path, line ranges overlapping within three
lines, the right lane, at or above the expected severity. The tolerance matches
what `Finding::fingerprint` already treats as the same finding after a move.

**Stage two is a keyword check** against the finding's own title, body and rule.
It looks like overreach and is not: a lane will happily leave a naming nit on
the exact line that holds the real bug, and scoring on overlap alone counts that
as a find — so the harness would reward commenting on hot lines, which is
precisely the behaviour it exists to catch.

What is scored is what would **reach the author**: the posted findings, after
`severity_gate`, `confidence_min`, dedupe and `max_comments`, and the *worth a
look* notes the summary names below the gate. Scoring raw model output would
measure a review nobody receives; scoring only comments would report a
boundary bug the summary named as a miss. A match from the noted tier says so
in its judgement.

Assignment is one-to-one and greedy over severity, then confidence. A second
finding on a claimed expectation is a **duplicate**, not a false positive —
saying the same true thing twice and inventing something are different defects
with different fixes.

## `exhaustive`, and why unmatched is not wrong

A case contributes to precision only when it claims its labels are complete.
Otherwise an unmatched finding is `Unscored`: neither credit nor defect.

This was not the original design. The first live run reported a genuine
off-by-one in a helper that nobody had labelled and scored it a false positive.
You can only call an unmatched finding wrong if you have asserted every right
one, and a corpus that penalises the reviewer for finding something real teaches
exactly the wrong lesson.

## Path dependence is the trap

Suppression, cross-push dedupe and prior-review loading make the review's output
depend on what it saw last time. A corpus run against a warm store, or with
`review.incremental` left on, measures **run order** and reports it as review
quality — silently, because a suppressed finding looks exactly like one that was
never made.

So the runner forces a fresh `MemoryState` and `incremental = false` on every
case. That is load-bearing, which is why the runner owns the config rather than
accepting one already prepared.

## No single number

A composite score can be improved by trading recall for precision, or either for
cost, and a reader cannot tell which happened. The gate is a conjunction, each
term on its own line: recall did not fall by more than `EPSILON`, forbidden
findings did not rise, findings on clean pull requests did not rise, nothing new
went over budget, and no case newly failed to review.

`EPSILON` is 2%. A gate with no tolerance fails on provider routing noise — no
`seed` is honoured and the gateway routes where it likes — and teaches people to
re-run CI rather than to read it.

Two runs are only comparable when corpus, config, selected cases, and execution
limits match. The scorecard persists selected case IDs, the run ceiling, the
requested per-case physical-call ceiling, each case's actually admitted call
allowance, replay mode, and each derived per-case model budget. A continuation
can admit fewer calls than requested because earlier attempts already consumed
the cumulative ceiling; that actual allowance is part of comparison identity.
A legacy scorecard without these limits is unknown and fails closed;
`--allow-config-drift` is the explicit override, and the report says so.
Recording scorecards from before admitted allowances were persisted also fail
closed; replay scorecards may legitimately have no provider-call allowance.
`eval score` carries these limits forward and re-scores only the original
selection, so it cannot silently expand a selected run to the full corpus.

When an admitted recording attempt starts, any existing top-level cassette take
files for that case move into `recording-attempts/<case>/prior-takes/<attempt>/`.
The append-only reservation audit remains in place, while strict replay reads
only the latest attempt's takes instead of combining a new short recording with
an older tail.

The CI replay keeps its three default-profile cassettes separate from the five
historical GPT-OSS/Novita cases. Each exact group writes a separate scorecard;
the workflow verifies case IDs, strict replay completion, and zero loose
replays before publishing each scorecard. It does not run an unfiltered score
or compare these differently bounded groups against the full-corpus baseline.

## Historical miss cases and measured recording baseline

The corpus includes five externally verified misses from the original
reviewed heads of `tinymcp#102`, `openhuman#8061`, and `openhuman#8076`. Each
expectation is anchored to the changed hunk from the external review and cites
the follow-up fix. These historical reviews establish why the labels are
valid; they are not outputs from the current TinySweeper pipeline.

The following measurements use the recording-only GPT-OSS 120B profile pinned
to Novita (`evals/recording.gptoss-novita.toml`), not the production review
configuration. All five reviews completed the `critique` lane. The lane covered
every eligible added or modified file; the first fixture also contains one
deletion-only file, which has no changed head lines to anchor a critique and is
excluded by lane policy. The per-case score is:

| Case | Coverage | Expected matched | Expected missed | Unscored | Known cost |
| --- | ---: | ---: | --- | ---: | ---: |
| `tm-0102-memory-pending-debug-state` | 10/11 (1 deletion-only file excluded) | 0/1 | E1 | 3 | $0.011998 |
| `oh-8061-standalone-local-only-policy` | 9/9 | 0/1 | E1 | 3 | $0.009062 |
| `oh-8061-speech-redirect-origin` | 9/9 | 0/1 | E1 | 1 | $0.010656 |
| `oh-8061-privacy-docs-contract-mismatch` | 9/9 | 0/1 | E1 | 3 | $0.010875 |
| `oh-8076-credential-read-failure-migration` | 6/6 | 0/1 | E1 | 1 | $0.008242 |
| **Aggregate** | **43/44 (1 deletion-only file excluded)** | **0/5** | **5** | **11** | **$0.050833** |

All five completed eligible reviews missed their labelled finding. The 43
reviewed paths are the 44 changed fixture paths minus the one deletion-only
path; there were no unanswered eligible paths. The aggregate is the
measured result for this corpus and recording profile, not a production
benchmark or a claim about other lanes. The initial DeepSeek V3.2 capture for
`tm-0102-memory-pending-debug-state` is a separate partial baseline: it stopped
at its eight-call limit with ten files unanswered. Its miss is excluded from
the aggregate above. See [`evals/README.md`](../../../evals/README.md) for case
provenance and the [recording runbook](recording-and-48h-runbook.md) for the
provider, configuration and observation limits.

Each case selects only the `critique` lane. These measurements are evidence
about that lane on these fixtures, not a full review across all lanes.

Their original PR bodies are blank and their comments are empty: the available
PR bodies contain later fix explanations, and the exact review-time snapshots
were not retained. The frozen changed-file patches, commit ranges and short read-only
source lookups are pinned to the original reviewed heads. The lookup snippets
show unmodified callers, providers and public descriptions that help falsify
the findings. Any new recording must remain distinct from the historical miss
evidence and must use the reviewed head's source tree for additional lookups.

The fixture excerpts come from `tinyhumansai/openhuman` and
`tinyhumansai/tinymcp`, both licensed `GPL-3.0-only` at the reviewed heads;
TinySweeper uses the same license. Cases retain the source repository, pull
request, reviewed commit, review comment and follow-up fix as attribution in
`provenance`. Only changed-file patches and the short source ranges needed to
evaluate each finding are copied; the immutable source archives used to check
those excerpts stay outside the repository.

## A failed review scores zero, not nothing

A case whose review errors is scored with every required expectation missed,
rather than dropped. Dropping it from the denominator would let a run improve
its own score by breaking.

## Files

| File | Role |
| --- | --- |
| `types.rs` | `Case`, `Expected`, `Forbidden`, `Fixture`, `Scorecard` |
| `corpus.rs` | loads and validates `evals/`, reporting every problem at once |
| `runner.rs` | drives the real engine per case; `rescore` re-reads proposals |
| `score.rs` | the matching rule |
| `report.rs` | markdown and JSON, and the baseline comparison |
| `committed_test.rs` | replays the committed corpus offline, in `cargo test` |

The labelling contract lives in [`evals/README.md`](../../../evals/README.md),
including what the corpus does **not** measure yet.

Use the [recording and 48-hour observation runbook](recording-and-48h-runbook.md)
for bounded live recording, recall reporting, and the post-rollout metrics.
