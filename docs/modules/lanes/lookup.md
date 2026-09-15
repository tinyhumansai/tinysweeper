# Looking things up

How a reviewer reads the repository before it answers, and why that is a
loop the host runs rather than a tool the model holds. The code is
`src/flows/lookup.rs` (the loop), `src/ports/tree.rs` (the port), and
`src/forge/tree.rs` (reading through the forge API).

## The miss that made this

opencompany#2313 bumped a vendored submodule and turned a one-turn loop into a
round loop. The critique lane, one conversation per file on the diff alone,
approved it. Codex and CodeRabbit each found a boundary bug in the one file
that mattered: the pull request passed `round_start` — documented as
*inclusive*, "a peer's row above this is withheld" — into `read_pinboard`,
whose `before` is documented as *exclusive*, and left a sibling read four
lines below unbounded.

The production model's own summary for that file, from the cassette:

> One concern arises around the `round_start` field used to bound pinboard
> reads and `project_for` — its type and origin are not visible in this diff,
> and if it does not match the transcript start used by `step` the bounds
> would be silently wrong. No introduced bug is evident, but the missing
> context prevents full confidence.

It had the right doubt. The prompt told it *"everything you can see is in this
prompt … a claim that depends on code you were not shown is a claim you cannot
make: lower its confidence, or drop it"*, and it dropped it. Both reviewers
that found the bug were the two that read `read_before` first. See
`evals/cases/oc-2313-round-boundary-leaks.toml`.

## What a reviewer may do now

Two verbs, both reads, on the `TreeReader` port:

| lookup | answers with |
|---|---|
| `read` — a path and a line range | up to 200 numbered lines, and the file's length |
| `search` — a literal, optionally under a glob | up to 30 `path:line: text` hits |

Nothing here runs anything. The security boundary says contributor code is
read and never executed; a port whose only verbs are *read* and *search*
cannot be argued into building or installing, and every implementation is
built over a read handle. The model still holds no tool: it fills a `lookups`
field in its JSON answer, the host answers it, and the host decides what the
field is worth.

## The loop

```text
seed      the host reads the definitions the changed lines call into
turn 1    reviewer: verdict, or `lookups`
gather    host answers them; repeats and budget overruns are named, not run
turn 2    reviewer, with "## What you looked up" appended
…         up to `[lookup].rounds`; the last permitted turn offers no `lookups`
settle    the plain schema, told it is the last turn
```

**Seeding is the part that pays.** An identifier on an added line followed
by `(`, or a `Capitalised` one followed by `::` or `{`, is searched for as a
definition — `fn name(`, `struct Name` — and the doc comment and signature
above the hit are read. A same-file method the change calls is read further,
because the unbounded sibling on #2313 was a hundred lines into one. The
reviewer that asked for exactly these by name found the bug; the one that did
not ask did not. Fetching them unasked removes the difference, and it costs
reads rather than a model turn.

Two host-side follow-ups save a round each: a glob-scoped search that finds
nothing is retried across the whole tree and the retry is named — the
definition a changed line calls into is routinely in a vendored submodule the
reviewer scoped out — and a hit that is itself a definition line is followed
on the spot.

## The bounds

`[lookup]` is not overridable by a reviewed repository: every round is a
model call the operator pays for.

| key | default | what it bounds |
|---|---|---|
| `rounds` | 2 | follow-up turns per reviewer |
| `per_round` | 4 | lookups one turn may carry |
| `max_chars` | 40 000 | text one reviewer may accumulate, seeding included |
| `checkout` | true | fetch a shallow checkout per review so search works |

They are enforced in the loop as well as in the schema, because under
`json_object` a schema is a request the provider does not check. The shape
matters more than the size: two rounds of four is enough to follow one
definition and check one sibling, and not enough to wander.

## Where the tree comes from

| deployment | reader | search |
|---|---|---|
| server, `lookup.checkout = true` | `DirTree` over a shallow checkout, `ForgeTree` behind it | yes |
| server, `lookup.checkout = false` | `ForgeTree` — one `file_at` per read | no; the reviewer is told, and names paths |
| `local-review` | `DirTree` over the working directory | yes |
| `eval run --record --tree <dir>` | `RecordingTree` over `DirTree`; outcomes written into the fixture | yes |
| `eval run` (replay), `cargo test` | `MockTree` over the fixture's recorded outcomes | as recorded |

The forge reader follows one level of submodule: `.gitmodules` at the head
names the path and remote, `ForgeRead::submodule_at` gives the gitlink, and
the file is read from that repository at that commit — when the operator has
listed that repository in `retrieval.submodules`. Nothing else is followed:
the `.gitmodules` URL is contributor-controlled, the read token would follow
it into a private sibling under the same owner as readily as anywhere, and
neither same host nor same owner is authorization. The same list governs
which submodules the indexer fetches into its checkout.

A checkout that has an empty directory where a submodule belongs answers
*unavailable* for paths under it, not *not found*. The difference is a false
positive: a reviewer told a manifest was missing reported it missing.

## What the falsifier sees

`src/falsify` is handed what the reviewer read, alongside the diff, and is
told in so many words that a comment cannot disprove a finding. Before that,
the one correct finding on #2313 survived the reviewer and was deleted by the
filter, which quoted the diff's own comment — *"this confirms the intent to
exclude `round_start` itself"* — as proof. The comment was the claim under
review.

## What it costs, and what it found

On the #2313 case, five lanes, default model: $0.23 before, $0.17 after, with
50 of 58 files verified as the rename and never sent to a model. On the
critique lane with a code-review-tuned model: the exclusive-bound finding,
three runs out of three, at 0.67–0.81 confidence — below the posting gate,
named in the summary as *worth a look*. The unbounded sibling read is missed
by every one-shot configuration and by that model; `gpt-5.6-luna` on the
box's ladder reaches it two runs in three at a fiftieth of the price
(tinysweeper#157), which is why it became the `deep` tier.
