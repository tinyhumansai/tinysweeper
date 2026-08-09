# `src/lanes`

One agent, one narrow job, one GitHub check run. A lane takes evidence and
returns a `LaneOutcome`. It does **not** take a `ForgeWrite`, so it cannot
mutate a pull request even by mistake — lanes propose, `src/apply` disposes.
That boundary is enforced by the type system rather than by discipline.

## The lanes

| Lane | Check run | Subject | Scanner kinds it adjudicates |
|---|---|---|---|
| `critique` | `tinysweeper/critique` | Correctness of the diff | — |
| `security` | `tinysweeper/security` | What the change makes attackable | `workflow`, `dependency` |
| `tests` | `tinysweeper/tests` | Whether changed behaviour is covered | — |
| `commits` | `tinysweeper/commits` | The commit range and what it committed | `secret`, `blob`, `junk` |
| `description` | `tinysweeper/description` | Title and body against the diff | — |
| `gate` | `tinysweeper/gate` | Deterministic aggregate of the others | — |

The scanner-kind column is a **partition, not an overlap**. Each deterministic
finding has exactly one owning lane, because two lanes discussing one match
reports it twice. `src/app/review.rs` republishes a kind itself only when its
owner never ran — disabled in config, or skipped as a draft — so nothing
vanishes when a lane is switched off.

## Scanners first, model second

The deterministic scanners in `src/scan/` run before any token is spent. Their
findings are facts, and a lane that owns a kind:

1. republishes those findings **unchanged**, and
2. hands them to the model as evidence to adjudicate — say whether each is real
   here and why.

A model verdict never deletes one. "The reviewer was talked out of a committed
private key" is not a failure mode anyone can audit, and the point of running a
regular expression first is that it cannot be argued with. What a model *can*
do is add what a scanner cannot see, and its findings are dropped when they
merely restate a scanner match on the same path and rule.

## What the `commits` lane is shown

`git log -p` over the range: each commit's message *and* the patch it
introduced, fetched per commit by `ForgeRead::commit_patch` and assembled by
`pull_request_context`. Without the patches the lane could only read subject
lines, and it built a confident security finding out of the phrase "kernel
bypass" in one (issue #47).

Two bounds, both stated in the evidence rather than applied silently:

| Bound | Value | What happens past it |
|---|---|---|
| Commits rendered | 50 | Listed as "… and N more commits, not shown." |
| Commits fetched | `ports::forge::MAX_PATCHED_COMMITS` (50) | `patch: None`, rendered as "no patch was fetched" |
| Patch bytes per range | 48 KiB | Message still shown, patch omitted, count reported |
| Patch bytes per commit | 12 KiB | Cut on a line boundary with the dropped byte count |

A commit with no patch is never rendered as an empty diff. The instructions let
the lane judge such a commit's message as a message and forbid it from inferring
what the commit did — a distinction it can only make if the evidence draws it.

The lane's findings then go through `src/falsify` before the scanner findings
are merged in. That order is the invariant: a model's claim about the range is
exactly what a falsifier can prove wrong from the patches, and a scanner's match
is never up for a model's opinion.

## Anchoring

`lanes::anchor` holds the two rules, and the difference between them matters:

- **Strict** (`critique`, `security`, `tests`) — a finding must sit on a line
  this pull request changed, or it is dropped and counted into the summary.
  A comment on unrelated code is the fastest way to lose a team's trust.
- **Demote** (`commits`, `description`) — the subject is a commit message or a
  missing body, which has no line at all. The bad anchor is removed rather than
  the finding, and `apply` renders it in the check-run summary instead of as an
  inline comment.

## Per-file fan-out

`security` runs one conversation per changed file (`lanes::fanout`), capped at
`MAX_CONCURRENT_FILES`. Each conversation is told it owns exactly one file and
must not report on any other — without that clause, every one of the N reviewers
notices the same cross-file problem and the author gets it N times. One file's
failure is collected, not propagated: the rest are still reviewed and the
summary says which were not.

Before the fan-out, `lanes::triage` decides deterministically — for free, with
no model call — which changed files are worth one and in what order:

- **Skipping** is narrow. Only lockfiles, vendored and build output, prose,
  binary assets, snapshots and generated code are dropped, and never a path a
  scanner already flagged. Agent instruction files (`AGENTS.md` and friends) are
  explicitly *not* prose here: a tool reads them back as instructions, so a
  change to one is attack surface. Every skip is named in the lane summary,
  because a review that quietly skipped half a pull request reads exactly like
  one that found nothing wrong with it.
- **Ordering** is aggressive, because it can only change *when* a file is
  reviewed, never *whether*. Added lines that reach a dangerous sink, and paths
  naming an authorisation or credential boundary, go first; tests go last. That
  matters because `per_file_with_budget` spends in order, so an exhausted budget
  has bought the riskiest files rather than the alphabetically luckiest ones.

`tests`, `commits` and `description` are pull-request-scoped. Their subject is a
relationship between files, and a reviewer shown one file cannot see it.

## Rule documents

Per-path review rules live under `presets/rules/` as data, selected by the
ordered `path_instructions` table — **first match wins**, so a Rust file's
reviewer never sees the workflow rules. Roughly half of each document is the
"do NOT report" list; that half is where the precision comes from. See
`presets/rules/README.md`.

## Adding a lane

1. A new file in `src/lanes/`, implementing `Lane`.
2. Its instructions in `harness::prompt::instructions`.
3. A dispatch arm in `src/app/review.rs`.
4. A golden test: fixture diff, canned `MockModel` response, assertions on
   exactly the findings that survive filtering, dedupe and capping.
