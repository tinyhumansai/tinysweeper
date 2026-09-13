# `src/memory` — what the reviewer remembers between pull requests

Always compiled. Everything goes through the `Memory` port
(`src/ports/memory.rs`), so ingest, recall and the prompt block all run offline
against `MockMemory`, and the default build links no HTTP client. The CortexDB
adapter in `src/memory/cortex.rs` is the one file behind the `cortex` feature.

## Why a memory, when there is already an index

`src/retrieve` finds the code that reads like the diff and the code the diff
reaches. `src/knowledge` reads the instruction files on the branch under review.
Both are recomputed from the tree on every push, and both forget everything the
moment the review ends. Neither can answer the two questions that decide whether
a long-running reviewer gets quieter or noisier over time:

- *Has this reviewer raised this before, and what did the maintainers say?* The
  largest source of noise in a bot that reviews a repository for months is a
  finding that was rejected on pull request 40 coming back on pull request 41,
  phrased slightly differently.
- *Which rule in this repository's own guides applies to these paths?* Asked as
  a question and answered with a citation, rather than as a similarity query
  that returns whichever paragraph shares the most words with the diff.

So the memory accumulates, and it is consulted by question as well as by query.

## Three sections

A repository's memory is one scope with three sections (`MemorySection`):

| section | holds | written by |
|---|---|---|
| `code` | the source, chunked exactly as `src/chunk` chunks it for the index | ingest of a checkout |
| `conventions` | the repository's instruction files and guides, one item per heading | ingest of a checkout |
| `reviews` | the findings the reviewer published, and what became of each | the review itself |

They are separate on purpose. "Did the maintainers reject a finding like this?"
has to be answerable without the answer being drowned by a thousand
similar-looking code chunks, and measured against a live engine the section
is the difference between a pointer and a listing (see *Questions* below).

Every item (`MemoryItem`) has a key, a kind, an optional path and symbol, a
title, a body and labels. Writes are idempotent on `content_id()`, which hashes
the key **and** the body: re-ingesting an unchanged tree replays every item and
writes nothing; an edited section is a new memory, not a conflict.

## Ingest: code and conventions

`Ingestor::ingest_checkout` walks a checkout with the same `Selector` the
indexer uses, so `paths.ignore` applies to memory too.

**Code** goes through `Chunker`, so a recollection and a retrieved chunk name
the same span and a lane is never shown two versions of one function. Keyed on
`code:{path}#{symbol}`. It has its own switch (`memory.ingest_code`) because it
is the one ingest that costs real money on a large repository and the one the
index already half-covers.

**Conventions** are the files matching `memory.convention_files` — `AGENTS.md`,
`CLAUDE.md`, `CONTRIBUTING.md`, `README.md`, `.cursorrules`, Copilot
instructions and every `docs/**/*.md` by default — split one item per markdown
heading. The title is the heading path (`AGENTS.md › Security Boundary`), so a
recollection reads as a pointer into the file rather than as a loose paragraph.
A section longer than `convention_section_chars` is split at paragraph
boundaries and never mid-sentence: a truncated rule can say the opposite of what
it said. Headings inside code fences are not headings.

### Which commit

The server feeds memory from the pull request's **base** tip, not its head
(`src/server/memory.rs`). The index is built from the head because retrieval
wants the code the change lives in; memory holds the policy the repository has
committed to, and a memory a pull request could write to before it merged would
be a memory a contributor could poison. A pull request that edits `AGENTS.md` is
remembered once it lands, on the next review of anything.

Ingest runs in the background and never blocks a review, for the same reason
indexing does not: a full ingest of a large repository is thousands of engine
writes, each held until indexed, and a review is expected in seconds. The
review recalls whatever the engine holds right now and says so when that is
nothing. Freshness is tracked per process, so one process ingests a given base
tip once.

### Stale versions are retired, not stacked

