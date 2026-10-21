# Bounded reviews for large pull requests

TinySweeper plans review work before paid instruction extraction and the lanes.
It uses the complete forge diffstat, including files whose patches are missing,
and the eligible `critique` and `security` groups after normal preparation.
A group belongs to one lane: the same paths can contribute a group to each lane.

## Operator settings

These are the shipped defaults in `src/config/defaults.toml`:

```toml
[review.large_pr]
enabled = true
files = 30
changed_lines = 2000
groups = 24
max_groups = 12
max_calls = 48
soft_deadline_percent = 80
drain_seconds = 30
```

Large mode is selected when **any** enabled threshold is reached: at least
30 changed files, 2,000 added plus deleted lines, or 24 eligible lane groups.
It selects at most 12 groups across both lanes, not 12 per lane. A related-file
group stays intact; it is never split to fit the selection limit.

All numeric settings must be positive; `soft_deadline_percent` must be between
1 and 95. These settings belong to the operator and cannot be changed by the
reviewed repository's remote config. Setting `enabled = false` disables Large
selection; the soft deadline still applies in Normal mode. The separate
`review.max_changed_files` and `review.max_changed_lines` resource ceilings
still reject oversized input before model work.

## Selection and review depth

Large mode orders groups by their highest-risk member. Risk is compared in this
order: deterministic scanner evidence, a prior posted finding, sensitive paths
or dangerous sinks, public API changes, then added plus deleted lines capped at
1,000 per file. Ties use total group changed lines, path order, and security
before critique. These signals come from host evidence, not a model score.
Documentation remains eligible for critique.

The selected critique and security groups share one ranked dispatcher.
Large mode forces one pass and disables council reviewers and subagents.
Placement and falsification still run: a first answer alone does not prove a
group was fully reviewed. Normal mode retains configured passes and council
behavior.

The physical call ceiling covers the whole review, including instruction
extraction, JSON repairs, route fallbacks, truncation retries, agent turns and
later model stages. It is separate from `models.budget_usd_per_pr`. A new lane
or derived model scope does not refill either review-wide allowance.

## Soft stop, drain and hard deadline

The server starts its 900-second review clock after obtaining a review permit,
before metadata and checkout. Retries retain that clock. With the defaults,
the soft cutoff is 720 seconds from that original start; checkout does not buy
another 720 seconds. Public local review entry points use a fresh 900-second
window.

At soft time, stop starting groups and new host model requests in both modes.
Admitted work may drain until the earlier of soft time plus `drain_seconds`
or five seconds before the hard deadline. Unfinished futures are dropped at
that cutoff. If setup has already used the soft allowance, the review starts
no model work and reports zero reviewed paths with honest gaps. Prepared
pinned knowledge is retained when instruction extraction cannot run.

After a stop, deterministic scanners, filtering, finding caps and proposal
construction still run. The summary uses actual outcomes without another
summary model call. New model thread-resolution and wireframe work is skipped;
previous continuity is retained. The server hard timeout remains the final
failure boundary, and publication retains its separate server timeout.

## What the report means

The compact hub names coverage gaps; collapsed Diagnostics carries the full
path lists and reasons:

- `not reviewed (large PR)`: a candidate group was outside the selected set.
- `not reviewed (call budget)`: the shared call ceiling stopped its work.
- `not reviewed (time limit)`: soft time or the drain cutoff interrupted it.

Interrupted groups leave every member unanswered. Missing forge patches remain
unreviewed too. A whole-PR lane cannot mark those files reviewed merely by
answering. `Proposal.partial_review` records host-derived mode, estimate,
selection and omissions; it is not model-authored evidence.

A stopped lane without a qualifying blocking finding is Neutral. A real finding
that meets the existing blocking floor still yields Failure. Incomplete
coverage cannot approve the PR or qualify it for automerge; `Unverified`
advisories retain their existing nonblocking treatment.

A successfully published partial review is terminal Published, not a hard
deadline failure. It does not requeue the same head or reset its paid attempts.
Manual Full reviews remain separate jobs and bypass incremental replay, while
retaining the same size, call and deadline policy.

## Model adapter boundary and accounting

The production Gateway applies execution control at each admitted model
dispatch, including hidden repairs and agent turns. A dispatch already admitted
before soft time may finish while the group drains. The host uses the same
absolute drain cutoff around `Embedder::embed` and `embed_query`, including
retrieval's query call, so a slow embedding cannot hold publication past the
run window. A dropped embedding result contributes no invented usage. Known
completed model usage survives cancellation and is counted once; missing
provider usage or cost stays explicitly incomplete rather than becoming a zero
bill.

An opaque custom `Model` without execution control remains usable in Normal
mode: the host stops new logical requests and cancels unfinished work at the
drain cutoff. It cannot guarantee what retries that backend performs inside an
already-running future. Large mode refuses an unsupported adapter before paid
extraction or model work.

Cassette Record preserves the live adapter's control. Strict and Loose replay
are historical response data and report no current physical dispatch capability.
A replay admission adapter, when explicitly supplied, bounds logical replay
work; it does not establish historical physical call counts.

See the [configuration reference](../config/README.md),
[application verdict rules](../app/README.md), and
[server scheduling and failure handling](../server/README.md).
