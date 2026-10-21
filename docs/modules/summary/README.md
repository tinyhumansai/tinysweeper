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

Summary evidence includes the pull request's title, scrubbed description, and
the forge's complete changed-path list and full pull-request diffstat. The
executive opening starts with the pull-request title before any model narrative,
so a re-review cannot lead with a claim about only the latest changed files.
The latest-push delta is separately labelled. The review state retains the
previous rendered diff, but the forge input has no base-to-head comparison
port; that snapshot cannot prove the exact push delta, so the field says it is
unavailable rather than presenting the full PR diff as a push delta. Each lane
also carries a typed `answered`, `partial`, `no_response`, or `skipped` status.
Per-file coverage counts only assigned paths, bounded to the forge's full
changed-path list; explicitly unanswered paths remain in scope even when they
had no parseable diff. Whole-pull-request lanes with waiting checks carry only
the pending-check count, without a fabricated file denominator. Completion provenance distinguishes deterministic lane
results from policy skips and unanswered model calls; old proposal shapes with
no provenance remain unknown. A sentence that explicitly names a non-answered
lane or makes a collective claim about all lanes/reviewers is discarded,
regardless of its wording or claimed verdict; unrelated sentences remain.

The public comment stays short when a review has no findings: it shows the
verdict, file coverage, and one summary sentence, then puts configured support
sections under a collapsed details block. Findings appear first when present;
each includes a link to its reviewed location, the problem, and any suggested
fix. File gaps and whole-lane no-response messages use separate lines. New
summaries carry reviewed and total file counts only when lane evidence supports
them; an older proposal without that metadata reports coverage as unavailable.
Models, cache continuity, and run diagnostics remain inside the collapsed
details block. The Mermaid flow appears only when at least four changed,
non-test symbols can be shown; test names and generic keyword nodes are left
out.

The structured schema restricts positive observations to the six known lane
keys. If a reply still fails typed deserialization, generation makes one
semantic repair request with the scrubbed rejected JSON fenced as untrusted
data. A second shape mismatch or a model/transport failure uses the
deterministic summary. Both successful replies and paid usage attached to a
failed call remain in accounting. Diagnostics record only a failure category,
the schema name, and the attempt number; they never include reply values or
serde error text.

Production logs do not retain raw provider replies for schema fallbacks. The
contract regression uses a synthetic witness derived from the difference
between the permissive wire schema and the typed lane-key parser; it is not a
captured production response.

When no supported change explanation exists the "What changed" section is
omitted rather than filled with failure text, and the update time renders as
ISO 8601 UTC (`2026-10-08T11:39:30Z`), never as raw epoch seconds.

Readiness, priority, change counts, findings, incomplete work, the before-merge
checklist, usage, and bounded pass history are deterministic. Inline findings,
lane checks, approvals, and changes-requested reviews remain the enforcement
surfaces; the hub only explains their combined result.

Generated text is escaped as prose, paths and symbols use backtick spans sized
to their contents, and table cells receive table-specific escaping. A validated
`path:line` citation links to that path and line at the exact reviewed head;
symbol-only citations stay in code spans. These contexts share the helpers in
`src/markdown.rs`, so a title, path, and table cell do not accidentally inherit
one another's escaping rules.

The server migrates a bot-authored `tinysweeper:change-map` comment in place.
Markers copied into contributor comments are ignored. During a new pass the
comment shows the new head as in progress and retains the previous completed
report. Stable comment markers delimit the preserved report, so repeated
in-progress and failure updates unwrap the previous status wrapper instead of
nesting it. A failed pass uses a short, human-readable cause and keeps the
completed report under a prominent warning, naming the report's reviewed head.
Provider error text and pull request references are not copied into that
failure notice.

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