CortexDB is an append-only event log with no update route: the same key
offered with an edited body hashes to a different `content_id` and is written
as a second, independent event, never a replacement (see [The CortexDB
adapter](#the-cortexdb-adapter)). Recall would then have no principled way to
prefer the current version of an edited or deleted convention over the stale
one — whichever the engine ranks first wins, which is exactly how an obsolete
`AGENTS.md` rule could keep being recalled as current policy.

So every `ingest_checkout` call forgets the whole `code` and/or `conventions`
section for the repository — whichever it is about to (re-)ingest — before
writing this pass's items. `ensure_ingested` only calls it once the base tip
has actually moved, so a section is never left empty for long: the same call
that forgets it repopulates it in full, in the same background task. The cost
is real — a repeat ingest of an unchanged tree now always rewrites it rather
than replaying content-idempotent no-ops — and it is the trade this adapter
makes for never serving a superseded rule as current. Review outcomes are a
separate section and are never touched by this: they come from review
threads, not the tree, and are covered in the next section.

**Not versioned, not atomic.** Forget and repopulate are two calls, not one:
a recall landing on this process between them sees a partially rebuilt
section rather than either the old or the new one whole. `ensure_ingested`'s
per-repository lock (`server::memory::MemoryBackend::ingest_lock`) keeps two
*ingests* of the same repository from racing each other, but it does not
block a concurrent *recall* — that would need the [`Memory`] port to support
either a per-item delete (so retiring one stale item never requires emptying
the section) or a versioned scope recall could be atomically retargeted to,
neither of which CortexDB's public API gave evidence of supporting safely
when this was built. Until one of those exists, the honest tradeoff is: a
recall mid-window degrades to *less remembered*, never to something wrong.

### Review outcomes are append-only by design, and that is a known gap

Unlike code and conventions, an outcome's key is never retired the same way:
`ingest_checkout` only forgets the `code` and `conventions` sections, because
outcomes accumulate one pull request's write-back at a time and there is no
single "whole tree" moment to recompute a full replacement from, the way a
checkout gives one for the other two sections. A thread first observed as
`rejected` and later reopened, fixed, or reversed by a maintainer still
appends a *new* outcome event under the same logical key rather than
replacing the old one — and recall's dedupe-by-key only runs after relevance
ranking, so an old, since-superseded verdict can still be the one a future
review sees. Fixing this for real needs one of: a per-key delete the
[`Memory`] port does not have, or reading the engine's own event recency
(`recorded_at`) back through recall to prefer the newest version, which needs
a datetime dependency and a round-trip this adapter does not currently make.
Tracked as a known limitation rather than fixed here.

## The review path

`app::review::review_with_memory` consults memory twice and writes it twice, in
this order:

1. **Observe.** The pull request's review threads are read. Every thread the
   reviewer itself opened — matched by login and by its `tinysweeper:fp=`
   marker, never by prefix — that has settled becomes a `ReviewOutcome`:

   | resolved | outdated | human replied | outcome |
   |---|---|---|---|
   | yes | yes | — | `fixed` |
   | yes | no | yes | `rejected` |
   | yes | no | no | `dismissed` |
   | no | — | yes | `disputed` |
   | no | — | no | *(nothing yet)* |

   Only GitHub's deterministic signals are used. No model classifies an
   outcome: it is evidence about the maintainers' judgement, and a model
   inferring it would remember the model's judgement instead. The maintainer's
   latest reply is kept, bounded to `MAX_REPLY_CHARS`, because "this is
   intentional, the caller checks it" is the single most useful thing to know
   before reviewing the next push.

2. **Recall.** The same bounded query `src/retrieve` composes from the pull
   request is put to the `reviews` and `conventions` sections, and to `code`
   only when retrieval showed the lane nothing. Every recall and every question
   runs concurrently: each is a round trip, a question is a model call behind
   it, and running them in sequence puts the whole list on the critical path.

3. **Ask.** Each configured question is templated over the changed paths and
   put to the engine's grounded-answer route, in the section it names.

4. **Remember.** After the lanes run, every finding they produced is written as
   a `ReviewFinding`, so the next review can be told what was said and, once
   the thread settles, what became of it. This happens on the read side
   deliberately: the memory records the reviewer's conclusions, and whether
   `apply` later posts each one is a separate decision that the outcome pass
   reads back off the thread.

Everything comes back under `memory.context_tokens`, answers first — they are
the synthesis, the recollections are the evidence — then outcomes, then
conventions, then code. Whatever the budget dropped is counted.

### Questions

Each `[[memory.questions]]` names a section and a template; `{paths}` becomes
the changed paths (at most twelve, then "and N more") and `{title}` the pull
request title. The defaults ask `conventions` which rules apply to the changed
paths, and `reviews` which earlier findings about them were rejected.

The section is measured, not tidy. Against a live CortexDB, a question over the
`conventions` section came back with the Security Boundary rule quoted from
`AGENTS.md` and a citation resolved to that path; the same question over the
whole repository scope came back with the ten oldest events and "not enough
information". The engine's ranked recall works within a scope that holds
vectors; a parent scope enumerates its children.

An engine that has nothing to say answers with a sentinel the instructions ask
for ("nothing relevant is remembered"), which is filtered out rather than
rendered.

## Where it sits in the prompt

Layer 5e of `harness::prompt`: the volatile suffix, fenced as
`repository-memory`, after the retrieved code and before the diff. Everything
in it is prose somebody other than the operator wrote — a merged `AGENTS.md`,
a maintainer's reply, the engine's own synthesis — so it goes where the model
is told to treat text as data. The framing tells the lane what a *rejected*
outcome means: do not raise it again unless the code is materially different,
and if you must, say why this case differs.

The prefix is byte-identical with and without memory
(`memory_context_lands_in_the_suffix_and_never_in_the_prefix`), which is the
prompt-cache invariant every suffix layer exists to keep.

## Degrading honestly

Nothing here returns an error to the review. An unreachable engine, an empty
memory and a question with no grounded answer all produce a `MemoryContext`
whose `MemoryStatus` says which, and the check-run summary carries the sentence
— *Memory was unavailable (…), so this review ran without it.* — on every lane
that produced a verdict. A reviewer that quietly ran without its memory is
worse than one that says so.

The one exception is boot. `MemoryBackend::open` proves the engine is
reachable before the server starts, and a configured engine that cannot be
reached is a refusal to start rather than "memory off": a silently forgetful
reviewer still posts reviews, just ones that repeat themselves.

## The CortexDB adapter

`memory::cortex::CortexMemory`, behind `cortex`. CortexDB is an append-only
event log with an extraction pipeline behind it:

- `POST /v1/experience?wait=indexed` (and `/bulk`) writes an event and holds
  the response until it is readable, which is what makes ingest-then-recall in
  one process honest. The idempotency key is `content_id()`.
- `POST /v1/recall` ranks events for a query within a scope. Recall asks for
  the events layer only; the extracted layers carry no envelope and cannot
  come back as items.
- `POST /v1/answer` synthesises a cited answer from a recall pack. The pack
  is weighted to ten events plus a few from each extracted layer — measured:
  a pack that lacks the section a question is about answers "not enough
  information" however good the model. Citations arrive as event ids and are
  resolved to paths against the pack this process holds.
- `POST /v1/forget` takes ids, never a wildcard: the ids are listed first, so
  a scope the listing could not read is never wiped on the strength of an
  empty selector.

Scopes are `owner:o/repo:r/section:s`; an id outside the grammar's charset
(`tiny.place`) is hex-encoded under a marked type. Each event's text is a
one-line header the adapter parses back — kind, key, path, symbol — followed
by the title and body as prose, so the engine's extractor reads well-formed
text and a recollection comes back typed.

The credential is read by name from `memory.api_key_env`, held as a sensitive
header, and never rendered by `Debug`, an error or a log. Plain HTTP is
accepted to loopback only; `config::validate` refuses anything else.

CortexDB also ships a native code-intelligence plane (`CORTEX_CODE_PLANE`,
SCIP imports, a separate embedding provider). It is off by default and not
used here: tinysweeper's own chunker already produces the spans the index and
the graph agree on, and one chunking is easier to reason about than two.

## Operating it

```toml
[memory]
enabled = true
endpoint = "https://api-v1.cortexdb.ai"   # or http://127.0.0.1:3141 locally
api_key_env = "CORTEX_API_KEY"
```

`[memory]` is not repository-overridable: it names a credential and an endpoint
the operator's reviewer would talk to.

```sh
tinysweeper memory ingest --repo owner/name --dir .      # seed ahead of the first review
tinysweeper memory recall --repo owner/name "write token apply"
tinysweeper memory ask --repo owner/name --section conventions "Which rules cover src/app/?"
tinysweeper memory forget --repo owner/name --section reviews --yes
tinysweeper doctor                                        # reports the switch and the key
```

`cargo run --features cortex --example memory_review -- . owner/name` runs one
mock review against a real engine and prints the block the lane received. It
is the smoke test for what the offline suite cannot check: that the engine ranks
a section above its neighbours, that a section-scoped question quotes the rule
and names the file, and that a citation resolves to a path.

Latency is the engine's model calls: each question is one, measured at four to
twelve seconds through a local ladder, run concurrently. A deployment that
wants memory without the wait sets `ask = false` and keeps recall.
