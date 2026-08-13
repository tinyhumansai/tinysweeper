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

## The three rounds

```
                    ┌─ lens: input ───┐
  evidence ─────────┼─ lens: authz ───┼─ merge ─┐
                    └─ lens: adjudicate ┘       │
                                                ▼
                                     consensus::propose  (union, deduped)
                                                │
                          questions ─► sub-agents (one level deep)
                                                │
                                                ▼
                        each proposal ─┬─ verifier 1 ─┐
                                       ├─ verifier 2 ─┼─ majority ─► findings
                                       └─ verifier 3 ─┘
```

1. **Propose.** Each lens reads the same evidence with a different charter, in
   parallel. Output is **unioned**, not voted on.
2. **Answer.** A lens that could not settle something asks instead of guessing;
   each question goes to one sub-agent.
3. **Verify.** Every proposal faces independent judges asked to *refute* it. A
   majority keeps it; a tie or worse drops it.

### Why the propose round is not a vote

This is the design decision most worth understanding. A panellist reading for
missing test coverage is not competent to notice a widened trust boundary. If
findings needed agreement to survive, specialisation would destroy exactly the
findings it exists to produce. Majority voting only means anything when the
voters were asked the same question — which is true in round three and false in
round one.

`consensus.rs::a_specialists_lone_finding_is_not_discarded_for_lack_of_agreement`
is the test that pins this.

### Why verification defaults to dropping

A verifier that cannot tell answers `false`. Arguing with a contributor about a
problem that is not there costs far more than missing one finding — and an
unverified proposal is dropped rather than trusted, so a verifier outage makes
the review quieter, never louder.

## Sub-agents, and the one level of depth

The bound is **structural, not a counter**. `subagent::answer_graph` contains a
trigger and one `agent` node, and `caps::ChildGraphs` is populated only with
graphs this crate builds. A sub-agent therefore has no `sub_workflow` node to
reach for and no registry entry it could name if it had one. A depth integer
threaded through the run is a bound a future edit removes by accident; this one
fails to compile a recursion into existence.

Cost is the reason: files × lenses × questions is already the widest part of a
review, and a second level multiplies it again for answers about evidence
nobody has looked at directly.

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
