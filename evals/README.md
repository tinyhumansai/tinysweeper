# `evals/` — the labelled corpus

Review quality as **data**, the way `presets/` is review policy as data. Live
evaluation runs through `tinysweeper eval`: a labelled pull request is not a
unit test — it costs money to run, its answer moves, and it is edited by
whoever disagrees with a score. `cargo test` does load this corpus too, but only
to replay the committed cassettes offline for free, never to call a model.

See [docs/modules/eval/README.md](../docs/modules/eval/README.md) for how the
harness works. This file is the labelling contract.

## Layout

```
cases/<id>.toml        one labelled pull request
fixtures/<id>.json     its frozen forge state, written by `eval add`
cassettes/<id>/        the model's recorded answers, written by `eval run --record`
baselines/current.json the scorecard a change is compared against
runs/                  scratch output (gitignored)
```

## The one rule

**Every case cites evidence outside this bot.** The loader refuses a case whose
`provenance.evidence` is empty, and that is not bureaucracy — it is the only
thing standing between a corpus and a mirror.

An expectation written by reading tinysweeper's own output measures whether
tinysweeper still agrees with itself. It cannot expose a blind spot, because a
blind spot is exactly what never appears in the output you copied from. So
evidence has to be something the bot did not produce:

- the follow-up commit or pull request that fixed the defect,
- a human review comment somebody acted on,
- a revert,
- an issue filed against the behaviour.

## `exhaustive` is a claim, and it is off by default

A case only contributes to **precision** when `exhaustive = true`, which asserts:
*I read this whole diff and these are all the defects in it.*

Without it, a finding that matches no expectation is recorded as `unscored` —
neither a credit nor a defect. That default is not timidity. The first live run
of this corpus reported a real off-by-one in a helper that nobody had labelled,
and scored it a false positive; you can only call an unmatched finding wrong if
you have asserted every right one.

Consequence worth knowing: a large `unscored` count means the **corpus** is
thin, not that the reviewer is noisy. It is the number that says how much
labelling is still owed.

## Adding a case

```sh
tinysweeper eval add --repo owner/name --pr 123 --id ts-0123-short-slug
```

That freezes the pull request into `fixtures/` and leaves a stub in `cases/`.
The stub will **not** load until a human fills in `provenance.evidence` and
writes the labels — which is the point. Then:

```sh
tinysweeper eval run --record          # live, costs money, writes cassettes
tinysweeper eval score                 # free, offline, re-reads the proposals
tinysweeper eval report --baseline evals/baselines/current.json
```

`eval score` is the loop to iterate in. It re-reads proposals from disk, so
rewriting a matching rule or a label costs nothing.

## Writing expectations

`must_mention` slots are a **conjunction of alternations**: every slot must be
satisfied, and `"evict|expire|unbounded"` is satisfied by any one of the three.
Deliberately not regular expressions — a corpus that needs a regex engine to
read is a corpus nobody audits.

The keyword check exists because a lane will happily leave a naming nit on the
exact line that holds the real bug. Scoring on path and line alone counts that
as a find, and the harness would then reward commenting on hot lines.

Write slots for the *concept*, not the phrasing. `["panic|empty"]` survives a
reworded model; `["panics when the slice is empty"]` does not.

## Writing `[[forbidden]]`

Every entry needs a `reason`, because the next person to read an unexplained
exclusion will assume it is a mistake and delete it. Unlike `must_mention`, one
slot matching is enough — this is looking for a specific wrong claim rather
than confirming a whole right one.

An entry may be scoped by `lanes` instead of keywords when the defect is
structural. "The `description` lane must not anchor to implementation code" is
about which lane landed where, and no vocabulary expresses it.

## What this corpus does not measure yet

**Recall.** Both current cases are regressions — they assert what the review
must *not* say — so `recall` renders `n/a` and will until cases with
`[[expected]]` entries land. Those need a human to read a diff and a fix and
write down what a good reviewer should have caught, and no amount of
machinery substitutes for it.

**Clean pull requests.** A case with `exhaustive = true` and no expectations is
how noise gets measured, and there are none yet. The obvious candidates —
recently merged pull requests with no review comments — are not usable evidence
here: the only reviewers on this repository are tinysweeper itself and a
rate-limited CodeRabbit, so "nobody objected" is close to circular.

**More than one language.** Every case is Rust, so the corpus measures Rust
review. Cases mined from a public repository may also sit in a model's training
data.

These are stated rather than fixed because a corpus that overstates what it
covers is worse than a small one that does not.
