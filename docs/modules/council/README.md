# `src/council`

Several reviewers on one lane's evidence, folded into one review. Always
compiled; nothing here calls a model.

Off by default. `[council]` is on the **not overridable** list in
`src/config/remote`, because every key in it either spends the operator's money
— a second reviewer is a second call per file — or decides what a model is told.
A pull request that could add reviewers to its own review would be no gate at
all.

## It is not a second opinion

`src/falsify` is already that, and [its README](../falsify/README.md) explains
at length why asking a second model *"are these correct?"* deletes the best half
of a review: a checker that saw less than the reviewer rejects what it cannot
confirm, and what it cannot confirm is exactly what needed the extra context.

A council runs in the opposite direction. More reviewers so that **more is
found**; agreement used only to rank what comes back.

## Agreement raises confidence. It never gates posting.

The obvious design is a quorum — post a finding when two of three raise it — and
it is the falsification failure in a different hat. Worse here, because the
architecture is *deliberately built* so reviewers do not overlap:
`harness::prompt::ISOLATION_CLAUSE` exists precisely to stop N conversations
reporting the same cross-file problem. A reviewer with a different angle, a
different slice, or later a tool belt the others lack will find things the
others structurally cannot see. Gating on agreement deletes those, which is to
say the findings that justify running more than one reviewer at all.

So the merge is **monotone**: no finding is ever worse off for the council
having run. Corroboration raises confidence by noisy-OR (capped at 0.99 — three
models agreeing is not a certainty any of them claimed), and breaks ties last in
`lane_proposal`'s sort so a corroborated finding survives the `max_comments`
truncation. A singleton passes through on its own merit, judged by the same
`confidence_min` it would have faced alone.

## Grouping is not the fingerprint

`Finding::fingerprint` hashes the lane, path, **rule** and anchored code. That
is right for its job — deciding whether this is a finding already posted — and
wrong for this one, because `rule` is model-authored free text and two reviewers
on one missing bounds check will write `unchecked-index` and
`missing-bounds-check`. Grouping on the fingerprint would post both.

`agree::corroborates` therefore uses a looser rule: same lane, same file,
anchored ranges overlapping within three lines. It never compares titles or
bodies — two agents describing one defect word it differently by construction,
which is the entire reason for running more than one.

That looseness is used **only** for grouping. `Finding::identity` stays exactly
as `anchor::stamp` computed it, and a merge keeps the identity of the *first*
sighting: it is what the `tinysweeper:fp=` marker carries and what suppression
reads back, so letting the representative bring its own would repost a finding
that had already been answered.

## The representative is verbatim

Where several reviewers describe one defect, one finding is chosen whole —
highest confidence, then highest severity. Nothing is blended, rewritten or
summarised. A merge step that can author text is a second reviewer nobody
gated, which is the same objection `src/falsify` raises to a filter that can
return findings of its own.

Severity is the *highest* anyone assigned rather than the representative's.
Merging must not talk a review down: a reviewer outvoted on wording keeps its
opinion about how much the defect matters.

## A merge never crosses reviewers' own findings

Only findings contributed by *earlier* reviewers may absorb a later one, and
each may absorb at most once per round.

Both halves are load-bearing. Merging within one reviewer's output would make
the council change behaviour at a single agent — two findings three lines apart
would silently become one — which destroys the property the whole rollout rests
on. It is also not the council's job: one reviewer repeating itself is a dedupe
question `lane_proposal` already owns.

This is not hypothetical. It shipped in the first draft and `tinysweeper eval`
caught it on `ts-0068`, where the critique lane legitimately raises two findings
on adjacent lines of one file.

## Personas are names, never text

`src/config/remote` excludes `path_instructions` from what a repository may set,
with the reason stated there: it is free text injected straight into a lane's
instructions, unfenced, and repository prose reaches a prompt through exactly
one door — the sandboxed extraction in `crate::knowledge`. A persona is the same
shape of text in the same position, so it is a `&'static str` in
`council::persona` selected by name, and an unknown name is a configuration
error reported by `tinysweeper check`.

A persona must change *what the reviewer looks at* — the failure classes it
reaches for first — not merely how it phrases the answer. Asking one model the
same question twice at the same temperature produces the same answer twice, and
paying for both is not a council. Each persona also states that other reviewers
are running, for the same reason `ISOLATION_CLAUSE` exists.

## One agent is a provable no-op

With the council off, `reviewers()` yields the lane's own model and the empty
persona, so the prompt is byte-identical to the pre-council one. With one agent
configured and no persona, the same holds and `merge` returns its input
untouched.

That is asserted twice: directly, in `merge_test.rs`, and empirically — the
committed `evals/` corpus replays against its existing cassettes, which are
keyed on the full prompt text. A prompt that had moved by one byte would miss.

## Cost

One extra reviewer is one extra call per file for that lane, plus nothing else:
falsification runs **once over the merged set** rather than per reviewer, since
a reject-only filter given more inputs in one pass has identical semantics at a
fraction of the calls.

Each agent has its own cache stream, because the persona sits in the prefix. Two
agents on one model means two prefixes to warm rather than one — real, and small
against the call itself.

## Files

| File | Role |
| --- | --- |
| `mod.rs` | resolves configuration into the reviewers for a lane |
| `agree.rs` | whether two reviewers found the same thing |
| `merge.rs` | folding them together, monotonically |
| `persona.rs` | the reviewing angles, as in-tree text selected by name |
