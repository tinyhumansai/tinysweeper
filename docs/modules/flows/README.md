# `flows` — lane orchestration as a graph

Every model-calling lane runs as a [tinyflows] `WorkflowGraph` rather than as
hand-written concurrency. This document is why, and what the shape buys.

[tinyflows]: https://github.com/tinyhumansai/tinyflows

## The change in one line

A lane's reviewers stopped running one after another. They run as a graph — all
at once, with the budget enforced somewhere that does not require serialising
them — and a reviewer may now ask the codebase a question instead of guessing.

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

## Who reviews, and why they differ

`council` picks the reviewers; a **persona** decides what each one looks at.
The diversity that pays is diversity of *subject* — two reviewers reading the
same file for the same failure class are a duplicate with a bill attached.

| persona | subject |
|---|---|
| `correctness` | this code on its own: boundaries, ordering, the empty case |
| `integration` | callers and contracts it does not show |
| `adversary` | what a hostile input does with it |
| `resilience` | what happens when something it depends on fails |
| `data` | what becomes of records written before this shipped |
| `style` | consistency with the surrounding code |

`style` is the exception in every way. It is the noise every other rule in this
repository exists to suppress, and it is the one subject a model will always
find *something* to say about. So `persona::ceiling` caps its findings at `low`,
below the default severity gate: they reach the check-run summary and never
become inline comments. The cap is in code rather than configuration — an
operator who could raise it would have an uncapped style reviewer, which is the
thing being prevented. A capped reviewer also yields the check-run headline to
any uncapped one, whatever order they are configured in.

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

### Sub-agents have tools, and loop

Each sub-agent may call `read_file` or `search` over the tree at the revision
under review, up to `MAX_TOOL_ROUNDS` lookups before it must answer. Every
question loops independently: one settled on the first turn stops there while
another is still on its third lookup.

```
  sub-agent ─► tool_call ─► read_file ─┐
       ▲                               │
       └───────── result ◄─────────────┘   ×3, then it must answer
```

**The invocation is host code, not a capability.** The graph's `tools`
capability is still `NoTools` — a model's `tool_call` arrives as a field in its
*structured output*, and `runner` decides whether to honour it. The engine has
no door of its own to open.

Three things bound the loop, and they bound different failures:

- **Rounds** (`subagent::MAX_TOOL_ROUNDS`) bound how long one question takes.
- **Bytes** (`tools::MAX_TOTAL_BYTES`) bound what it drags into the prompt.
  This is the limit that matters: a tool call costs nothing to make, but its
  result is re-sent on every later turn, so twenty reads are billed twenty times
  over. Every other cost control here counts *model calls* and would not see it.
- **The last pass offers no tools at all**, for the same reason the reviewer's
  own final turn drops `questions`: a call nothing will answer.

A truncated read says so in its text. A reviewer shown the first 24kB of a file
with no marker has been told the file ends there, and "the cleanup is missing"
is exactly the finding that gets invented from that.

### What the tools cannot reach

`ReadOnlyTools::safe_path` rejects absolute paths, `~`, `..` and drive letters
in front of *every* corpus rather than trusting each one. The arguments come
from a model that has just read a pull request body — untrusted input — so
`../../.ssh/id_rsa` is a thing that gets asked for.

`ports::corpus::Corpus` exists as a separate port rather than a borrow of
`Forge` for the same reason: `Forge` can comment, label, approve and merge, and
handing it to the thing that executes model-chosen calls would put every write
method one slug away. `Corpus` has no write method to reach.

`ForgeCorpus` pins the revision at construction, so a sub-agent cannot name a
SHA and read another branch. It also refuses to search: a forge's code index
covers the default branch and lags, so "this appears nowhere else" would be a
false conclusion about the branch under review. `Corpus::search` returns
`Ok(None)` — *cannot search* — which is deliberately not the same value as
`Ok(Some(vec![]))`, *searched and found nothing*. The two support opposite
conclusions.

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
| `panel.rs` | one `agent` node per reviewer, and the fan-in barrier |
| `subagent.rs` | the child graph, the question schema, and the depth bound |
| `runner.rs` | runs the rounds and returns one answer per reviewer |

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
