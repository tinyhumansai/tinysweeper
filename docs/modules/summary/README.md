# `summary`

`summary` owns Tiny Sweeper's durable pull-request review hub: one early issue
comment marked with `<!-- tinysweeper:review-hub -->`, updated in place for the
life of the pull request. GitHub has no API for pinning a PR comment, so the
server creates this comment after acquiring the review lease and before any
model call.

The model generates only narrative fields: the executive summary, behavioral
change explanation, cited features, cited test mappings, and positive lane
observations. A generated feature or test claim is discarded unless every
citation names a changed path or known changed symbol. Unsupported claims that
tests ran, passed, or reached numerical coverage are discarded as well.

Readiness, priority, change counts, findings, incomplete work, the before-merge
checklist, usage, and bounded pass history are deterministic. Inline findings,
lane checks, approvals, and changes-requested reviews remain the enforcement
surfaces; the hub only explains their combined result.

The server migrates a bot-authored `tinysweeper:change-map` comment in place.
Markers copied into contributor comments are ignored. During a new pass the
comment shows the new head as in progress and retains the previous completed
report. A failed pass keeps that trustworthy report under a prominent warning.

Summary conversations are stored as exact evidence/assistant pairs so previous
messages remain a byte-stable provider-cache prefix. The chain has a fixed
storage ceiling; crossing it restarts continuity from the latest structured
summary and records that restart in the run details.

Configuration is deliberately closed:

```toml
[summary]
enabled = true
sections = ["snapshot", "changes", "features", "tests", "findings",
            "before_merge", "flow", "agent_details", "run_details"]
max_features = 8
max_tests = 8
history_entries = 5
```

Repositories may disable or reorder sections and lower the three limits from
base-branch configuration. They cannot raise operator limits, select the model,
alter prompts, or change persistence and write behavior.
