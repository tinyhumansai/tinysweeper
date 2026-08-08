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
