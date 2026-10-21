# `src/server` — the GitHub App

Feature-gated behind `serve`. This is the only way tinysweeper runs in
production: the GitHub Actions distribution path was removed, and there is no
composite action, reusable workflow or release binary to install.

| File | Responsibility |
| --- | --- |
| `routes.rs` | The axum app: `/healthz`, `/webhook`, the review worker |
| `webhook.rs` | HMAC verification and event parsing |
| `model.rs` | Resolve reusable text and optional vision models through the `ModelFactory` port |
| `auth.rs` | App JWT and cached installation tokens |
| `store.rs` | MongoDB: contributors, trust, delivery and lease claims |
| `indexing.rs` | `IndexBackend`: the embedder and the retrieval stores, and the background index job |
| `admin.rs` | The authenticated `/admin` API |

## Model lifetime and construction

`serve` uses the default gateway-backed `ModelFactory`; hosts can inject another
factory through `serve_with_model_factory`. Model construction is async and
receives only the operator's `[models]` configuration. The always-compiled
`StaticModelFactory` provides offline text and optional vision handles.

The server resolves one text adapter and, when `model_for_vision` returns an
explicit nonempty ID, one separate vision adapter before opening its listener.
A construction failure refuses startup rather than surfacing on the first
contributor's request. `AppState` and its worker clones share these handles for
review lanes, issue triage, preview planning/steps, and captions. Workload model
IDs and token limits remain request-specific. Repository overlays cannot change
model credentials or routes, so they do not rebuild the adapters.

Vision still uses its own unpinned gateway with no text fallbacks or per-model
text routes. Without a vision ID, captions use the same text adapter and their
preview workload ID, with images omitted as before. The model factory never
receives GitHub credentials; write-token ordering remains independent of model
construction. Credentials and provider policy are fixed for the server's
lifetime; restart to pick up a rotated model key.

## Review timing logs

The server emits a structured `review timing` info event when a worker exits.
Its `repo`, `pr`, `head_sha`, and `mode` identify the run; `outcome` is
`success`, `failure`, `deadline`, `declined`, `superseded`, or `cancelled`.
`total_ms` is worker wall time and `queue_wait_ms` is time waiting for a
review permit. `phases_ms` and `lanes_ms` are JSON maps, while call counts,
token totals, and answered/unanswered file counts are individual fields. Filter for the event in
the container logs, for example `docker compose logs tinysweeper | grep 'review timing'`.
The `review_inner` tracing span also carries the repository, PR number, head
SHA, and mode, so phase spans can be correlated to a specific push.

Model-call events include `latency_ms` alongside token usage and model ID.

## Bounded review scheduling

Separate review and triage pools, queued check identity and status recovery,
including the status-only pre-permit ordering, are documented in
[Review queue status and capacity](review-queue-status.md).

Webhook and manual reviews are atomically admitted to one bounded Mongo scheduler
document before the server acknowledges them. Up to 260 unfinished
records, queued or running, are durable; the in-memory channel carries wakeup hints only,
and a polling dispatcher recovers work when a hint is lost or a process
restarts. Admission also reserves space for seven days of dedupe tombstones
under an 8 MiB BSON limit. If either bound is full, the webhook returns `503`
without acknowledging the delivery; GitHub does not retry automatically, so an
operator must redeliver that overflowed event. Manual batches are admitted
atomically, or refused as a whole. Other webhook actions keep their existing
dispatch path; corpus recall recording and post-rollout observation are covered by [the evaluation runbook](../eval/recording-and-48h-runbook.md).

The manual endpoint checks durable capacity before resolving installations or
pull requests. Its final whole-batch admission is still authoritative, so a
concurrent admission can fill the scheduler after the preflight and cause the
manual batch to be refused without partial jobs.

`[server].review_concurrency` bounds the jobs holding a durable 45-second owner
lease across the server fleet (four by default). A heartbeat extends the lease
every 15 seconds. Generation checks fence claims, renewals, and completion;
local expiry checks stop a paused process
before it resumes model work, and model dispatches also verify ownership. A
shutdown stops new claims and leaves unfinished jobs in Mongo for the next
process. If shutdown lands while the initial status check is being opened, that
accepted job is deferred and remains available to the next owner.

