# `e2e-required`

For repositories with an end-to-end suite worth holding changes to.

Runs every default lane plus `e2e`, which is off in every other preset. That
lane asks two questions the `tests` lane deliberately does not:

1. Is each behavioural change reachable by an end-to-end test — one that
   drives the running system the way a user, a client or an operator would?
2. Did the repository's own end-to-end jobs actually **run** on this head, and
   what did they conclude?

The second is where the value is. A workflow with a stale `paths:` filter, or
one that only runs on `workflow_dispatch`, leaves a green pull request whose
e2e suite has an opinion about nothing. The lane reads the workflow at head,
decides in code whether it would trigger, and reads the check runs to see
whether it did. A job still running when the review finishes leaves the
`tinysweeper/e2e` check `neutral`; the server concludes it when the job does.

Nothing is executed. The repository's CI is the hands; tinysweeper reads what
it reported. See `docs/modules/lanes/e2e.md` for the design.

## What it assumes

- The tree has an e2e harness: test files under `e2e/`, `tests/e2e/`,
  `cypress/`, `playwright/`, `integration/`, `acceptance/`, `smoke/`, or named
  `*.e2e.*` / `*.feature`, and a workflow named for it or with a step that
  runs Playwright, Cypress, `docker compose up`, testcontainers or k6. With
  `missing_harness = "require"`, a tree with neither gets one finding asking
  for one.
- Jobs are named as their check runs are. GitHub names a job's check run after
  its `name:` (or key), with matrix values in parentheses; that is what the
  lane matches.

## When the detection guesses wrong

```toml
preset = "e2e-required"

[lanes.e2e]
paths = ["qa/**/*.ts"]            # replaces the path table
workflows = ["browser-suite"]     # replaces name-and-step detection
```

Each override replaces the detection it names rather than extending it: a
repository that says where its suite lives has said the default guess is
wrong for it.
