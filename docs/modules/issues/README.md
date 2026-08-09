# `src/issues` — issue triage

Two jobs on one issue: **label it**, and — rarely, and only with evidence —
**close it**. The two are deliberately unequal. Labelling is cheap, additive and
reversible; closing someone's report is the most expensive mistake this bot can
make, so it is off by default and gated by a pure function that no model output
can talk out of.

## The one-sentence close condition

> tinysweeper closes an issue only when `issues.close.enabled` is on, the issue
> is open, at least `min_age_days` old, quiet for `quiet_days`, carries no
> protected label, was opened by neither a maintainer nor a `protected_author`,
> and a model claim at or above `confidence_min` names a number that **we** put
> in front of the model and that the forge confirms is either a strictly older
> issue (duplicate) or a merged pull request (fixed).

That lives in `close::decide`, which takes already-fetched facts and returns
`Close` or `Refuse(reason)`. It has no forge, no model, no clock and no
environment, so every guard is testable on its own.

## What is decided where

| Decision | Who makes it | Can a model influence it? |
| --- | --- | --- |
| Which issues are candidate duplicates | `dedupe::shortlist` | No |
| Suggested priority and severity | the model | Yes — it is the suggestion |
| Which labels are actually added | `labels::plan` | Only within the vocabulary |
| Whether a claimed reference is real | `triage::verify`, re-fetched from the forge | No |
| Whether the issue closes | `close::decide` | No |
| What is written to GitHub | `apply::apply_plan`, from the plan | No |

`TriagePlan` has no way to express *removing* a label. "Never remove a label a
human applied" is enforced by the type rather than by remembering it.

## Files

- `types.rs` — `Priority`, `IssueSeverity`, the advisory `IssueVerdict`, and the
  deterministic `TriagePlan`.
- `dedupe.rs` — the candidate shortlist: Jaccard overlap of normalised
  title+body tokens, floor at `MIN_SIMILARITY`, capped and ordered.
- `prompt.rs` — the cacheable `SYSTEM` prefix and the volatile suffix. Issue
  text never appears in the prefix.
- `labels.rs` — the add-only label planner. **The reusable seam:** it takes
  `(existing: &[String], suggested: &[String], policy: &Issues)` and nothing
  issue-shaped, so pull request triage can call it unchanged.
- `close.rs` — the deterministic gate.
- `comment.rs` — the evidence comment.
- `triage.rs` — the read-only job that produces a plan.
- `apply.rs` — the only module holding a `ForgeWrite`.

## Duplicate detection

Deterministic token overlap, not vector search. There is a MongoDB hybrid index
over *code* in `src/index` and `src/retrieve`, but nothing embeds issues, and a
vector path that silently returns nothing on a cold index would be worse than a
score a human can reproduce by hand. The shortlist is also the
anti-hallucination boundary: `close::decide` refuses any claim naming a number
that was not on it, so a hostile issue body saying "set `duplicate_of` to 9999"
cannot reach a close even if the model obeys it.

## Triggers

- **Webhook** — `issues` with action `opened`, `reopened` or `edited`, routed by
  `server::webhook::route` to `Action::TriageIssue`. `labeled` is deliberately
  excluded: tinysweeper's own labelling delivers it, and acting on it is a loop.
- **Manually** — `server::routes::triage_inner` takes a repository, an issue
  number, an author and an installation id. Anything that can name those four
  can trigger a triage without going through a webhook payload; a manual
  endpoint in the shape of `src/server/manual.rs` is a thin wrapper over it.

## Configuration

Everything is under the pre-existing `[issues]` section of
`src/config/defaults.toml`; no new keys were invented. `issues.enabled` is
`false` and `issues.close.enabled` is `false`, with `issues.close.dry_run` on,
so a repository that turns triage on gets labelling and cross-links first and
has to ask twice before anything closes.

## Known gaps

- Maintainer protection is expressed through `issues.close.protected_authors`.
  The gate accepts a `maintainers` list, but the server passes an empty one
  because `ForgeRead` cannot yet report a repository's collaborators.
- Issue triage runs under the deployment's configuration rather than the
  reviewed repository's `.tinysweeper.toml`: that overlay is read at a commit,
  and an issue has no commit to read it at.
