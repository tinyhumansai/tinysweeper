# Review queue status and capacity

The server keeps review and issue-triage capacity in separate operator-owned
pools. `[server].review_concurrency` defaults to four and
`[server].triage_concurrency` defaults to two. Startup validates both values
against Tokio's semaphore limit. Repository configuration overlays cannot
change either pool; changing capacity requires editing trusted server
configuration and restarting the process.

Each accepted review has a durable status identity for its queued check. The
check publishes the accepted head and its position among eligible queued work,
then the same check ID moves through in-progress and a terminal conclusion.
Queue rank follows accepted order and excludes work that is not currently
eligible. Check head and generation fields are UI status evidence; they do not
authorize paid work. A review owner must still confirm the live head and paid
budget through the scheduler lease before context, indexing, or model work.

## Status-only admission work

After signature, authorization, and trust checks, the server durably admits
work before resolving minimal live pull-request metadata. It publishes a
deterministic queued status before acquiring a review permit only when that
metadata says the pull request is open and not a draft; closed and draft work
is declined without a queued check. This is the narrow status-only exception
to the usual review-start credential ordering: webhook acceptance must remain
visible while all review permits are occupied, and GitHub does not
automatically redeliver a failed webhook request. This path does not fetch
diffs, trees, review context, or index data, and it never invokes a model. The
write handle is used only by the deterministic status plan in
`src/app/apply.rs`; it is never passed to model code.

Admission is persisted before this network work. A bounded background status
worker retries metadata resolution and queued-check publication independently
of review permits, including after restart. Headless accepted events reserve
capacity for both metadata resolution and the eventual check identity. A stale
metadata result cannot overwrite a newer accepted head. Closed and draft pull
requests receive no queued check or model work.

Status claims are generation-fenced and acquired immediately before each item.
Each external operation has a four-minute deadline inside a five-minute durable
claim. A drain processes at most sixteen metadata intents and sixteen check
intents, so later work receives a fresh claim instead of aging while an earlier
request is in flight.

## Recovery and limits

The scheduler persists each check's external ID, head, and write generation.
After a restart or an ambiguous response, the worker searches for that
external ID on the same head and under tinysweeper's check name and App before
reusing the returned check ID. A retired status whose remote ID is not
recoverable can be concluded directly as neutral without first creating a
queued check. Detached status work shares bounded scheduler capacity and remains until GitHub
acknowledges the terminal write.

The external ID is a recovery locator, not a remote uniqueness constraint.
GitHub can accept a POST whose response is lost, and a later lookup may not
immediately reveal it. The durable intent is retained and retried; the server
does not claim exactly-once remote writes. If a status update fails after paid
work has started, a later cleanup status says only that completion could not be
confirmed and makes no judgment about the code. A confirmed published review
retains its actual result. If the final status update for that published review
fails, the scheduler does not rewrite it as unreviewed; retrying that exact
terminal update is a remaining limitation.

The metadata head and queue check never replace the lease-confirmed paid head.
When an authoritative read finds a different head, the scheduler atomically
reserves that head's status identity and retires the superseded status. If
status capacity is full, the review defers before paid work and retries after
the bounded status worker drains detached records.
