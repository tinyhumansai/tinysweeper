# The `e2e` lane

The argument for a sixth lane, and how it is built. The lane lives in
`src/lanes/e2e/`: `inventory.rs` (the harness and the trigger analysis),
`runs.rs` (job states, the deterministic findings, and settling), `evidence.rs`
(the reads through `ForgeRead`), and `mod.rs` (the lane). The write that
concludes a pending check is `app::apply::settle_e2e`.

## The question it answers

The `tests` lane asks: *did behaviour change, and does a unit test now fail if
it regresses?* It is deliberately narrow, and its rule document says so:

> Do NOT report: the absence of an integration or end-to-end test, unless the
> repository's own policy asks for one.

That exclusion is right for `tests` and leaves a hole. A feature can be unit
tested to the line and still never have been *ingested*: the route is
registered but no request has been sent to it; the CLI flag parses but no
invocation has run; the migration applies but nothing has read the table
afterwards. The `e2e` lane owns that hole. It asks two questions about each
behavioural change in a pull request:

1. **Is it verifiable end to end?** Is there an end-to-end test, or an
   end-to-end job in the repository's own CI, whose scope reaches this
   change — and if not, could there be, or is the change genuinely
   unobservable from outside?
2. **Was it verified on this head?** Did that test or job actually *run* on
   this pull request's head commit, and what did it conclude?

The second question is the one nobody currently asks. The common failure is not
"the e2e suite is red"; branch protection catches that. It is "the e2e workflow
has a `paths:` filter, or is `workflow_dispatch` only, or is gated on a label,
and it never triggered for this change" — a green pull request whose e2e suite
has an opinion about nothing.

## What the lane does not do

**It does not run anything.** The security boundary in `AGENTS.md` stands:
contributor code is never executed, and this lane holds a `Model` and a
`ForgeRead` and nothing else. The same source-text assertion `lanes::tests`
carries (`the_lane_never_executes_anything`) is carried here.

That is not a limitation to be worked around; it is the design. The
repository's own CI already runs its e2e suite, with its own secrets, in its
own trust domain — the same split as the UI preview (`docs/modules/preview`):
the hands are the repository's workflows, the brain is this lane reading what
the hands reported. GitHub check runs on the head SHA are that report.

**It does not replace `tests`.** The partition rule from `docs/modules/lanes`
applies: `tests` owns unit coverage and assertion quality, `e2e` owns
end-to-end coverage and whether the e2e path was exercised. Neither lane
speaks about the other's subject, so an author is never told the same thing
twice.

**It is opt-in.** It is absent from the default `review.lanes`. Demanding an
e2e test from a repository that has no e2e harness is exactly the noise the
gates exist to suppress, and the `tests` rule document's exclusion was written
after seeing it. A repository enables the lane when it has a harness worth
holding changes to.

## Evidence

Everything a lane sees today is in `LaneInput`: the diffs, the changed files'
content, the retrieved neighbourhood, the memory. `e2e` needs three things the
input does not carry, all gathered by `src/app/review.rs` before the lane
runs, all deterministic, all through `ForgeRead`.

### 1. The e2e inventory of the tree at head

Which files in the repository *are* e2e tests, and which workflows *are* e2e
jobs. This is a path-table question, like `tests::Inventory`, and it is
answered before a token is spent.

- **Test paths.** `e2e/`, `tests/e2e/`, `test/e2e/`, `cypress/`, `playwright/`,
  `integration/`, `tests/integration/`, `features/**/*.feature`, `*.e2e.*`,
  `*.integration.*`, `acceptance/`, `smoke/`. Overridable per repository with
  `lanes.e2e.paths`, because these conventions are looser than unit-test
  conventions and a repository that keeps its e2e suite in `qa/` should be
  able to say so once.
- **Workflows.** Every `.github/workflows/*.yml` whose name, job names or
  steps say e2e: a name matching `e2e|end-to-end|integration|acceptance|smoke`,
  a step that runs `playwright`, `cypress`, `docker compose up`, `testcontainers`,
  `k6`, or a service container block. Overridable with `lanes.e2e.workflows`,
  a list of workflow or job names that count. The scanner in
  `src/scan/workflows.rs` already parses these files; the classification is a
  second reader over the same YAML, not a second parser.

This needs a tree listing: `ForgeRead::tree_paths(repo, sha) -> TreeListing`,
backed by the recursive `git/trees` endpoint on GitHub, with `MockForge`
serving a canned list. One round trip. GitHub truncates past 100 000 entries,
and the listing carries that flag so a truncated tree is reported in the lane
summary rather than silently treated as complete. `IndexManifest::paths` was
the alternative and is the wrong key — it is tied to an embed signature, not
to a commit.

### 2. The e2e workflows' trigger conditions

For each e2e workflow, whether it *would* run for this pull request:

- `on:` includes `pull_request` (or `pull_request_target`) — or it is
  `push`-to-main only, `workflow_dispatch` only, or `schedule` only, in which
  case it never runs on a pull request and every pull request is unverified.
- `paths:` / `paths-ignore:` filters, evaluated against the changed paths.
  This is the one that bites: a filter written when the e2e suite covered the
  frontend, still in place after the suite grew a backend job.
