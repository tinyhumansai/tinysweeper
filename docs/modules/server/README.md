# `src/server` — the GitHub App

Feature-gated behind `serve`. This is the only way tinysweeper runs in
production: the GitHub Actions distribution path was removed, and there is no
composite action, reusable workflow or release binary to install.

| File | Responsibility |
| --- | --- |
| `routes.rs` | The axum app: `/healthz`, `/webhook`, the review worker |
| `webhook.rs` | HMAC verification and event parsing |
| `auth.rs` | App JWT and cached installation tokens |
| `store.rs` | MongoDB: contributors, trust, delivery and lease claims |
| `indexing.rs` | `IndexBackend`: the embedder and the retrieval stores, and the background index job |
| `admin.rs` | The authenticated `/admin` API |

## The write-token boundary

`AGENTS.md` says the model never holds a write token. The two-job Actions split
used to be one way of enforcing that. Removing Actions did not weaken it,
because the enforcement was never really in the workflow:

- **The type system carries it.** `ForgeRead` and `ForgeWrite` are separate
  traits in `src/ports/forge.rs`. Lanes are handed a `ForgeRead`; only
  `src/apply` takes a `ForgeWrite`. A lane cannot mutate a pull request even by
  mistake, because it never holds a handle that could.
- **The ordering carries the rest.** In `routes.rs::run_and_publish`, the
  read-only handle is built from an installation token minted before the lanes
  run, `crate::app::review(...)` runs against it, and only after that call has
  returned is a second token minted and wrapped in a `GitHubWrite` for
  `crate::app::apply(...)`. The write handle does not exist while a model call
  is in flight.

## Indexing does not block the review

A cold full index of a large repository is thousands of embedding calls and
minutes of wall clock; a review is expected in seconds. So `handle_review`
spawns the index job and does not await it, and the review runs against
whatever the index holds right now. On a repository's first push that is
nothing, and `src/retrieve` degrades through it honestly — the check-run summary
says the index was cold rather than quietly reviewing on less context than the
operator believes. Blocking the first review on the first index would trade a
thin review for a late one, and a late review is the one nobody reads.

A refused claim is requeued, never waited on. `IndexManifest::claim` answers
`Busy` when another worker holds the repository; the job sleeps, retries a
bounded number of times, and then leaves it to the holder, who is doing the same
work anyway. A worker blocked on that claim would be a worker not indexing
anything else, and with a bounded pool a few of those are the whole pool.

`MAX_CONCURRENT_INDEXES` is lower than the review cap and for a different
reason: a review is mostly waiting on one model, while a full index is a fetch,
a tree in memory and thousands of embedding calls.

## Concurrency and failure

`MAX_CONCURRENT_REVIEWS` bounds how many reviews run at once. The limit is about
spend and rate limits rather than CPU: a force-push across a repository delivers
a burst, and an unbounded worker pool turns that into an unbounded bill.

A review is wrapped in `catch_unwind`, so a panic inside a lane still reaches
the lease release below it. Leases also carry a TTL in Mongo, which is the only
thing that covers a killed process — a stranded lease would otherwise mean that
pull request can never be reviewed again.

## The admin API

Everything under `/admin` requires `Authorization: Bearer <token>` matching
`TINYSWEEPER_ADMIN_TOKEN`. The token is compared as a SHA-256 digest in constant
time, in a `route_layer` that runs before any handler extractor — so an
unauthenticated request never has its body parsed, the same ordering the webhook
uses for its HMAC.

Two decisions worth knowing:

- **No token, no router.** When the variable is unset the admin routes are never
  mounted and every `/admin` path 404s. The alternative to failing closed here
  is shipping an unauthenticated write endpoint.
- **A short token is refused at startup.** Under 32 characters and the process
  will not start, because a weak token is brute-forceable over the same public
  endpoint it protects.

### Contributor trust

`Trust::Blocked` is checked before every review. These are how it gets set.

| Method | Path | Status |
| --- | --- | --- |
| `GET` | `/admin/contributors/{login}` | Live |
| `PUT` | `/admin/contributors/{login}/trust` | Live |

`PUT` takes `{"trust": "unknown" \| "allowed" \| "blocked", "note": "why"}` and
returns the stored contributor. An unknown trust value is rejected rather than
silently read as `unknown`. A login that is not plausibly a GitHub login is
rejected before it reaches the database, because the login is a Mongo `_id`.

A contributor nobody has seen returns `Unknown` rather than `404`: "never seen
this person" is an answer, and it lets trust be set ahead of a first pull
request.

### Index

| Method | Path | Answers |
| --- | --- | --- |
| `GET` | `/admin/index/{owner}/{name}` | State, revision, chunk count and cumulative spend |
| `POST` | `/admin/index/{owner}/{name}/reindex` | Discards the index; the next delivery rebuilds it |

Both report the embedding signature alongside, because it is the partition key:
"absent" on a deployment that has just changed embedding model means something
quite different from "absent" on a repository nobody has pushed to, and the two
are indistinguishable without it. `503` when no embedding provider is
configured — there is genuinely no index, which is a different answer from an
empty one.

`reindex` does not index inline, and that is a decision rather than a shortcut.
Indexing needs a checkout, a checkout needs an installation token, and the admin
credential authenticates a human rather than an app installation. Rather than
invent a second credential path into GitHub for an operator convenience, the
route resets the freshness record and lets the ordinary push path do the work
with the token it already holds. It takes the same claim a worker does, so it
cannot reset the record underneath a run in progress, and answers `409` rather
than waiting when one holds it.

The reset deletes the chunks as well as the record. Forgetting the record alone
would leave every existing chunk with nothing pointing at it — the next run
would neither reuse nor delete them, and stale code would sit in retrieval
permanently.

### Knowledge documents

The write side of the knowledge centre — see `docs/modules/knowledge/README.md`.
The scope comes from the path, never the body, so a request cannot claim one
scope in its URL and write another into the database. A repository listing
includes its organisation's documents, because that is what a review at that
scope sees. `503` when no retrieval database is reachable, rather than a
pretended success.

| Method | Path |
| --- | --- |
| `GET` | `/admin/knowledge/org/{owner}` |
| `PUT` / `DELETE` | `/admin/knowledge/org/{owner}/{slug}` |
| `GET` | `/admin/knowledge/repo/{owner}/{name}` |
| `PUT` / `DELETE` | `/admin/knowledge/repo/{owner}/{name}/{slug}` |

## What the store is for

Deliberately narrow: identity, trust, and delivery bookkeeping. Review state
stays on GitHub in the durable comment's markers, so losing the database costs
the trust decisions and nothing else. That is a property worth keeping — the
database is never the thing standing between a pull request and its review.
