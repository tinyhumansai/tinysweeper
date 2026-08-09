# `src/automerge/`

The deterministic gate that may put code on the default branch without a human.

This is one of only two modules permitted to mutate GitHub, and the only one
that can do so with nobody watching. Everything below follows from that.

## The rule

**Only deterministic policy may mutate GitHub. A model verdict is advisory.**

No model is asked whether a pull request looks safe to merge. That question gets
answered plausibly, unaccountably, and differently on every run. Every criterion
here is arithmetic over observable state: check conclusions, review states, file
counts, path matches, line counts, SHAs. Same inputs, same answer, every time —
which is also what makes every refusal testable offline against `MockForge`.

"Complexity" in particular is measured, not judged. See
[the signals](#complexity-measured-not-judged).

## Failing closed

A wrongly-merged pull request cannot be un-merged. One left alone merely waits
for a person. Every ambiguity resolves towards waiting:

| Ambiguity | Answer |
| --- | --- |
| `mergeable: None` — GitHub still computing | not mergeable |
| A required check that never reported | not passed |
| A required check that was **skipped** | not passed — it could not run |
| A check still running | not passed |
| An unrecognised conclusion | read as a failure |
| A glob in the policy that will not compile | refuse, and say which |
| A file the forge gave no patch for | unmeasurable, refuse |
| `max_files = 0` | refuses everything; a zero cap is not "unlimited" |

## The criteria, in order

Evaluated by `policy::evaluate`, which is pure — no I/O, no clock, no model.
The order is cheapest-and-most-decisive first, so the reason an operator is
shown is the most useful one.

1. `automerge.enabled` — off unless the repository opts in.
2. Not a draft.
3. No label in `block_labels`.
4. A label in `allow_labels`, when that list is non-empty.
5. `mergeable == Some(true)`.
6. No standing changes-request from **anyone**, bots included. The question is
   whether a concern stands unaddressed, not who raised it.
7. At least `require_approvals` *human* approvals, counted from the review
   history rather than GitHub's tally — a bot approving is not a second opinion.
8. The checks on the head SHA, asked two different questions:
   - **Nothing anywhere is red or still running.** Every check, not only the
     required ones — a repository's own CI does not have to be listed for
     tinysweeper to respect it. `action_required` counts as red, and so does a
     conclusion GitHub introduces after this was written, because
     `CheckStatus::from_api` maps anything it does not recognise to `Failure`.
   - **Every name in `require_checks` produced a verdict.** Missing refuses —
     a required check that never ran is not a check that passed, and that is
     the case that would otherwise let a deleted workflow silently retire the
     whole gate. `Skipped` refuses too, with a reason of its own: the check
     could not run, so the gate it was named for has nothing behind it.

   `Neutral` and `Skipped` sit between the two questions, and keeping them
   apart is load-bearing. This used to read "not green blocks", which is not
   the conservative choice it looks like: a workflow job behind
   `if: github.event_name == 'push'` concludes `skipped` on *every* pull
   request, for ever, so nothing in such a repository could ever merge. A gate
   that never opens is not safe — it teaches operators to widen it until it
   does. `Neutral` from a *required* check is accepted, and only there: it is a
   lane reporting that it did not apply, which is an answer, not an absence.
9. At least one changed file. Nothing measured is no gate at all.
10. `files <= max_files`.
11. No path matching `sensitive_paths` — unless this is a dependency bump.
12. Every complexity signal under its threshold — unless this is a dependency
    bump.

Then, immediately before merging, the whole policy is re-evaluated against
freshly read state and the head SHA is compared. `src/app/apply.rs` does the
same thing for the same reason: the decision was reached against one commit,
and a push may have replaced it since. Checks that went green on a commit
nobody is looking at any more say nothing about the one about to land.

## Complexity, measured not judged

Four signals, all arithmetic over the file list the forge returned:

| Signal | Default | Why this one |
| --- | --- | --- |
| `max_files` | 20 | The cheapest proxy for "a human should look". A change spread over thirty files is a refactor whatever its line count. |
| `max_changed_lines` | 400 | Additions plus deletions, across every file. Volume of change. |
| `max_hunks` | 30 | Twenty scattered one-line edits are harder to be sure about than one new two-hundred-line file. |
| `max_directories` | 5 | Blast radius across the tree. A change reaching eight directories has crossed module boundaries. |

Hunks are counted off `@@` at the start of a line in the unified patch. A `@@`
written inside the code being changed carries the diff's own space, `+` or `-`
prefix, so it does not start the line and is not counted — which is what stops
a contributor inflating or deflating their own hunk count.

A file with no patch and no line movement is a rename or a mode change:
measurable at zero hunks, because there is genuinely nothing there. A file with
no patch **and** real line movement is unmeasurable, and unmeasurable refuses —
counting it as zero would let the largest changes through the tightest cap.

## Sensitive paths

`sensitive_paths` names everything that changes *what runs* or *who may run it*:
CI workflows, container and deploy configuration, Terraform, Kubernetes and
Helm, migrations, anything under an `auth/` directory, `CODEOWNERS`, the
instruction files that are the security boundary itself, and every dependency
manifest and lockfile. Globs, matched against both the current and the previous
path so a rename cannot smuggle a file out of the list.

## The dependency-bump exemption

Manifests and lockfiles are sensitive by definition and a lockfile churns
thousands of lines, so without an exemption no Dependabot pull request would
ever qualify. The exemption is deliberately narrow — three conditions, all
required:

1. `allow_dependency_bumps = true`.
2. The author is one of `dependency_bots` by **exact** login match, *and*
   GitHub itself reports the account as `type: "Bot"`.
3. Every changed path matches `dependency_paths`.

The login match is exact because anyone can register `dependabot-evil`; this
repository already learned that in `findings::prior::is_own_login`, where a
`starts_with` would have counted `tinysweeper-anything` as itself. A `[bot]`
suffix is presentation and is normalised off both sides, and nothing else is.
The bot flag alone would not do either — any GitHub App can open a pull request.

The exemption waives the sensitive-path and complexity refusals, whose purpose
the manifest-only rule already serves more strictly. It waives **nothing** else:
checks, reviews, labels, `max_files` and the live head SHA all still apply. One
source file in the same pull request and it is an ordinary change again.

## The merge method

Read from `automerge.method`, never hardcoded, because repositories disable
merge methods — squash is disabled on this one. The forge refusing the method is
reported as `Outcome::Rejected` rather than raised as an error: the pull request
is exactly where it was, which is the safe state, and the next run tries again.

## Testing

`src/automerge/test.rs`, weighted deliberately towards the refusals. Merging is
one path and a wrong one cannot be undone, so every reason to stop has its own
test, and each asserts that `MockForge` recorded no merge at all.

## Files

| File | Role |
| --- | --- |
| `mod.rs` | The job: read, decide, re-read, decide again, merge. |
| `policy.rs` | `evaluate` — pure, the whole decision. |
| `complexity.rs` | The measured signals. |
| `paths.rs` | Glob compilation and exact login comparison. Both fail closed. |
| `types.rs` | `Decision`, `Refusal`, `Outcome`. |