- A job-level `if:` naming a label (`contains(github.event.pull_request.labels.*.name, 'run-e2e')`)
  that the pull request does not carry.

All of this is read from the workflow file at the head SHA via
`ForgeRead::file_at` and decided in code, by an indentation-outline reader
rather than a YAML parser — `on:`, `paths:`, `jobs:` and `steps:` are all it
needs, and the crate's dependency set stays where it is. The model is told
the answer, in the same spirit as `tests::Inventory::render`: a model that
guesses wrong about whether a workflow triggers produces a finding on a wrong
premise.

### 3. What ran on this head

`ForgeRead::check_runs(repo, head_sha)`, which `src/automerge` already calls,
matched against the e2e workflow and job names from (1). Each e2e job is in
one of four states:

| State | Meaning |
|---|---|
| `passed` | Ran on this head and concluded success. |
| `failed` | Ran on this head and concluded failure, cancelled or timed out. |
| `pending` | Queued, in progress, or expected by (2) and not yet reported — GitHub creates the check run when the job is queued, which can be after the review starts. Never treated as a pass. |
| `not triggered` | No check run on this head under that name and (2) explains why; or the forge reports the job `skipped`, so a condition the outline could not read was false. |

A check run that exists is the fact; the trigger analysis only explains an
absence. A job that ran despite a filter the outline misread is reported as
what it did.

### What the graph could add

`graph::impact` already emits `Impact::untested`: changed symbols nothing in
the graph exercises. Its `Tests` edges do not distinguish an e2e test from a
unit test — the extractor marks a scope as a test by `#[test]`, `test_`,
`Test` prefix — but the file path does. Filtering the test-scoped callers by
the e2e path table would give the list of changed symbols an e2e test
*directly* reaches. The lane already receives the retrieved neighbourhood
through `LaneInput::retrieved_context`; the filtered view is not wired yet.

That list would usually be short, and the lane must not read its shortness as
absence. E2e tests reach features through HTTP paths, CLI arguments, UI text
and configuration keys, not through symbol calls. So the bridge the first cut
ships is lexical, in `evidence.rs`: the quoted literals and route- or
flag-shaped tokens the diff added (a route, a subcommand, a flag, a button
label, an env var — deliberately not identifiers) searched for in the e2e
test files, read through `file_at` under a budget (`MAX_TEST_FILES`,
`MAX_TEST_CHARS`), files the pull request itself changed first. Hits are
handed to the model as *candidate* coverage with the matching lines quoted;
the model decides whether the candidate actually drives the changed
behaviour or merely mentions the same word. Routing the search through the
index instead is the obvious next step once the harness is indexed.

## The prompt

One conversation for the whole pull request, like `tests` and for the same
reason: the subject is a relationship between the change and a suite, and a
reviewer shown one file cannot see it. No fan-out.

The evidence, in order, above the diff:

```
Files changed, already classified:
- behaviour: src/server/routes.rs, src/preview/apply.rs
- e2e tests changed: (none)
- neither: docs/modules/preview/README.md

End-to-end harness in this repository:
- tests: e2e/preview.spec.ts, e2e/webhook.spec.ts (2 files, 340 lines)
- workflows: .github/workflows/e2e.yml
    triggers on pull_request, paths: ["src/server/**", "e2e/**"]
    jobs: e2e-playwright
- on this head (abc123):
    e2e-playwright: NOT TRIGGERED — paths filter does not match src/preview/**

Candidate coverage (lexical, verify before trusting):
- e2e/preview.spec.ts:41  `await request.post('/preview/sessions', …)`
    mentions: /preview/sessions  (added at src/server/routes.rs:88)
```

Then the diff, then the retrieved context, memory and rules exactly as every
other lane. The instructions ask for a decision per behavioural change:
covered, uncovered-but-coverable, or unobservable — and for the last, why
(needs a third party, needs hardware, is internal refactoring with no
external effect).

## Findings

Rules the lane may raise, and who decides each:

| Rule | Decided by | Anchoring |
|---|---|---|
| `e2e-not-triggered` | code — from (2) and (3) | Demote: the workflow file line with the filter, if it is in the diff; otherwise the summary |
| `e2e-failed` | code — from (3) | Demote |
| `e2e-uncovered` | model | Strict: the changed line that introduces the uncovered surface |
| `e2e-weakened` | model | Strict: `test.skip`, `.only`, a raised retry count, a lengthened timeout, a deleted assertion in a changed e2e test |
| `e2e-unobservable` | model, `Severity::Info` | Strict; never fails the check, exists so the author's decision is recorded |

The first two are deterministic and are republished unchanged, on the
`commits`-lane principle: a model verdict never deletes a fact the code
established. "The reviewer decided the untriggered workflow did not matter" is
not a failure mode anyone can audit.

A repository on `missing_harness = "require"` whose tree has no harness at
all gets one more, `e2e-missing-harness` (Medium, Demoted), with no model
call.

`e2e-pending` is **not** a finding. It is a conclusion — see below.

## The timing problem, and the check run's lifecycle

