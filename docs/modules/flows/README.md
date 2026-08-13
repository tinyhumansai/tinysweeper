# `flows` — lane orchestration as a graph

Every model-calling lane runs as a [tinyflows] `WorkflowGraph` rather than as
hand-written concurrency. This document is why, and what the shape buys.

[tinyflows]: https://github.com/tinyhumansai/tinyflows

## The change in one line

One expensive model call per file became a **panel** of cheap ones that propose,
then a round of independent judges that verify — and only what survives
verification reaches a contributor.

## Why a panel is cheaper *and* quieter

The deep tier was buying two different things at once: a better reader, and a
noise filter. Splitting them is what makes this work.

Measured against the pinned DeepSeek endpoint (see `config/defaults.toml`),
blended at the ~80% prompt-cache hit rate this prompt layout achieves:

| tier | input $/M | cache read $/M | blended |
|---|---|---|---|
| `deep` (`deepseek-v4-pro`) | 0.435 | 0.003625 | $0.090 |
| `flash` (`deepseek-v4-flash`) | 0.14 | 0.0028 | $0.030 |

Three flash opinions cost roughly what one pro call cost. The noise filter is
no longer "the model was good enough not to say something silly" but "the claim
survived three attempts to refute it", which is a property this crate can test
offline.

## What runs

`src/council` decides **who** reviews — agents, personas, and what becomes of
their findings. This module is **how they run**: one `agent` node per reviewer,
concurrent, joined by a merge barrier.

```
  evidence ─┬─ agent: reviewer-a ─┐
            ├─ agent: reviewer-b ─┼─ merge ─► one answer per reviewer
            └─ agent: reviewer-c ─┘
```

Placement, merging and removal stay where they were — in the lane, in
`council::merge`, and in `falsify` respectively. Those are the steps the golden
tests pin, and moving them into a graph would buy nothing and cost the tests.

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

Exactly one level. `subagent::answers_graph` contains a trigger and `agent`
nodes, nothing else, and `caps::ChildGraphs` is populated only with graphs this
crate builds. A sub-agent has no `sub_workflow` node to reach for and no
registry entry it could name if it had one. A depth integer threaded through the
run is a bound a future edit deletes by accident; this one cannot compile a
recursion into existence.

### Two couplings that fail silently if broken

- **The instruction and the schema travel together.** A reviewer told it may ask,
  answering a schema with no `questions` key, is rejected under strict mode and
  silently truncated under `json_object`. `with_questions` therefore creates
  `properties` when a schema lacks it rather than returning the schema
  unchanged.
- **The final turn is not offered a way to ask again.** There is genuinely no
  turn after it, so leaving `questions` in its schema invites a question nothing
  will ever answer.

## What the graph is *not* allowed to do

`caps.rs` is as much about refusal as wiring. `tools`, `http` and `code` are
supplied as implementations that deny every call with an error naming the
invariant from `AGENTS.md`; `shell` and `memory` are absent entirely. A graph
that grows a `code` node fails on its first run with the reason, rather than
quietly executing contributor code.

## Where the budget lives

In `caps::ModelCapability`, checked before each call. This is what let the
per-file fan-out become concurrent again: the previous design serialised every
file *precisely because* spend is only known once a call returns, so there was
nowhere else to enforce a ceiling. One capability object sees every call in a
lane, so it can refuse one however many are in flight.

## Files

| file | role |
|---|---|
| `caps.rs` | the capability seam, budget, spend tally, and every refusal |
| `tier.rs` | `scan` / `deep` / `flash` → a configured model id |
| `panel.rs` | the lens sets, and the propose/verify graph builders |
| `consensus.rs` | dedupe by anchor, and the majority rule |
| `subagent.rs` | the child graph, and the depth bound |
| `runner.rs` | drives the three rounds and assembles the outcome |

## Testing

Everything here is offline. `MockModel::panel` answers a whole panel from one
lane response — dispatching on the schema each round asks for, so a golden test
still reads "given a model that says exactly this, the lane must post exactly
that" without depending on the panel's internal call order.
`MockModel::panel_matching` answers per file, for the tests that are about two
files behaving differently.
