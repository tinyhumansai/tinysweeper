# `src/retrieve` — what a reviewer sees besides the diff

Always compiled. Everything goes through the `Embedder`, `ChunkIndex`,
`GraphStore` and `IndexManifest` ports, so the whole pipeline runs offline
against `crate::index::mock` and the default build links no database driver.

A reviewer that only reads the diff cannot tell a rename from a semantic change,
cannot find the second caller that needed the same fix, and re-argues
conventions the team settled a year ago. This module is the answer to the first
two. `src/knowledge` answers the third.

## The pipeline

| step | module | what it does |
|------|--------|--------------|
| 1 | `query` | composes a bounded query from the pull request |
| 2 | `mod` | one hybrid `$rankFusion` query over the chunk index |
| 3 | `expand` | walks the code graph out from the diff and fetches what it reaches |
| 4 | `assemble` | ranks, deduplicates, truncates to a token budget |

### 1. The query is composed, never sliced

The obvious implementation embeds the first N kilobytes of the raw diff. It is
wrong in a way that only shows on the pull requests that need retrieval most: a
fixed slice of a large diff is its *first* files, so every later file is
unrepresented and the reviewer retrieves context for the top of the change and
nothing for the bottom. It also makes embedding cost scale with the one input
that has no upper bound.

So the query is built from four things the diff states about itself, under
per-section caps:

| section | share | contributes |
|-------------|-------|--------------------------------------------|
| title | 5% | what the author says the change is for |
| paths | 20% | where in the repository it lands |
| headings | 20% | the enclosing signatures git already named |
| identifiers | 55% | the names the change actually moves |

The shares are **caps**, which is the load-bearing part: a diff that renames a
directory is hundreds of paths and would otherwise fill the query with directory
names, leaving no room for the identifiers that decide what comes back. Unused
budget flows forward to the identifiers, never backwards.

Where a section does not fit it is **sampled across its whole list**, and the
last entry is always kept. Identifier ties — which in a large diff is almost
every identifier — are broken in diff order rather than alphabetically, and the
band that does not fit is sampled rather than truncated. Both rules exist for
the same reason: a 300-file diff that only ever describes its first forty files
is the failure this replaces.

Hunk headings come from the text git writes after the second `@@`, which
`evidence::diff` retains for this purpose. It is deliberately *not* rendered
back into the evidence text — that output is replayed byte-for-byte for the
prompt cache.

### 2. One hybrid query, fused by the server

`$rankFusion` over a `$vectorSearch` arm and a `$search` arm, weighted. The
fusion is a MongoDB stage, not Rust: hand-rolling reciprocal rank fusion would
mean over-fetching both arms, and hand-rolling a sparse vector for the lexical
arm would throw away real BM25 with corpus IDF — the part that makes a rare
identifier outrank a common word.

The arm is over-fetched threefold, because dedupe and the token budget both
remove candidates and one larger query is cheaper than a second round trip.

### 3. Graph expansion is the step similarity cannot do

A function's caller two files away shares no vocabulary with the diff. That is
the normal case, not an edge case, so it never comes back from a hybrid query
however the weights are tuned. Extracting imports at index time and never
traversing them at review time yields a repository graph that answers no
question anybody asked.

Seeds are the changed file paths **and** `path#symbol` for the symbol each hunk
heading names. The second kind is what makes one hop enough to reach a caller,
and it needs no checkout — which matters, because the forge-only review path has
no file contents. A seed that names nothing is harmless: the store skips seeds
it does not have, and a pull request that adds a file names paths the graph has
never seen.

Three bounds, none redundant. Hops bound the shape of the walk, the node cap
bounds its size, the chunk cap bounds what is fetched. Chunks of the pull
request's own files are excluded: the lane is already reading their diff.

### 4. A stated token budget

Retrieval with no ceiling gets a share of the prompt decided by whatever the
diff happened to weigh. The default is `retrieval.context_tokens = 8000` —
roughly 32 KB, comparable to the diff it accompanies rather than a garnish
beside it, and about 12% of `models.budget_usd_per_pr` across five lanes at the
deep tier's input price.

The two arms are interleaved two search hits to one graph hit. Concatenation in
either order is a silent preference: search-first means a diff with plenty of
lexical neighbours never spends a token on the caller it breaks. Dedupe is by
**line-range overlap**, not by identity, because chunk boundaries move when a
file is re-chunked and the same function comes back from both arms under two
different ranges. Whatever the budget dropped is counted and reported.

## Degrading honestly

Nothing in this module returns an error to its caller. A cold index, a stale
one, an unreachable database or a deployment with no `mongot` all produce a
`RetrievedContext` carrying a `RetrievalStatus` that says which, and the review
runs on the diff alone.

Two properties, and they are not the same one:

- **Losing retrieval must not lose the review.** Handing every contributor a way
  to break the bot by taking a database down would be worse than a thin review.
- **Losing retrieval must not be silent.** `RetrievedContext::note()` returns
  the sentence appended to every non-skipped check-run summary. A `critique`
  check that reports "the change looks sound" while its index was cold is making
  a claim it had no way to check.

`RetrievalStatus::Off` is the one degradation that is *not* reported: an
operator who set `retrieval.enabled = false` does not need telling on every
review. A missing manifest is likewise not reported — no record is not evidence
of staleness, and an operator who chases a stale index that is fine stops
believing the notice.

## Where it sits in the prompt

The **suffix**, always. Retrieved context is composed from this diff, so it
differs on every push and every pull request; a prefix carrying it would never
hit the prompt cache once while producing output nobody could tell was wrong.
`harness::prompt`'s `the_prefix_is_identical_when_only_new_evidence_changes` and
`retrieved_context_lands_in_the_suffix_and_never_in_the_prefix` guard that.

It is fenced and labelled `repository-context`, framed as code that is *not*
part of the change: a finding about it would be a comment on somebody else's
work, which is the fastest way for retrieval to make a review noisier rather
than better.

## Cost

Embedding the query is billed through `harness::pricing` and lands in
`Usage::embed_tokens`, never `input_tokens` — the cache hit rate has to stay a
statement about prompts. The call is billed as soon as it returns, whether or
not the vector was ever used.

## Not yet wired

`src/server/routes.rs` still calls `review_with_context`, which passes no
retriever. The reason is a missing adapter, not a decision: nothing in this
build implements the `Embedder` port against a real provider, so a server-side
`Retriever` could only ever report `Cold`. The seam is
`app::review::review_with_retrieval`.
