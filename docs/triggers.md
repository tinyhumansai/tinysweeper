# What wakes tinysweeper up

tinysweeper runs entirely in GitHub Actions. There is no webhook server, and
this document is the reasoning behind that — chiefly, that the two things a
server is usually bought for turn out not to be things a server can do.

## What fires reliably

| Something happens | Event | Fires? |
| --- | --- | --- |
| A commit is pushed to the pull request | `pull_request: synchronize` | Yes, every push |
| The pull request is opened or reopened | `pull_request: opened`, `reopened` | Yes |
| A draft is marked ready | `pull_request: ready_for_review` | Yes |
| The title or body is edited | `pull_request: edited` | Yes — must be listed explicitly |
| A label is added or removed | `pull_request: labeled`, `unlabeled` | Yes — must be listed explicitly |
| Someone comments on the pull request | `issue_comment: created` | Yes |
| Someone comments on a line of the diff | `pull_request_review_comment: created` | Yes |
| A review is submitted | `pull_request_review: submitted` | Yes |
| A check run finishes | `check_suite: completed` | Yes |
| Time passes | `schedule` | Best-effort — see below |

`issue_comment` fires for issues *and* pull requests; the payload distinguishes
them only by `github.event.issue.pull_request` being present. The reusable
workflow filters on exactly that.

A comment event carries no head SHA, so the workflow resolves it from the API
rather than assuming `github.event.pull_request.head.sha` exists.

## What never fires — for anyone

These have **no webhook event at all**. A hosted GitHub App would not receive
them either, so they are not an argument for running a server:

- **Resolving a review thread.** There is no event. Thread resolution state is
  only readable by querying `isResolved` on review threads through the GraphQL
  API.
- **Adding a reaction** (👍 / 👎) to a comment. No event.
- **Editing a review comment's body.** No event for the edit.

This matters because two designed behaviours depend on that state: suppressing a
finding the author resolved, and learning from a 👎. Both are therefore
**pull-based** — each review run reads the current resolution and reaction state
at the start and folds it into the suppression set. That is the only design that
works, on any architecture, so the no-server decision costs nothing here.

The practical consequence: resolving a thread does not immediately re-run
anything. The suppression takes effect on the next run, which the next push or
comment triggers anyway.

## Fork pull requests

This is the one place the trigger choice genuinely changes what is possible, and
it is easy to get wrong.

Under `pull_request`, a pull request from a fork gets a **read-only**
`GITHUB_TOKEN` and **no secrets at all**. Not a reduced set — none. So the App
private key is unreadable, no installation token can be minted, the model
gateway key is absent, and no check run can be published. The App does not
rescue this, because the workflow cannot reach the App's key in the first place.

Under `pull_request_target`, the workflow runs in the **base** repository's
context: secrets are present and the token can write. The cost is that the
checked-out contributor code is now inside a privileged context.

tinysweeper is safe there for one reason, and it is a constraint rather than a
mitigation: **it never executes anything from the tree it reviews.** It reads
the diff and reads files. It does not build, install dependencies, or run the
repository's scripts. Any step that breaks that invariant turns
`pull_request_target` into a full repository takeover for anyone who can open a
pull request — which is why the checkout step in `review.yml` carries a
load-bearing comment saying so.

Repositories that take outside contributions should use `pull_request_target`.
Repositories where every pull request comes from a branch in the same repository
should use `pull_request`, which is strictly safer.

## Scheduled work

`schedule` drives the auto-merge sweep, stale handling and Sentry promotion.
Three honest caveats:

- It is best-effort. Runs are routinely minutes late and are occasionally
  skipped under load. Never depend on a cron firing at a specific moment.
- It is **disabled automatically after 60 days of repository inactivity**. A
  quiet repository silently stops sweeping.
- It runs on the default branch's version of the workflow, not the pull
  request's.

Because of the first point, the sweep is written to be idempotent and to
reconcile from live state rather than assuming it ran last time.

## External systems

`repository_dispatch` is the supported door in: any system can `POST` to
`/repos/{owner}/{repo}/dispatches` with a token and an `event_type`.

Sentry cannot use it — its webhook integration cannot set the `Authorization`
header GitHub requires. So Sentry promotion **polls** the Sentry API from a
cron workflow instead. Slower, and entirely adequate for triage.
