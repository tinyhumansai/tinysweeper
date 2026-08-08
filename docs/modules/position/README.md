# `position`

Turns a quoted snippet into a line range.

## The model never emits a line number

It emits `existing_code` — a verbatim copy of the code it is complaining about —
and the host works out where that is.

The old arrangement was the obvious one: the model reports a line, and the lane
drops any finding whose line is not in the diff. It fails in the way models
fail. They are bad at arithmetic over a rendered diff and good at copying, so a
correct finding routinely arrived with a line number two or three off, and got
thrown away by the very filter meant to protect against mis-anchoring. The
filter was right to distrust the number; the mistake was asking for the number
at all.

Inverting it removes the whole class of failure. A wrong quotation still fails
to match, but a *wrong* quotation is rare in a way a wrong line number is not,
and the failure is now recoverable rather than silent.

## Three stages, first hit wins

| Stage | What it does | Cost |
| --- | --- | --- |
| 1 | Slide the snippet over every hunk: new side first, then the old side | free |
| 2 | Slide it over the whole head-revision file | free |
| 3 | One cheap model call that re-extracts the snippet, then stages 1–2 again | one cheap-tier call (`Workload::Relocate`) |

Stage 1 resolves nearly everything. Stage 2 needs the file, which only a run
with a checkout has; without it the stage is skipped rather than guessed at.
Stage 3 runs only for a finding that survived both, and its answer is fed
straight back through stage 1 — it cannot assert a position, only propose a
quotation that then has to match.

If all three fail, the finding is **unanchored**: it keeps its title, body and
severity, loses its line, and is rendered into the check-run summary instead of
posted inline. It is not dropped. Dropping it was the bug.

## Normalisation is the crux

Exactly three steps, applied identically to both sides:

1. trim whitespace,
2. strip **one** leading `+` or `-`,
3. trim again.

Blank lines are dropped entirely when splitting. Those four rules absorb the
three ways a model mangles code it copied out of a diff: indentation drift, a
leaked diff marker, and blank-line mismatch. CRLF falls out of step 1.

Nothing else is normalised. Collapsing interior whitespace or lowercasing would
start matching code the model never quoted, and a confident match on the wrong
lines is worse than no match — it is the exact failure the module exists to
prevent.

Stripping a marker is safe *because* both sides go through it: a genuine line of
code beginning with `-` loses its marker on both sides and still compares equal.
Only one marker is stripped, so `--force` stays `-force` rather than becoming
`force`.

## Old-side matches are still head-revision line numbers

A snippet that matches a deleted line has no head-revision line of its own. It
borrows the nearest surviving line in the same hunk — the next one, falling back
to the previous one, falling back to the hunk's start. Reporting the
base-revision number would put the comment on whatever unrelated code now
occupies that number, which is the one thing this code must never do.

## Where a resolved finding may be posted

Resolution says *where the code is*. The lane decides whether that is postable,
and the rule is: inside a hunk. That is what GitHub accepts an inline comment
on. It is wider than the changed-line set — a quoted context line inside the
hunk is fair game, because the quotation is evidence the model read it — and
narrower than the whole file, so a stage-2 match out in untouched code goes to
the summary instead.

## The corpus

`src/position/test.rs` is the test that matters: one fixture file, one fixture
diff, and every known way a snippet arrives mangled — wrong indentation, a
leaked `+`, a leaked `-`, blank-line drift, CRLF, a snippet only reachable
through stage 2, and a snippet that exists nowhere. Each asserts an exact range
or an explicit unanchored reason. Never a panic, and never a plausible-looking
wrong line.
