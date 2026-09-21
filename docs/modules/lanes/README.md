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
| `commits` | `tinysweeper/commits` | What entered the history — **no model call** | `secret`, `blob`, `junk` |
| `description` | `tinysweeper/description` | Title and body against the diff | — |
| `e2e` | `tinysweeper/e2e` | Whether changed behaviour is reachable end to end, and whether the repository's e2e jobs ran on the head | — |

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

## The `commits` lane makes no model call

It republishes what the deterministic scanners found in the commit range —
secrets, oversized blobs, committed build output — and does nothing else. No
prompt is built, no tokens are spent, and its verdict is a regular expression's
rather than a model's.

It used to also judge the range itself: messages that describe nothing, merge
noise, unrelated work bundled together, an author identity that looks
accidental. That job is gone. Recorded here because the temptation to restore it
is obvious and the reason not to is not.

A model asked "is anything wrong with these commits?" will always find
something. Commit prose is infinitely criticisable and the question presumes a
defect, so the lane produced a steady stream of style objections — on a
repository whose commits are frequently written by an automated checkpointing
hook, attached to a check that could block a merge. One of them arrived carrying
the rule `Commit message style only — not flagged` and flagged it anyway.

The judgement was also the part nobody could audit, and the scan is the part
anybody can. Removing it makes the lane's verdict deterministic — it fails when
a scanner matched and for no other reason — which is a stronger security
property than it had before, and it costs nothing rather than one model call per
review.

**It is the one lane that still runs on a draft.** Every other lane defers,
because its opinion can wait. A committed credential cannot: it is in the
history the moment it is pushed, and marking the pull request draft afterwards
does not take it back out.

## Anchoring

`lanes::anchor` holds the two rules, and the difference between them matters:

- **Strict** (`critique`, `security`, `tests`, `e2e`) — a finding must sit on a line
  this pull request changed, or it is dropped and counted into the summary.
  A comment on unrelated code is the fastest way to lose a team's trust.
- **Demote** (`commits`, `description`) — the subject is a commit message or a
  missing body, which has no line at all. The bad anchor is removed rather than
  the finding, and `apply` renders it in the check-run summary instead of as an
  inline comment.

  This rule governs the *model's* findings; it does not reach `e2e`'s
  deterministic ones. `e2e-not-triggered` and `e2e-failed`
  (`src/lanes/e2e/runs.rs`) are built by code, not returned from
  `LaneResponse`, so they never pass through `LaneOutcome::from_response` and
  are neither dropped nor demoted — `e2e-not-triggered` anchors on the
  workflow's `paths:` line when there is one, `e2e-failed` carries no line at
  all, and both are always rendered in the summary regardless of whether that
  line changed. See `docs/modules/lanes/e2e.md`.

## Per-file fan-out

`security` and `critique` run one conversation per changed file
(`lanes::fanout`), capped at `MAX_CONCURRENT_FILES`. Each conversation is told
it owns exactly one file and must not report on any other — without that clause,
every one of the N reviewers notices the same cross-file problem and the author
gets it N times. One file's failure is collected, not propagated: the rest are
still reviewed and the summary says which were not.

`critique` reviewed the whole pull request in one call until a 31-file change
landed carrying two real correctness bugs, and the lane answered with a single
hallucination — the exact failure `lanes::fanout`'s module doc predicts, where
the first few files are read closely and the rest are an afterthought. An
external reviewer found both. The subject of a `critique` conversation is one
file's correctness, so nothing is lost by splitting it.

**`tests` deliberately does not fan out.** Its subject is the relationship
between two *sets* of files — whether the tests in this pull request cover the
behaviour it changed — and a reviewer shown one file in isolation cannot see it.
Fanning it out would not make it more thorough, it would make the question
unanswerable.

A per-file lane also skips a file it reviewed before and that has not changed
since (`replay::unreviewed`), rather than replaying it into the cacheable
prompt prefix the way a whole-diff lane does. Both are sound; skipping is
strictly better, because it pays always and a cache prefix only pays when the
provider honours it.

A reviewer in a per-file conversation is not confined to the hunk. It is
handed the definitions its changed lines call into before its first turn,
and may read a file range or search the tree before it answers, up to
`[lookup].rounds` times — see [`lookup.md`](lookup.md). The prompt used to
say the opposite, and the reviewer that had the right doubt on
opencompany#2313 obeyed it and filed nothing.

`critique` also verifies a **mechanical substitution** before the fan-out
(`lanes::mechanical`). When one literal replacement explains a file's whole
diff line for line — a rename across fifty files — the file is proven here,
named in the summary as verified; one sample of it is still read so a uniform but semantic substitution is judged, and the rest are never sent to a model. The check is
exact, so its only failure is a false negative: a file with one line that is
not the substitution goes to the model like any other. On the pull request
that motivated it, 50 of 58 files were the rename and $0.20 of $0.23 had
gone to reading them.

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

## Grouping

Isolation cuts both ways. Telling every conversation to ignore every other file
stops N reviewers reporting one cross-file problem N times, and it also hides a
bug that only shows up by reading two files together: a caller changed in `a.rs`
while its callee changed in `b.rs`, or a function and the test that exercises
it. Neither ungrouped conversation ever sees both halves.

`lanes::grouping` decides — deterministically, **no model call** — which of a
lane's changed files travel together in one conversation instead. Two files are
grouped when:

- the code graph has a `Calls`, `References`, `Tests`, `Imports` or `Extends`
  edge between a symbol in one and a symbol in the other, read off the same
  neighbourhood `graph::impact` and `overview` already walk for the changed
  set — no second query; or
