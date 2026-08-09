# What wakes tinysweeper up

tinysweeper runs as a GitHub App. A single server receives webhook deliveries
for every installed repository; there is no workflow file anywhere and no
Actions path at all. This document is what the App subscribes to, what it can
never be told about, and how it handles the difference.

## What fires reliably

| Something happens | Event | Fires? |
| --- | --- | --- |
| A commit is pushed to the pull request | `pull_request: synchronize` | Yes, every push after it is ready for review; drafts are tracked only |
| The pull request is opened or reopened | `pull_request: opened`, `reopened` | Yes when it is not a draft; drafts are tracked only |
| A draft is marked ready | `pull_request: ready_for_review` | Yes, this starts its first review workflow |
| The title or body is edited | `pull_request: edited` | Yes |
| A label is added or removed | `pull_request: labeled`, `unlabeled` | Yes |
| Someone comments on the pull request | `issue_comment: created` | Yes |
| Someone comments on a line of the diff | `pull_request_review_comment: created` | Yes |
| A review is submitted | `pull_request_review: submitted` | Yes |
| A check run finishes | `check_suite: completed` | Yes |
| The App is installed or repositories are added | `installation`, `installation_repositories` | Yes |

`issue_comment` fires for issues *and* pull requests; the payload distinguishes
them only by `issue.pull_request` being present. `webhook::route` filters on
exactly that.

A comment event carries no head SHA, so the server resolves it from the API
rather than assuming `pull_request.head.sha` exists.

Deliveries are acknowledged and queued, never handled inline: GitHub allows ten
seconds and a review takes minutes, so handling one in the request would
guarantee a timeout — and a timeout means a redelivery, which would mean a
second review of the same event. The delivery id is claimed in the store, so a
redelivery is a no-op rather than duplicate spend.

## What never fires — for anyone

These have **no webhook event at all**. Being a hosted App does not help; the
event does not exist:

- **Resolving a review thread.** There is no event. Thread resolution state is
  only readable by querying `isResolved` on review threads through the GraphQL
  API.
- **Adding a reaction** (👍 / 👎) to a comment. No event.
- **Editing a review comment's body.** No event for the edit.

This matters because two designed behaviours depend on that state: suppressing a
finding the author resolved, and learning from a 👎. Both are therefore
**pull-based** — each review run reads the current resolution and reaction state
at the start and folds it into the suppression set. That is the only design that
works on any architecture, which is why this was never an argument for or
against a server.

The practical consequence: resolving a thread does not immediately re-run
anything. The suppression takes effect on the next run, which the next push or
comment triggers anyway.

## Fork pull requests

Under Actions this was the sharp edge: a `pull_request` run from a fork got a
read-only token and *no secrets at all*, so nothing could be published, and the
workaround — `pull_request_target` — put contributor code inside a privileged
context.

As an App, none of that applies. Credentials belong to the installation rather
than to the run, so a fork pull request is delivered and reviewed exactly like
any other, and the check runs publish normally.

The invariant that made `pull_request_target` survivable is still in force, and
it is a constraint rather than a mitigation: **tinysweeper never executes
anything from the tree it reviews.** It reads the diff and reads files. It does
not build, install dependencies, or run the repository's scripts. Nothing in the
server relaxes that — see the security boundary in `AGENTS.md`.

The one fork-specific behaviour that remains is a policy choice, not a platform
one: an unknown contributor is `Trust::Unknown`, and a blocked one is not
reviewed at all. Trust is set through the admin API — see
[modules/server/README.md](modules/server/README.md).

## Scheduled work

The auto-merge sweep, stale handling and Sentry promotion are timers inside the
server rather than `schedule` workflows. That removes three caveats that used to
apply — cron being minutes late, being disabled after 60 days of repository
inactivity, and running the default branch's copy of the workflow.

The sweep is still written to be idempotent and to reconcile from live state
rather than assuming it ran last time. A process restart is now the thing that
can drop a tick, and reconciling from live state covers that just as well.

## External systems

Anything that can make an HTTP request can reach the server directly, so
`repository_dispatch` is no longer the door in. Sentry could never use that door
anyway — its webhook integration cannot set the `Authorization` header GitHub
requires — so Sentry promotion **polls** the Sentry API. Slower, and entirely
adequate for triage.
