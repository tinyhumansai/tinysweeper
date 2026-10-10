# `flows` — how a lane's reviewers are asked

Every model-calling lane asks its reviewers through `flows::runner`: one
structured call per reviewer, all at once, under one shared budget. This
document is why, and what the shape buys.

## The change in one line

A lane's reviewers stopped running one after another. They run concurrently —
with the budget enforced somewhere that does not require serialising them — and
a reviewer may ask the codebase a question instead of guessing.

This used to be expressed as [tinyflows] graphs. Every graph was the same flat
shape — a trigger, one `agent` node per call, a merge barrier — so they are now
plain futures (`futures::future::join_all`), which removed a dependency, a JSON
envelope that had to be read two `json` hops deep, and a set of refusing
capability stubs the engine required. Nothing about what runs or in what order
changed; the golden tests and the lane tests pass unchanged.

[tinyflows]: https://github.com/tinyhumansai/tinyflows

## What runs

`src/council` decides **who** reviews — agents, personas, and what becomes of
their findings. This module is **how they run**: one call per reviewer, concurrent, joined
before anything is read.

```
  evidence ─┬─ reviewer-a ─┐
            ├─ reviewer-b ─┼─ join ─► one answer per reviewer
            └─ reviewer-c ─┘
```

Placement, merging and removal stay where they were — in the lane, in
`council::merge`, and in `falsify` respectively. Those are the steps the golden
tests pin.

## What is deliberately absent: a verification round

An earlier version ran one — every finding put to independent judges, majority
keeps it. `src/falsify` argues that a checker seeing less than the reviewer did
rejects whatever it cannot confirm, which deletes exactly the findings that
needed context to notice. That argument is right, and the round is gone.
Removal is falsify's job; it rejects only what it can *prove* wrong from the
diff, and it fails open. Agreement between reviewers only ever ranks.

## Sub-agents: asking instead of guessing

Off by default (`council.subagents`). A reviewer may end its turn with
**questions** rather than a hedged finding; each is answered by a sub-agent
against the same evidence, and that reviewer is asked **once** more with the
answers in hand. What it says on that turn is what counts.

```
  reviewer ──asks──► ┌─ sub-agent: q1 ─┐
                     ├─ sub-agent: q2 ─┼─► answers ──► reviewer, once more
                     └─ sub-agent: q3 ─┘
```

This makes a reviewer *find more* — the same direction `council` argues for a
second reviewer, and the opposite of asking whether the first was right.
Nothing here can remove a finding.

Cost is shaped rather than merely capped:

- A reviewer with **no questions costs exactly one call**, as before.
- One that asks costs at most three cheap sub-agent calls plus one more turn.
- If **every** sub-agent fails, there is no second turn — re-asking with no new
  evidence is the same turn at full price.

### The depth bound is structural, not a counter

Exactly one level. A sub-agent is a single call built by
`subagent::answer_call`, answering `subagent::answer_schema` — which has no
`questions` key and no `lookups` key. A sub-agent therefore has nothing it
could ask with and no turn after its answer to ask on. A depth integer threaded
through the run is a bound a future edit deletes by accident; this one is a
property of the schema, and `subagent_test` pins it.

### Two couplings that fail silently if broken

- **The instruction and the schema travel together.** A reviewer told it may ask,
  answering a schema with no `questions` key, is rejected under strict mode and
  silently truncated under `json_object`. `with_questions` therefore creates
  `properties` when a schema lacks it rather than returning the schema
  unchanged.
- **The final turn is not offered a way to ask again.** There is genuinely no
  turn after it, so leaving `questions` in its schema invites a question nothing
  will ever answer.

## Lookups

The other follow-up, and the one that pays: a reviewer may end a turn with
reads and searches of the repository instead of a verdict, and is asked again
with what came back. The loop is host-owned — the model fills a JSON field,
`flows::lookup` answers it through the `TreeReader` port — so the `Model`
port stays one structured completion and every turn is a cassette can replay.
Lookups run before questions, so a sub-agent answering a question is handed
the evidence the reviewer already fetched rather than the diff alone; before
this it was handed the reviewer's own prompt and told it was answering "from
the repository". The turn prompts say what each turn may do: the settling
turn alone is told it is the last. See
[`docs/modules/lanes/lookup.md`](../lanes/lookup.md).

## What a reviewer is *not* able to do

A reviewer's only capability is answering a schema. There is no tool, HTTP,
code or shell path in `flows` for it to reach — not refused at run time, but
absent: a `Call` is a system prompt, an evidence suffix and a schema name, and
the model it reaches is a stateless completion (`harness::openrouter`, over
OpenHuman's `Completer`) that declares no tools. Repository reads a reviewer
asks for are performed by the host, through the read-only `TreeReader` port.

## Where the budget lives

In `caps::ModelCapability`, checked before each call. This is what let the
per-file fan-out become concurrent again: the previous design serialised every
file *precisely because* spend is only known once a call returns, so there was
nowhere else to enforce a ceiling. One capability object sees every call in a
lane, so it can refuse one however many are in flight.

One call here is one changed file **or one file group** — `lanes::grouping`
decides which, before any of this runs, with no model call of its own. A file
and its test grouped into one conversation is one call charged against the
budget instead of two, at the cost of one prompt carrying both diffs; a
component too large to bet on falls back to the ungrouped count exactly. Either
way this module counts calls the same way — it has no notion of a "file"
beneath a `Call`, only the id and the prompt it was given.

The default maximum `review.passes = 3` adds up to two more calls per qualifying
group — adaptive coverage passes (`lanes::coverage`) that ask one reviewer
rather than the whole council and stop when a pass adds nothing distinct — and
each goes through the same `ModelCapability`, so
it counts against the same budget as everything else here. See "Coverage
pass" in `docs/modules/lanes/README.md`.

## Files

| file | role |
|---|---|
| `caps.rs` | the lane's one capability: the model call, budget and spend tally |
| `panel.rs` | the `Call` each reviewer makes, and the per-file concurrency cap |
| `subagent.rs` | the sub-agent call, the question schema, and the depth bound |
| `lookup.rs` | the lookup loop: seeding, the `lookups` schema, gathering, the budget |
| `runner.rs` | runs the rounds — lookups, then questions, then the settling turn — and returns one answer per reviewer |

## Testing

Everything here is offline. `MockModel::panel` answers a whole council from one
lane response, dispatching on the schema each call asks for, so a golden test
still reads "given a model that says exactly this, the lane must post exactly
that" without depending on call order. `MockModel::panel_matching` answers per
file or per reviewer, for the tests that are about two of them behaving
differently.

Two properties are asserted rather than assumed, because both are invisible
when they break:

- **Concurrency** is measured by peak in-flight calls, not wall clock. A serial
  runner never exceeds one; the test asserts it reached the reviewer count.
- **Cost shape** for sub-agents is pinned by call count: one call when nothing
  is asked, and `1 + MAX_QUESTIONS_PER_REVIEWER + 1` when the cap is exceeded.