- a name heuristic matches with no graph at all: a file and its test
  (`foo.rs`/`foo_test.rs`, `test_foo.py`, `foo.test.ts`, `FooTest.java`), a
  pair of locale files (`messages.en.json`/`messages.fr.json`, or `i18n/en.json`
  next to `i18n/fr.json`), or a component and its co-located stylesheet
  (`Button.tsx`/`Button.module.css`).

A grouped conversation is handed every file's diff and one isolation clause
naming all of them — see `harness::prompt::isolation_clause` — and its lookup
seeding (`flows::lookup::Ledger::seed`) reads the definitions every file's
changed lines call into, not just the first file's, so grouping a file with its
test does not regress the seeding that found the boundary bug on
opencompany#2313 (see [`lookup.md`](lookup.md)). A finding is placed against
whichever file in the group it actually names; one naming a path outside the
group is discarded exactly like a file the pull request never touched.

**A component over `[grouping].max_files` or `max_hunk_chars` falls back to
singletons — every one of its files reviewed alone, never a partial group.**
Grouping is a bet that one conversation reviews a handful of related files
better than several isolated ones; a bet with too many files or too much diff
in it is the same failure per-file fan-out exists to prevent in the first
place — the first few files read closely, the rest an afterthought — so it is
not made at all. `max_files = 4` and `max_hunk_chars = 20000` are chosen to
comfortably hold a file and its test, or the few files one rename touches,
while catching that case well before it does.

`[grouping].enabled = false` disables grouping entirely and returns to the
plain one-conversation-per-file fan-out, byte-identical to the prompts sent
before grouping existed, which is what keeps an operator's prompt cache and any
recorded eval cassette valid across the change.

## Below the gate, above notice

A finding that misses the posting gate but is at least `medium` and at least
`review.note_confidence` sure is named in the check-run summary under *Worth
a look* — never a comment, never a block, never counted toward the
conclusion. The gate exists so a half-sure reviewer does not block a merge;
it was also the reason a correct `medium/0.61` boundary bug reached nobody.

## Coverage pass

`review.passes = 3` ships as the maximum adaptive depth. Small groups still
take exactly one pass. For qualifying groups, `critique` and `security`
each ask their group's first council reviewer — index `0`, never the whole
council again — up to two more times after round one's own findings are placed
(and, for `critique`, falsified), told plainly what it already found and
asked to look for what a first pass misses. `lanes::coverage` builds that
call; see its module doc for why anchoring the answer is left to the caller
rather than done once in that module.

This is recall, not verification — the opposite direction from
`src/falsify`, which asks "is this correct" of a reviewer that saw less than
the first one did. Asking the *same* reviewer to look again, told what it
already said, is cheap enough to offer at all because it reuses round one's
own prompt prefix, evidence and `flows::runner::ask_all` entry point for
each extra call.

Two things keep it from being a second council for every unit:

- **A line gate.** `COVERAGE_PASS_MIN_LINES` (40, one constant per lane) has
  to be cleared by the *group's* own changed lines before the second prompt
  is even built. A rename or a one-line fix never pays for a call it cannot
  use.
- **Dedupe before falsify.** A new finding that `council::agree::corroborates`
  a round-one finding, or shares its `Finding::fingerprint`, is dropped before
  anything else runs against it — `critique`'s falsify call included, which
  is why that call is free when a coverage pass finds nothing new.

At the default ceiling, the second coverage pass is told about everything the
first found in addition to round one's own list. An empty, failed, malformed,
entirely duplicate, unplaceable, or fully filtered pass stops the loop rather
than paying for the next one.

Each qualifying group emits one structured telemetry event after it stops.
`passes_attempted` and `new_findings_per_pass` both begin with round one, then
list every adaptive attempt; `stop_reason` distinguishes reaching the ceiling
from an empty, failed, unplaceable, duplicate, or filtered response.
`input_tokens`, `output_tokens`, `cached_tokens`, and `cost_usd` are summed from
the model responses made by that group rather than inferred from the lane-wide
spend tally, which is shared by concurrently reviewed groups.
`model_elapsed_ms` is the accumulated model wait while `elapsed_ms` measures
the whole adaptive sequence.
`review.passes` is not in `config::remote::OVERRIDABLE_KEYS`: each pass above
one is another model call per qualifying unit, and that is the operator's
money, exactly like the per-pull-request budget in `[models]`.

## Rule documents

Per-path review rules live under `presets/rules/` as data, selected by the
ordered `path_instructions` table — **first match wins**, so a Rust file's
reviewer never sees the workflow rules. An entry can opt out of that with
`merge = true`, which also takes the next matching entry — one level only — so
a specific entry (`src/ports/**`) can keep the broader language document
(`rust.md`) beneath it instead of duplicating it. Roughly half of each document
is the "do NOT report" list; that half is where the precision comes from. See
`presets/rules/README.md`.

## The `e2e` lane is quiet without a harness, and settles later

It owns end-to-end coverage and whether the repository's own e2e jobs ran on
the head — the concern the `tests` rule document deliberately excludes. It is
on by default and skips, with no model call, on a repository that has no e2e
harness; `presets/e2e-required/` turns that skip into a finding. Opt out by
listing `review.lanes` without it. Its harness inventory, trigger analysis and job states are decided in code
before any model call, and a job still running when the review finishes
leaves the check `neutral` until the server settles it on the job's
completion. See [e2e.md](e2e.md).

## Adding a lane

1. A new file in `src/lanes/`, implementing `Lane`.
2. Its instructions in `harness::prompt::instructions`.
3. A dispatch arm in `src/app/review.rs`.
4. A golden test: fixture diff, canned `MockModel` response, assertions on
   exactly the findings that survive filtering, dedupe and capping.
