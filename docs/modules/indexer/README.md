# `indexer`

Keeping a repository's chunk index current, and knowing what it cost.

`chunk` decides what a chunk is. This module decides *when* to make one, and the
answer it is built around is "as rarely as possible".

## Files

| File | Contents |
| --- | --- |
| `types.rs` | `IndexState`, `RepoIndex`, `Claim`, `IndexLease`, `Settled`, `IndexedFile`, `IndexReport`, `IndexOutcome` |
| `cost.rs` | `EmbedUsage`, the embedding price table, `estimate_tokens` |
| `mock.rs` | `MockManifest`, `CountingEmbedder` — always compiled |
| `run.rs` | `Indexer` — the run itself |
| `fetch.rs` | `Checkout` — a shallow, hook-disabled fetch of one commit. Behind `serve` |
| `mongo.rs` | `MongoManifest`. Behind `serve` |

The port is `src/ports/manifest.rs`. The server-side driver — which repository
gets indexed, when, and with which permit — is `src/server/indexing.rs`.

## Getting the tree without running it

Indexing reads a repository's tree, so the server has to have one. `fetch.rs`
shallow-fetches exactly one commit into a temporary directory that is deleted
when the `Checkout` is dropped. This is the crate's only process spawn, and a
test pins that the only program it will ever spawn is `git`.

Reading a tree is not running one, and the two ways git could have run something
are closed rather than left to defaults: `core.hooksPath` is pointed at nowhere,
so a hook in the fetched history cannot fire, and `GIT_TERMINAL_PROMPT=0` stops
a credential prompt turning a fetch into a hang. Nothing builds, installs or
executes anything from the fetched tree.

The installation token is not in argv. The obvious `https://x-access-token:TOKEN@…`
remote URL puts a credential in the process table for anything on the host to
read; git's `GIT_CONFIG_KEY_n`/`GIT_CONFIG_VALUE_n` protocol carries the same
`http.extraHeader` through the environment instead.

## Write first, delete afterwards

The obvious order is *delete this repository's chunks, then embed and write the
new ones*. It is one line, and it is trivially correct when it finishes. When it
does not — a provider timeout, a killed process — it leaves the repository with
**zero** chunks and a failed status, and it stays that way until somebody
notices reviews stopped citing anything.

So the run is:

1. Chunk the changed files and diff their chunk ids against the manifest.
2. Record the ids about to be written, as *pending*.
3. Embed and upsert the ids that are new.
4. Confirm those ids in the manifest.
5. Delete the ids the previous pass had and this one does not.

An interruption anywhere leaves the index a superset of the truth, which
degrades retrieval slightly. The reverse order leaves it a subset — usually the
empty set — which breaks it silently.

Step 2 is the crash-safety half. Without it, a run that died between embedding
and recording would leave chunks in the index that no manifest entry mentions:
the next run would neither reuse them nor ever delete them, and stale code would
sit in retrieval forever. `IndexedFile::pending` is deliberately *not* counted
as indexed — doing so would skip embedding a chunk that was never written.

`ChunkIndex::delete_chunks` exists for step 5. `delete_paths` can only run
*before* the new chunks are written, which is exactly the ordering being
avoided.

## Unchanged content costs nothing

A chunk's id contains the SHA-256 of its text. An id already confirmed in the
manifest is content already embedded, so it is skipped without a call. Re-index
an untouched checkout and the embedder is not called once — asserted by
`re_indexing_unchanged_content_issues_zero_embedding_calls`, using
`CountingEmbedder`, because the promise is a negative and nothing about the
produced chunks demonstrates it.

At the coarser grain, a run whose revision the manifest already records returns
`IndexOutcome::AlreadyFresh` without walking anything.

The same finding is reported, not just acted on. `IndexReport::changed` names
the files whose chunk ids differ from the manifest's and `IndexReport::removed`
names the ones whose chunks went, so the code graph can re-parse the same subset
instead of the whole tree — see `docs/modules/graph/README.md`. `changed` is a
set comparison rather than a count, because a file can gain and lose a chunk in
one edit and keep its length. On a first index every file is changed, which is
the right answer: nothing about it was known before.

## A claim, not a lock

Two workers indexing one repository corrupt nothing but pay twice and race on
the stale sweep. The claim is a single insert against a unique index — the same
shape `server::store` uses for delivery claims and review leases — with the
duplicate-key error as the answer rather than an exception.

A refused claim returns `IndexOutcome::Requeue`. It never waits: a worker
blocked on a lock is a worker not reviewing anything else. Claims also expire on
a TTL, because an explicit release cannot run if the process is killed and a
stranded claim would mean that repository can never be indexed again.

`IndexState` has four states, and the useful thing is which one a failure lands
in: `Failed`, with everything the run managed to write still in place and still
queryable, and still claimable so a retry is possible.

## Spend is counted here because nothing upstream counts it

tinyagents tracks tokens and dollars for completions and gates on them; its
embedding calls go through a different client and are accounted nowhere at all.
Indexing a monorepo is easily tens of millions of tokens, so "not counted" is
not "not much" — it is an unpriced surprise, first visible on an invoice.

Two honesty caveats, both deliberate:

- The token count is an **estimate** — four bytes per token — because the
  `Embedder` port reports vectors, not usage. Counting exactly would mean
  shipping a tokenizer per provider to price a call already made.
- A model with no price on file costs zero and logs a warning, rather than being
  given an invented rate that would make a budget check meaningless. Same rule
  as `harness::openrouter`.

`Indexer::with_budget` stops at a batch boundary and reports
`IndexReport::budget_exhausted`. It does not fail and does not discard what it
wrote, and it does **not** record the revision — a partial index that claims the
revision would make the next push skip the work that was never finished.

## A batch is bounded by tokens, not only by count

`embeddings.batch` says how many texts go in one embedding call. It does not
bound the call, because providers cap a request by **tokens**, and 64 chunks of
real source are not the same size as 64 chunks of anything else.

That gap was a production bug rather than a theoretical one. Once the gateway's
credentials were fixed, indexing got far enough to be rejected outright:

```
openrouter embeddings returned 400 Bad Request:
  "Requested 467846 tokens, max 300000 tokens per request"
  code: max_tokens_per_request        repo="tinyhumansai/openhuman"
```

Large repositories therefore indexed nothing and reviewed from the diff alone,
while small ones succeeded — which is the worst shape for a bug of this kind,
because the failure is loudest exactly where retrieval matters most and absent
where anyone would have noticed it.

`batch_bounds` now closes a batch on whichever ceiling binds first: the count,
or `embeddings.max_request_tokens`. Two properties are load-bearing and are
each pinned by a test:

- Every queued chunk lands in **exactly one** batch. A dropped chunk is a file
  missing from the index that nothing downstream would ever report.
- A single chunk larger than the ceiling gets a batch of its own rather than
  being skipped or wedging the loop. The provider will reject that call, but an
  error naming the limit beats a silent gap or an index that never finishes.

The default is 120,000 estimated tokens against a 300,000-token provider limit,
and the factor of two is not padding. `estimate_tokens` assumes four bytes per
token, which is about right for prose and roughly **half** the true rate for
code. The rejected batch is the measurement: 64 chunks at the chunker's
14,400-char cap is at most 230,400 *estimated* tokens, and the provider counted
467,846 real ones. So a ceiling expressed in estimated tokens has to be halved
before it means anything to the provider enforcing it.

A deployment on a provider with a different limit sets
`embeddings.max_request_tokens`. Zero is read as the default rather than as
"unbounded" — an unbounded batch is the bug, so a config typo must not be able
to switch it back on.
