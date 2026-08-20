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

No sibling signature was added. Two identities would drift, and the exclusions
triage inheritance needs are the same exclusions dedupe needs.

### The anchor, for what the identity cannot catch

The identity recognises a finding described *identically* twice. Two of its four
inputs are model-authored — the `rule`, and the snippet quoted as the anchored
code — so a re-review that quotes one line either side of last push's snippet
mints a fresh fingerprint for a concern that has not moved. `src/eval/runner.rs`
on `tinysweeper#86` carried one such concern posted three times over two pushes,
at lines 146, then 145 and 171, under three fingerprints. Dedupe did exactly what
it was written to do and never fired.

So `already_posted` asks a third question after the two fingerprint checks: is
there already one of our comments in the same lane, on the same file, **under the
same title**, within three lines. It is asked last because it is weaker evidence
than a fingerprint.

The title is the content guard, and it is not optional. Position says two
findings are in the same place; it does not say they are the same finding, and
two defects a few lines apart in one function are ordinary. Without the guard the
second is silently deleted — and a deleted finding can flip a verdict, which is
the failure this whole mechanism exists to prevent.

The title rather than the `rule`, on the evidence: the four repeats of one
concern on `tinysweeper#86` carried the rules `untrusted-repo-rules`, `Pin
third-party actions to a commit SHA`, and twice nothing at all, while the title
was identical every time. Keying on the rule would leave the fallback catching
nothing in the exact case it exists for.

The remaining clauses are the same refusal to act on thin evidence: both findings
must be placed on a line, a comment whose lane or title cannot be read anchors
nothing rather than everything, and anchors are read off the live pull request
rather than the store, so a comment a maintainer deleted stops suppressing.

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

Prompt layer 5 lists prior findings as `severity — title` and appends
`CONTINUITY_CONTRACT` from `harness::prompt`: verify each earlier finding first,
never re-raise a fixed one, never silently drop an unfixed one, and report one
that still stands at the level it already has. That contract was unreachable in
production while `prior_findings` was always empty.

The severity is carried in `ReviewedState::severities` and read back off the
comment's own priority badge, with the forge winning where the two disagree — the
comment is what the author is looking at, and it is the copy a human can edit or
delete. Where one title has two levels already, the earliest holds: following the
newest would ratify the drift rather than stop it.

`lane_proposal` then pins any finding whose title it has seen to that level,
before the conclusion and the request-changes verdict are computed. The prompt
alone is not enough here, because severity is not derived from anything: two runs
over identical code can return different levels and neither breaks a rule the
model was given. `tinymemory#13` flipped from changes-requested to approved ten
minutes later on exactly that. Only an exact title match pins — a reviewer that
reworded the finding has re-made the case for it, and is free to argue a
different level.

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
