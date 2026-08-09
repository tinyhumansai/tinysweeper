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

## Scoring, in two stages

**Stage one is structural**: same path, line ranges overlapping within three
lines, the right lane, at or above the expected severity. The tolerance matches
what `Finding::fingerprint` already treats as the same finding after a move.

**Stage two is a keyword check** against the finding's own title, body and rule.
It looks like overreach and is not: a lane will happily leave a naming nit on
the exact line that holds the real bug, and scoring on overlap alone counts that
as a find — so the harness would reward commenting on hot lines, which is
precisely the behaviour it exists to catch.

What is scored is what would be **posted**: findings come from the
`LaneProposal`s, after `severity_gate`, `confidence_min`, dedupe and
`max_comments`. Scoring raw model output would measure a review nobody receives.

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

Two runs are only comparable when the corpus digest and the config digest match.
A stricter gate finds fewer things without the reviewer having got worse, so
`eval report --baseline` refuses that comparison rather than letting somebody
read it as a regression. `--allow-config-drift` makes it anyway, and says so.

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
