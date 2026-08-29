# `state`

Review state that outlives a single run, and the cross-push dedupe built on it.

## The incident this exists for

tinysweeper left **48 unresolved threads on its own pull request #7**. It posted
a fresh inline review on every push and never read back what it had already
said, so every re-review repeated every finding. Nothing was wrong with the
findings — the bot simply had no memory.

The machinery was all present and unwired: `Finding::fingerprint` existed and
was tested, `ForgeRead::review_comments` existed with a doc comment describing
this exact use, `apply` already stamped a `tinysweeper:fp=` marker into every
inline comment, and `LaneInput::{reviewed_evidence, prior_findings}` were passed
empty unconditionally. This module and `findings::prior` are the missing half.

## Where the truth lives

**On GitHub, in the markers.** Every inline comment carries
`<!-- tinysweeper:fp=<16 hex> -->`, and the next review reads them back. That is
what makes dedupe work for `local-review`, on a fresh deployment, and after the
database has been wiped.

The store here is a **cache**, and only for the one thing a marker cannot carry:
the exact bytes of the evidence the last review sent, which prompt layer 3 has
to replay verbatim to earn a cache hit. Losing it costs money, never
correctness. `review_state` must never become the only place a fingerprint is
recorded.

## Identity

One identity, used for both dedupe and triage inheritance:

```text
sha256(lane \0 path \0 rule \0 whitespace-normalised anchored code)[..8]
```

It deliberately excludes the **line number** (an added import moves the code,
not the finding), the **severity** (a re-rated finding is the same finding), and
the **title and body** (a reworded explanation is the same finding). Because it
covers the code the finding points at rather than the prose about it, a
developer's acknowledgement survives a rebase and a re-review.

`Finding::fingerprint` was already exactly this function. The bug was in what it
was fed: `apply` passed `finding.title` as the context, which put the model's
wording back into the identity, so a rephrased sentence minted a new fingerprint
and got posted again. The identity is now computed once during review — where
the diff is in hand, see `findings::anchor` — stamped onto `Finding::identity`,
and carried in the proposal so `apply` and the next review cannot disagree.

## Identity is not enough on its own

Excluding the title was necessary and not sufficient. `rule` is also
model-authored free text, and a model asked the same question twice renames the
defect: on `tinyhumansai/backend#1295`, six reviews produced eighty-nine
comments carrying **eighty-eight distinct fingerprints**, and one line collected
four comments with the same title and the same suggested patch under
`discarded-error-handling`, `unhandled-error`, `discarded-error` and
`swallowed-error`. Fingerprint dedupe was working perfectly and suppressing
almost nothing, because almost nothing repeated a fingerprint.

`council::agree` had already written down why — "`rule` is model-authored free
text" — and then argued the strict rule was still right across pushes because a
rule id is stable for one class of problem across runs. That premise was wrong.

So a finding is a repeat when **either** its fingerprint was already posted
**or** a comment of ours already sits on the lines it points at, within the same
few lines of slack the council allows between two reviewers
(`PriorReview::covers`). The anchor comes from the comment's own `path` and
`line` as GitHub reports them, so it needs no new marker and works on every
thread already open.

Two deliberate differences from `corroborates`:

- **The lane is ignored.** Two lanes reporting one defect on one line is a
  duplicate to the author reading the thread, whatever it is to the pipeline
  that produced it. `#1295` posted that pair too.
- **A comment GitHub no longer places contributes no anchor.** Outdated and
  rebased-away comments still suppress by fingerprint; they simply cannot say
  where they were.

The looseness is bounded on both ends. It cannot silence a file — the tolerance
is three lines — and it cannot silence a merge, because dedupe still runs after
the conclusion is decided. A false suppression costs the author a comment they
have to read in the check-run summary instead of inline. A false *duplicate*
costs them the fourth copy of a comment they answered three pushes ago, which is
the failure this module exists for.

## Suppression cannot unblock a merge

Dedupe runs **after** the check-run conclusion is decided, in
`app::review::lane_proposal`. A finding that is suppressed as a duplicate still
fails its lane and still blocks the gate. A committed credential reported on
push one keeps failing on push five without repeating the comment.

This is also the second line of defence against a forged marker: even if one got
through, it would hide a repeated comment and could not turn a failing review
into a passing one.

## Markers are untrusted input

Anyone who can open a pull request can write `<!-- tinysweeper:fp=… -->` into a
comment. Three things stop that suppressing a real finding:

1. A marker is honoured **only on a comment tinysweeper itself wrote**, matched
   by login (`findings::prior::is_own_login`) — exact match after stripping a
   `[bot]` suffix, overridable with `TINYSWEEPER_BOT_LOGIN`. A prefix match
   would have accepted an account called `tinysweeper-evil`, which anyone can
   register.
2. A marker must be a well-formed fingerprint: exactly sixteen lowercase hex
   characters. Nothing else in a body is ever read as one.
3. Suppression only removes a duplicate comment, per the section above.

Getting the bot login wrong fails in the noisy direction — nothing is recognised
as our own, so nothing is deduped — rather than in the direction that lets a
stranger silence a review.

## Degrading

Every path here is best-effort in the direction of repeating itself rather than
going quiet:

| What fails | What happens |
|---|---|
| Reading comments from the forge | Nothing known; findings may repeat |
| Reading the store | Re-review from scratch, full input price |
| Writing the store | Next review costs more; the verdict still publishes |
| No store at all (`local-review`, tests) | Dedupe works off the markers alone |

`review.incremental = false` opts out of the whole mechanism, for anyone who
would rather have a duplicate comment than a suppressed one.

## Re-review continuity

Prompt layer 4 lists the titles of prior findings and appends
`CONTINUITY_CONTRACT` from `harness::prompt`: verify each earlier finding first,
never re-raise a fixed one, never silently drop an unfixed one. That contract
was unreachable in production while `prior_findings` was always empty.

What the lane resolves is carried through to `LaneProposal::resolved` and
rendered into the check-run summary, so an author sees progress rather than only
new objections. A prior finding that is neither repeated nor declared fixed is
counted as still open in the summary, so it cannot vanish between two pushes.

## Layout

- `types.rs` — `ReviewedState`, and the key a pull request is stored under.
- `memory.rs` — the always-compiled offline store. Not a stub: it is what the
  dedupe tests run against and what `local-review` uses.
- The trait is `ports::review_state::ReviewStateStore`; the MongoDB adapter is
  `impl ReviewStateStore for Store` in `server/store.rs`, behind `serve`.