A review is triggered by the push. An e2e suite takes twenty minutes. When the
lane runs, the honest state of most e2e jobs is `pending`, and neither
"passed" nor "failed" is a claim the lane can make.

So the check run has two phases:

1. **On review**, the lane publishes the static half — inventory, trigger
   analysis, coverage findings — and, if any e2e job is pending, concludes
   `Neutral` with a summary that says which jobs it is waiting on. If a job is
   already `not triggered`, that is final and is reported now.
2. **On `check_run`/`check_suite` completed** for the same head SHA — the
   event the webhook already routes for auto-merge — the server runs
   `apply::settle_e2e` before it reconsiders the merge
   (`routes::handle_check_completed`). It re-evaluates *only* step (3):
   `runs::settle` over fresh check runs and the `Watch` the review recorded
   in `ReviewedState::e2e`, and publishes the terminal conclusion. No model
   call. This is the cheapest re-run in the system, and it must be, because
   a busy repository emits one of these events per job; the cheap exits (no
   watch, head moved, still pending) come before any credential is minted.

`LaneOutcome::pending` names the jobs still to hear from; `lane_proposal`
turns a non-empty list into `Neutral`, and the review writes the `Watch`
(head SHA, jobs, the static summary, whether the static half already failed)
into the state record. The watch is keyed to its head SHA, so a completion
for an older commit is ignored and a new push starts over.

One limit worth knowing: the pull request *review* (approve /
request-changes) is decided at review time, when the lane is still `Neutral`.
A job that fails later fails the `tinysweeper/e2e` check run — which is what
branch protection reads — but does not retroactively convert an approval
into a changes-requested verdict.

## Configuration

```toml
[review]
lanes = ["critique", "security", "tests", "commits", "description", "e2e"]

[lanes.e2e]
model = "scan"
fail_on = "high"
# When the tree has no e2e harness at all: "skip" says so in the summary and
# stops; "require" raises one Demoted finding asking for one. Default skip.
missing_harness = "skip"
# Overrides for the path table and workflow detection. Empty means detect.
paths = []
workflows = []
```

Severity defaults: `e2e-failed` high, `e2e-not-triggered` high when the
change touches a path the harness plausibly covers (a behaviour file under a
directory an e2e test already references) and medium otherwise,
`e2e-uncovered` medium, `e2e-weakened` high, `e2e-unobservable` info.

A preset `presets/e2e-required/` carries the enabled lane and a rule document
`presets/rules/e2e.md`, of which — as with every rule document — half is the
"do NOT report" list:

- Missing e2e coverage for a change with no external surface: a refactor, a
  type change, a log line, a comment.
- Missing e2e coverage for something the unit `tests` lane already owns:
  assertion quality, a branch a unit test should drive.
- The e2e suite's style, structure, or choice of framework.
- An e2e test that is thorough about something you would not have
  prioritised.
- A workflow that is `workflow_dispatch` only *when the repository's policy
  says its e2e suite is run manually before release* — the extracted rules
  and pinned knowledge are how that policy reaches the lane.

## Hands, later

Two things would make the second question sharper than "did the job go
green", and both are future work, not part of the first cut:

- **A report the hands upload.** A composite action `actions/e2e-report/`,
  on the `actions/ui-preview/` pattern, that posts the suite's JUnit output
  to `POST /e2e/reports` after the job. The brain then knows *which* tests
  ran, not just that a job did, and can match a candidate coverage line to a
  test that actually executed. Everything in that report is untrusted input,
  same as the preview manifest.
- **The UI preview as e2e evidence.** A preview flow that completed on the
  head is a feature that was exercised in a browser. `src/preview`'s manifest
  already records which flows ran and where they started; the e2e lane can
  read it as a second source of "was it verified" for UI-facing changes,
  with no new protocol.

Neither changes the boundary. The brain still never runs the code.

## Where each piece is tested

- `inventory.rs` — the path table across the common layouts, workflow
  classification by name and by step, `paths:` / `paths-ignore:` /
  dispatch-only triggers, label gates (and that a negated one is not a gate),
  explicit overrides replacing detection, comments and URLs not confusing
  the outline.
- `runs.rs` — a reported check run beating the filter, a filtered-out job as
  a High finding anchored on the `paths:` line, an expected-but-unreported
  job as pending, matrix legs, near-miss check names, label gates against
  the pull request's labels, and `settle` waiting for every job then
  concluding under `fail_on`.
- `evidence.rs` — surface tokens are literals and routes, not identifiers;
  candidates quote the line; `gather` reads the tree, the workflows, the
  checks and the specs through `MockForge`, and degrades on a missing tree.
- `mod.rs` — the golden test (a filtered-out workflow plus an uncovered
  route: exactly one `e2e-not-triggered` and one `e2e-uncovered` survive),
  pending leaving the lane `Neutral`, a passed job passing it, a failed job
  the model cannot remove, `e2e-unobservable` forced to Low, the
  documentation-only and no-harness skips, and the never-executes assertion
  over all four files.
- `app/apply.rs` — `settle_e2e` waiting, publishing once, clearing the
  watch, and leaving a moved head alone.
