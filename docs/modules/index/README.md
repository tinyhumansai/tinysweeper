# `index`

Retrieval: the chunk index, the code graph, and the knowledge base.

A reviewer that sees only the diff cannot tell a rename from a semantic change,
cannot find the second caller that needed the same fix, and re-argues
conventions the team settled a year ago. The three stores answer those in turn —
similarity for "what looks like this", the graph for "what breaks if this
changes", the knowledge base for "what did we already decide".

## Files

| File | Contents |
| --- | --- |
| `types.rs` | `Chunk`, `EmbeddedChunk`, `ScoredChunk`, `HybridQuery`, `EmbedSignature`, `GraphNode`, `GraphEdge`, `Neighbourhood`, `KnowledgeDoc`, `KnowledgeScope` |
| `mock.rs` | `MockEmbedder`, `MockChunkIndex`, `MockGraphStore`, `MockKnowledgeStore` — always compiled |
| `provider.rs` | `ProviderEmbedder` — the real embedder over tinyagents. Behind `harness` |
| `mongo.rs` | `MongoIndex` and the four MongoDB adapters. Behind `serve` |

The ports themselves are in `src/ports/{embed,index,graph,knowledge}.rs`.

## Embedding is billed like everything else

`Embedder::embed` returns `Embedded`, which is vectors *and* a `Usage`. Bare
vectors would make indexing the one model operation with no price attached to it,
and indexing a repository is the largest token count this program produces.
`Embedded::billed` does the accounting for every implementation, so an embedder
has to go out of its way to return unpriced work. Tokens are estimated from the
text rather than read off a response, because providers disagree about whether
they report embedding usage at all and a budget that only holds for the
cooperative ones is not a budget. An embedder missing from the price table is
charged the most expensive known rate.

## The embedding signature is a partition key

`EmbedSignature` is provider, model and dimensionality. It is written onto every
indexed document as `sig` and filtered on by every query arm.

Vectors from two different models are not comparable — cosine distance between
them is a number, but a meaningless one — so an index that mixed them would
return confident nonsense rather than fail. Making the signature a filter means
swapping the embedding model invalidates the index by construction: the old rows
are still there, they simply stop matching. `Embedder::signature()` is on the
trait for the same reason: an embedder that could not name itself would let a
model swap go unnoticed.

The provider-backed embedder has to make that correspondence hold against a
second spelling of the same idea. tinyagents' `EmbeddingModel::signature()`
returns `provider=…;model=…;dims=…`; `EmbedSignature::harness_key()` is the same
string, with a test asserting they are byte-identical, and
`ProviderEmbedder::new` refuses to construct when the configured signature and
the one the model reports describe different spaces. That turns "the index is
answering from the wrong embedding space" — which is silent, and looks like bad
retrieval rather than a fault — into a startup error naming both keys.

## The real provider

`ProviderEmbedder` is a thin adapter over tinyagents' `EmbeddingModel`, exactly
as `harness::openrouter::GatewayModel` is over its completion provider: the
harness owns the transport, the process-global rate limiter and the
`Retry-After` backoff, and this crate owns the signature and the bill. Which
provider is built comes from `[embeddings]` in the configuration — `voyage`,
`openai`, `cohere`, `ollama` or the offline `mock` — and every arm is a model
whose price is on file, because adding one without adding its price is how
indexing escapes the budget.

`voyage-code-3` at 1024 dimensions is the default: the corpus is code, it is
trained for code retrieval specifically, and 1024 is both its native width and
the cheapest to store as float32 `binData`.

Tokens are still estimated. No provider reachable through the harness reports
embedding usage — the trait returns `Vec<Vec<f32>>` and each adapter decodes the
response's `data` array and discards its `usage` object — so there is nothing to
plumb through even for the providers that send one on the wire.
`Embedded::metered` is the constructor a real count would use, and it is unused
on purpose rather than absent, so plumbing one through later is a one-line
change at the call site.

## Proving it end to end

`examples/index_and_retrieve.rs` indexes a checkout with the configured
provider and runs one hybrid query against it. It is declared with
`required-features = ["serve"]` so it never builds in CI: it needs a real
embedding provider and a MongoDB 8.2+ deployment with `mongot`, and the default
`cargo test` must touch neither. What the mocks cannot tell you is whether a
provider returns vectors of the width it advertises and whether MongoDB accepts
them into a `$rankFusion` query; that is what this is for.

```sh
docker compose up -d mongot
cargo run --features serve --example index_and_retrieve -- . "hybrid search"
```

## Why the ports are wider than `add`/`query`

The two-method shape a vector store usually exposes only supports building an
index once. tinysweeper re-indexes on every push, so the operations that decide
whether the thing is usable are the destructive ones: `delete_repo`, and
`delete_paths` before re-adding the files a push touched. Without the second, an
incremental re-index leaves the old chunks of every edited file in place and
reviews start quoting code that is no longer there.

`delete_paths` with an empty list deletes nothing rather than everything. The
destructive reading of an empty list is never the one a caller meant.

## MongoDB, and what it has to be

MongoDB **Community 8.2+** serves `$vectorSearch`, `$search` and `$rankFusion`
natively through the separate `mongot` process. That is recent — it used to be
Atlas-only — and it is the whole reason this is one database rather than Mongo
plus a vector store plus an inverted index.