Incremental jobs coalesce by repository and pull request. A delivery with a
different SHA hint adds its ID to the existing reservation and asks an active
owner to yield; the hint is advisory and does not choose the review target.
Queued hints collapse to one pending job. The running lease and global slot
remain occupied until that owner acknowledges the yield or its lease expires,
so a hint cannot free capacity while the old model call is still running. The
replacement owner reads the authoritative live Forge head before paid work;
an older delayed event cannot make its SHA the review target. Each confirmed
head has five durable paid-work starts across retries, owners, and restarts. A
same-head deadline or other terminal result remains recorded until a live read
confirms a different head; another delivery ID or webhook SHA hint cannot
refill the budget. Delivery IDs and terminal tombstones are deduped for seven
days, while the current head's terminal marker remains until authoritative
head change. After the fifth start, recovery opens or resumes the status check,
then concludes it as **Not reviewed** before recording the terminal outcome.
No further index or model work runs for that exhausted head. Manual full
reviews use a separate claim key and are not superseded by incremental
deliveries.

All coalesced delivery IDs and per-head history consume reserved bounded-state
capacity. An admission that would exceed those reservation limits is refused
as a whole, like an oversized manual batch. GitHub publication still uses the
app's live-head and idempotency checks: a process can crash between a GitHub
write and recording its outcome, so the scheduler cannot promise exactly-once
remote writes. A confirmed successful publication is retained when an owner
yields, preventing a known success from being rerun for the same head.

When an event includes a head SHA, the worker claims that exact SHA before
minting a read token or fetching live PR state. Without a SHA, it first claims
a provisional per-PR key, then reads live Forge state and binds the lease to
the authoritative head. Both paths confirm the live head before indexing or
model work. Manual reviews resolve pull request numbers and SHAs before
enqueueing, so the response can list the queued reviews. The app checks the
leased SHA again after loading review context and skips before any model call
if the context moved.

## The write-token boundary

`AGENTS.md` says the model never holds a write token. The two-job Actions split
used to be one way of enforcing that. Removing Actions did not weaken it,
because the enforcement was never really in the workflow:

- **The type system carries it.** `ForgeRead` and `ForgeWrite` are separate
  traits in `src/ports/forge.rs`. Lanes are handed a `ForgeRead`; only
  `src/apply` takes a `ForgeWrite`. A lane cannot mutate a pull request even by
  mistake, because it never holds a handle that could.
- **The ordering carries the rest.** In `routes.rs::review_inner`, the
  read-only handle is built from an installation token minted before the lanes
  run, `run_lanes` runs `crate::app::review(...)` against it under the
  deadline, and only after that has returned is a second token minted and
  wrapped in a `GitHubWrite` for `crate::app::apply(...)`. The write handle
  does not exist while a model call is in flight.

## Indexing does not block the review

A cold full index of a large repository is thousands of embedding calls and
minutes of wall clock; a review is expected in seconds. So `handle_review`
spawns the index job and does not await it, and the review runs against
whatever the index holds right now. On a repository's first push that is
nothing, and `src/retrieve` degrades through it honestly — the check-run summary
says the index was cold rather than quietly reviewing on less context than the
operator believes. Blocking the first review on the first index would trade a
thin review for a late one, and a late review is the one nobody reads.

Both paths use the server-owned repository cache: one private bare mirror per
canonical repository, with separate exact-head disposable worktrees. Git
operations for one repository are serialized, and review worktrees are removed
when the run ends. The cache is disposable state. To reclaim it, stop the
service first, then remove `TINYSWEEPER_REPOSITORY_CACHE_DIR` or the default
private cache directory; do not clean it while workers may hold checkouts.
Reviews fetch allowlisted gitlinks changed between base and head; unchanged or
unavailable submodule content comes through the forge API. Indexing recursively
fetches allowed gitlinks under the configured host and allowlist policy.

### Closed pull requests stop before review work

