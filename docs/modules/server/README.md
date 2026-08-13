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

### A failed review is never silent

`server::failure` exists because the paragraph above was, for a while, the
whole story: a review that could not run logged an error and stopped. Nothing
reached the pull request, so a total model outage and a clean bill of health
rendered identically on GitHub — and the auto-merge gate read the absence of a
review as nothing to object to.

Two things now happen instead.

A **transient** failure is retried up to `failure::MAX_ATTEMPTS` times with an
exponential backoff capped at eight seconds. Only `Error::Model` and
`Error::Forge` qualify; everything else is deterministic, and `Error::Budget`
in particular must never be retried, because the run has already spent its
ceiling. The retry is safe against the lease because `review_inner` releases it
on every exit path, so the next attempt re-claims it rather than colliding with
itself and returning a silent `Ok`.

A review that still cannot run publishes a **`tinysweeper/review`** check with
conclusion `ActionRequired`. That conclusion is the load-bearing choice:
`CheckConclusion::blocks` is true for it, and `automerge::policy::check_refusal`
refuses on the first failing check it sees regardless of `require_checks`, so
an unreviewed pull request now stops the gate without any repository having to
opt in. `Skipped` would read better in English and would have preserved the
exact bug, since it does not block; `Failure` would claim tinysweeper found
something disqualifying in the code, when it never got to look.

The summary says, in its first line, that no code was reviewed and that the
check is not a finding about the change — a red check reads as an accusation
otherwise. Error text is scrubbed before it is published: the gateway's quota
error embeds a key-management URL ending in the key's id, and per the security
boundary that must not reach a check-run summary.

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

### Manual full review

| Method | Path | Answers |
| --- | --- | --- |
| `POST` | `/admin/reviews/{owner}/{name}` | `202` with the pull request numbers queued |

The escape hatch for when a review has to be redone wholesale. The body is
`{"number": 12}` for one pull request, or `{}` for every open one — capped at 20
per request, because each queued review is model spend. Reviews run off the
request path; the response says which were queued, not what they found.

**What "full" means.** The run sets `review.incremental = false` for itself and
changes nothing else. That single flag gates all three halves of the incremental
path in `src/app/review.rs`: the prior findings read back off the pull request,
the remembered evidence and fingerprints in the review-state store, and the
write-back at the end. So a full review dedupes against nothing and will repeat
comments that are already there — that is what was asked for — and, because the
write-back is off too, it does **not** overwrite what the webhook path
remembers. Nothing is deleted: destroying the stored state would make the next
ordinary review duplicate its comments as well, which is a side effect an
operator did not ask for.

The manual run takes its own lease (`…@{sha}!full`) rather than the webhook
path's, since the ordinary review of that head has usually already happened and
sharing the key would make the button a silent no-op.

**Installation.** A webhook delivery names the installation; an operator does
not. The route asks GitHub which installation covers the repository, using the
app JWT — the only credential that can answer before an installation token
exists. A repository the app is not installed on fails with a message saying so.

**Organisation.** The owner must be the one organisation this deployment
allows (`TINYSWEEPER_ALLOWED_ORG`, defaulting to `tinyhumansai`); anything else
is `403`. The check lives in the route because the caller's own check protects
nobody — see `.github/workflows/manual-review.yml`, which is a button that POSTs
here and is the one deliberate exception to tinysweeper shipping no Actions
path.

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

### A delivery is acknowledged before the database is touched

The module doc says a delivery is "acknowledged and queued, never handled
inline", because GitHub allows ten seconds. That was true of the *work* but not
of the acknowledgement: `claim_delivery` — a MongoDB round trip — ran before
the response, so a slow database made the fast ack slow.

On 2026-08-13 that cost eight deliveries in ninety seconds: four timed out at
exactly 10.0s, four returned `503`. One of them was a `pull_request opened`, so
that pull request was never reviewed and nothing on our side recorded a
failure — the review had not started, so there was no failed review to report.

The 503 was deliberate, and the reasoning was wrong. It existed so GitHub would
retry rather than have a delivery silently dropped. GitHub does not retry: the
delivery log says `giving up after 1 attempt(s)`. Failing the request did not
buy a second attempt, it only turned a slow database into permanent loss.

So routing — which is pure — now happens first, and the two outcomes that do no
work (`Ignore`, `TrackDraft`) are answered without touching Mongo at all. That
is most deliveries. Anything that *is* work is spawned, and the delivery claim
happens there, on the other side of the response.

Dedupe is not weakened. The claim still runs before any work, and it was never
the only guard: `review_inner` takes a lease keyed on `repo#number@sha`, so a
claim that fails outright still cannot produce two reviews of one commit. When
the claim errors the worker now proceeds rather than returning — the delivery is
already acknowledged, so dropping the work would be silent, and doing it twice
is the better failure.