- **`$rankFusion` does the fusion.** Weighted reciprocal rank fusion over
  sub-pipelines is a server stage. Hand-rolling RRF would mean over-fetching
  both arms, and hand-rolling a TF vector for the lexical arm would throw away
  real BM25 with corpus IDF — the part that makes a rare identifier outrank a
  common word.
- **Vectors are `binData` float32**, not arrays of doubles: four bytes per
  dimension instead of eight plus per-element type overhead.
- **`(repo_id, path)` is a compound index.** Not an optimisation — without a
  path in the index, every incremental delete is a full collection scan, once
  per push, over every repository's chunks.

### The boot assertion

A stock `mongo:` image has no mongot, and an unsupported aggregation stage fails
when the query runs — which is to say on a contributor's pull request, hours
after the deploy, as a red check run nobody can explain.

`MongoIndex::prepare` therefore creates the search indexes, waits for them to
report queryable, and then runs the *real* hybrid pipeline once. `serve()` calls
it before binding. It must not degrade to "retrieval off": a silently unindexed
reviewer still posts reviews, just worse ones.

The probe searches with a unit vector rather than a zero one. Cosine similarity
against the zero vector is undefined and the server rejects the whole
aggregation for it, so a zero probe would fail against a perfectly good
deployment — a boot assertion that cries wolf is worse than none.

Boot is therefore slow the first time against an empty database, and slowest
against a managed one: the indexes are created and then waited on, and provider
-side index builds take tens of seconds each. The server logs nothing and does
not bind until they are queryable. That is the assertion working, not a hang.

### Against a managed deployment

`docker-compose.atlas.yml` points the server at an external MongoDB and detaches
it from the bundled pair:

```sh
TINYSWEEPER_MONGODB_URI='mongodb+srv://…' \
    docker compose -f docker-compose.yml -f docker-compose.atlas.yml up -d tinysweeper
```

The overlay leaves the `mongod` and `mongot` service definitions in place, so
the live tests below still work unchanged.

Check the deployment, do not assume it. Running `$vectorSearch` against a
collection that does not exist returns an empty result rather than an error, so
a probe written that way reports success on a cluster that supports none of the
three stages. The honest check is a round trip: insert, create the index, wait
for `READY`, query, and read the scores back. `MongoIndex::prepare` does exactly
that, which is why pointing the server at a deployment is itself the test.

## Running the live tests

`cargo test` stays offline: the MongoDB tests are `#[ignore]`d and skip without
`TINYSWEEPER_TEST_MONGODB_URI`.

```sh
export MONGO_ROOT_PASSWORD=devpass TINYSWEEPER_WEBHOOK_SECRET=dev
docker compose up -d mongod mongot
TINYSWEEPER_TEST_MONGODB_URI='mongodb://tinysweeper:devpass@localhost:27017/?authSource=admin&directConnection=true' \
    cargo test --locked --features serve --lib -- --ignored index::mongo --test-threads=1
```

Each live test uses its own repository ids: they share one process and therefore
one database.

## Embedding providers

Two implementations of the `Embedder` port, chosen by `embeddings.provider`
through `index::embedder_from_config`.

`openrouter` (the default) is a direct HTTP client in `index/openrouter.rs`.
Everything else goes through `index/provider.rs`, which wraps tinyagents'
`EmbeddingModel` and its Voyage / OpenAI / Cohere / Ollama adapters.

The split exists for one reason: tinyagents' `EmbeddingModel::embed` returns a
bare `Vec<Vec<f32>>`, and every adapter behind it decodes the response and
discards the `usage` object. OpenRouter sends one carrying both `prompt_tokens`
and the `cost` it charged, and indexing a repository is the largest token count
this program produces — the line of the bill least well served by an estimate.
Routing it through that trait would throw the number away and fall back to
`estimate_tokens`, which is four bytes to a token.

So the accounting has three tiers, best first:

| Constructor | Tokens | Cost |
|---|---|---|
| `Embedded::charged` | provider | provider |
| `Embedded::metered` | provider | local price table |
| `Embedded::billed` | estimated | local price table |

The gateway's own cost is preferred over `harness::pricing` wherever it is
present. That table is hand-maintained and goes stale silently; a gateway
quoting what it actually billed cannot disagree with the invoice, and it
already includes any routing markup a per-model table would miss. The table
remains the fallback for a response that omits `cost`.

### Dimensions

`embeddings.dimensions` is pinned in configuration and is **not** discoverable
from the API — OpenRouter's model listing does not report it. It is both the
index partition key and the width the search index is created with, so a wrong
value is not an error but confidently wrong retrieval against vectors from
another space. `OpenRouterEmbedder` refuses any response whose vector width
disagrees with the configured signature.

Measured against the live API:

| Model | dims | $/M | context |
|---|---|---|---|
| `openai/text-embedding-3-small` (default) | 1536 | 0.02 | 8k |
| `voyageai/voyage-4` | 1024 | 0.06 | 32k |
| `mistralai/codestral-embed-2505` | 1536 | 0.15 | 8k |
| `qwen/qwen3-embedding-8b` | 4096 | 0.01 | 32k |
| `baai/bge-m3` | 1024 | 0.01 | 8k |