Webhook routing ignores a pull request event when GitHub supplies a non-open
state or `merged: true`. Comment deliveries use the issue-shaped PR reference;
when it includes a closed state, a mention is ignored before it is queued.
Some trigger payloads have no state, so the worker claims its supplied revision
before reading the live pull request, then applies this gate before indexing,
opening a check, or starting the lanes. This also protects `/admin/reviews`: its queued head can close while
waiting for a worker, and a manual request follows the same live-state gate.
After the lanes return, `app::apply` reads live state again before its first
write, so a pull request merged while a model call was running receives no
review or check publication from that run.

A refused claim is requeued, never waited on. `IndexManifest::claim` answers
`Busy` when another worker holds the repository; the job sleeps, retries a
bounded number of times, and then leaves it to the holder, who is doing the same
work anyway. A worker blocked on that claim would be a worker not indexing
anything else, and with a bounded pool a few of those are the whole pool.

The default `MAX_CONCURRENT_INDEXES` limit of two is lower than the default
review limit of four, and for a different reason: a review is mostly waiting
on one model, while a full index is a fetch, a tree in memory and thousands of
embedding calls. The index limit is independent of the operator-configured
review pool.

## Concurrency and failure

`[server].review_concurrency` (default four) bounds how many reviews run at
once. The limit is about
spend and rate limits rather than CPU: a force-push across a repository delivers
a burst, and an unbounded worker pool turns that into an unbounded bill.

A review is wrapped in `catch_unwind`, so a panic inside a lane still reaches
the lease release below it. Leases also carry a TTL in Mongo, which is the only
thing that covers a killed process — a stranded lease would otherwise mean that
pull request can never be reviewed again.

`REVIEW_DEADLINE` is 900 seconds from the worker's permit acquisition, including
metadata, checkout and retries. The default soft cutoff at 720 seconds stops new
work; admitted groups drain for at most 30 seconds, ending at least five seconds
before the hard deadline. A published partial review is terminal `Published`
without requeueing or resetting paid attempts. A genuine hard timeout remains
`ActionRequired` with "ran out of time" and is not retried. See
[bounded review planning](../lanes/large-pr.md) for selection, coverage and
Normal custom-model limitations.

How it is enforced differs by phase, because cancellation is only safe where
nothing non-idempotent is in flight:

- **The lanes and the lookup checkout** are *cancelled* at the deadline, in
  `run_lanes`. This is what drops the model calls.
- **The metadata phase** between the lease claim and the lanes — the umbrella
  check's POST, the config overlay, the default-branch read — is not
  cancelled: a check-run POST cut off after GitHub accepted it is an orphaned
  check. Each call is bounded by `forge::github`'s request timeout instead,
  and `Run::check` re-reads the deadline at the boundary before the lease is
  claimed and again before the lanes start, so a spent budget stops the run
  before it holds anything.
- **The publish** has a budget of its own, `PUBLISH_DEADLINE`, rather than
  whatever the lanes left over. `apply` is a handful of sequential,
  non-idempotent writes, so its bound is deliberately generous, sized to fire
  only on a publish that is stuck.

The three add up, with `METADATA_ALLOWANCE` standing in for the metadata
phase's per-call bounds, and the sum is asserted at compile time to be
shorter than `LEASE_TTL`, because a review still running when its lease
lapses is exactly the duplicate the lease prevents.

### A running review is never silent either

`server::status` owns **`tinysweeper/review`**, one check with a lifecycle
rather than a single verdict. It is created *in progress* as soon as a run has
claimed the lease and knows the head SHA — after two metadata reads, before any
model call — and the same run is concluded afterwards by id.

| State | Conclusion | Meaning |
|---|---|---|
| in progress | `None` | accepted, lanes running |
| concluded | `Success` | the lanes ran and the review did not request changes |
| concluded | `Neutral` | the lanes ran and the review requested changes |
| concluded | `Neutral` | opened, then declined — draft, blocked author, taken |
| concluded | `ActionRequired` | the review could not run (see below) |

A completed review concludes from `app::apply::requests_changes`, the same
predicate the submitted review reads, so the check never shows a pass beside a
Changes Requested review. `Neutral` rather than `Failure`: the review already
blocks, and liveness must not become a second merge gate.

It is opened *after* the draft check and the lease claim, not on the delivery
path, so a draft, a blocked contributor or a duplicate delivery never announces
a review that is not going to happen. It is still early enough that a
contributor sees it seconds after pushing rather than minutes.

`Success` here reports that the review **completed**, not that the code passed,
and the summary says so. Conflating the two would make a pull request with
findings unmergeable on a check that only ever measured liveness.

Two consequences are load-bearing:

- **A pending check refuses auto-merge.** `automerge::policy::check_refusal`
  refuses on *any* pending check, not only a required one. That is what we
  want — nothing should merge underneath a review still in flight — but it
  means an in-progress check that never concludes stalls the gate on that
  commit until somebody pushes again. Every exit from `handle_review`
  therefore concludes it, which is what `routes::StatusSlot` is for: the
  obligation is discharged in one place instead of at each `return`.
- **The check is updated, never re-posted.** A second POST of the same name
  creates a second run and leaves the first pending forever, so `publish_check`
  returns the id and `update_check` takes it.
- **A redeploy concludes what it interrupts.** `docker compose up` sends
  `SIGTERM` and the reviews in flight will not finish inside its grace
  period. `AppState::in_flight` lists every open slot, and the shutdown path
  concludes each as `ActionRequired` ("restarted while this review was
  running") *before* axum starts draining connections — after it, there may
  be no time left. Taking the status out of the slot is what keeps this safe
  against a lane that finishes in the same second: its own `close_status`
  finds the slot empty and does nothing. `AppState::in_flight` also stops
  accepting new registrations in the same locked step as the snapshot, so a
  webhook accepted while the snapshot's network calls are still in flight —
  axum has not started draining yet, only `shutdown` returning triggers that —
  cannot land a slot after the snapshot and run unwatched by any shutdown
  pass; `handle_review` declines that review outright, before it opens a
  check, and leaves it for the next push or redelivery.

  What this does not close, because no amount of in-process bookkeeping can:
  Compose's ten-second grace period is shorter than a single GitHub write can
  legitimately take (`forge::github::REQUEST_TIMEOUT` is 60s), so a task that
  is genuinely mid-write — `open_status` posting the initial check, a
  `dispatch` task still awaiting `claim_delivery`, or the review's own
  `close_status` mid-`update_check` on the ordinary completion path, not the
  shutdown one — when `SIGKILL` lands can still be cut off with GitHub having
  already accepted a request this process never learns the outcome of. Every write this module makes is built to
  degrade the same way when that happens: a delivery claim self-heals through
  its own 7-day TTL exactly as a review lease self-heals through `LEASE_TTL`
  (`server::store`'s own words for it — "the backstop for the cases this
  cannot cover, a kill, or a lost machine"), and a check left "in progress"
  is a pending check, which blocks auto-merge rather than mismerging anything.
  Closing this fully would mean either the grace period growing past the
  slowest write this process makes, which is a Compose/`docker-compose.yml`
  change on the deployment, not code here, or tracking and joining every
  spawned worker before the process is allowed to exit, which would routinely
  spend the *entire* grace period waiting on one write and starve the group
  conclude above of the time it needs for the reviews it can actually see.

On the write token: opening this check mints an installation token before the
lanes run, which the security boundary otherwise reserves for after every model
call. The property that rule protects — *the model never holds a write handle*
— is preserved exactly. The token is minted in `open_status`, used for one
request, and dropped before the function returns; it never enters `AppState`,
never reaches `run_lanes`, and no lane or model can reach it.
`report_failure` has always minted one on the same terms.

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

A review that still cannot run concludes the **`tinysweeper/review`** check as
`ActionRequired` — updating the in-progress run when there is one, and creating
its own when the review died before it had a SHA to pin one to. That conclusion
is the load-bearing choice:
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
request path; the response says which were queued, not what they found. If all
review permits are occupied, the endpoint returns `503` before minting an
installation token or reading pull request state. Queue slots for an expanded
request are reserved together, so an overflow cannot partially enqueue it.

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
sharing the key would make the button a silent no-op. Full retains the same
Large selection, shared call limit and original-clock deadline policy.

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
