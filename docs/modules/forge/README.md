# `forge`

Forge adapters and the domain types the [`ports::forge`](../ports/README.md)
traits speak in.

## The types are ours, not GitHub's

`forge::types` is hand-written rather than re-exported from octocrab. That is
what lets the offline mock be a first-class implementation instead of a stub,
and what keeps an HTTP client out of the default build. It also means a second
forge — a self-hosted GitHub Enterprise, or something else entirely — is an
adapter, not a rewrite.

`Issue` carries `age_days` and `quiet_days` as plain numbers rather than
timestamps. The age guards on issue closing are the most safety-critical
arithmetic in the codebase, and this shape makes them trivially testable without
a clock to fake.

## Reading state a merge decision could be taken on

`CheckStatus` and `ReviewVerdict` are the read-side counterparts of `CheckRun`
and `ReviewEvent`, and they are separate types because a check being *read* may
not have finished. `CheckStatus::conclusion` is an `Option`: `None` is "still
running", and `is_green()` is true only for `Success`. `Neutral` and `Skipped`
are deliberately not green — a check that declined to run has reported nothing,
and the conservative reading of nothing is "not yet".

`CheckStatus::from_api` and `ReviewEvent::from_api` live in `types.rs` rather
than in the adapter so the offline suite can test them. An unrecognised check
conclusion maps to `Failure` on purpose: a conclusion GitHub adds after this was
written must never be mistaken for a pass by a caller deciding whether to merge.

`ForgeRead::reviews` returns the history, oldest first, rather than a folded
verdict. Only the caller knows that a later `COMMENT` does not retire an earlier
`CHANGES_REQUESTED`, and folding in the adapter would bury that rule where no
offline test can reach it. `ReviewVerdict::bot` comes from the account type
GitHub reports, not from the shape of the login, because "how many humans
approved this" is a question a bot must not be able to answer for itself.

`check_runs` is keyed on a commit SHA rather than a pull request number: a check
that is green on the previous head says nothing about the commit about to be
merged. Neither method has a default implementation — an adapter that forgot to
answer would report "no checks", and a caller that cannot see a red check is
worse off than one that cannot see any.

## `MockForge` is not a stub

It backs the entire test suite *and* `--dry-run` in production. Because it
records writes rather than discarding them, a test can assert on the exact check
runs, comments and labels a run produced — which is how the noise-control rules
stay honest as the lanes change.

`read_only()` records the intent but does not apply it. That is the difference
between "what would you have posted" and "what did you post", and `--dry-run`
needs the first without any risk of the second.

## Files

| File | Role |
| --- | --- |
| `types.rs` | `PullRequest`, `ChangedFile`, `Commit`, `CheckRun`, `Issue`, … |
| `mock.rs` | The recording in-memory forge |

`Commit::patch` is `None` until something fetches it. Both adapters behave the
same way on purpose: the listing endpoint returns metadata, and the patch comes
from `commit_patch`. `MockForge` withholds a stored patch from `commits()` for
exactly that reason — a mock that handed it over would pass tests on behaviour
GitHub does not have, and leave the `pull_request_context` plumbing untested.

The GitHub adapter assembles a commit's patch from the commit endpoint's
per-file `patch` fields rather than requesting `application/vnd.github.diff`,
because the JSON shape lets each file be capped on the way in. A commit that
vendors a directory must not be held in memory in full before anything gets the
chance to shorten it.

The octocrab-backed `github.rs` arrives with M6, behind the `github` feature.

## Absence has to mean absence

Three fields on the read path used to answer with a value where the honest
answer was "unknown", and each one degraded silently rather than loudly.

`ChangedFile::size_bytes` was hard-coded `None`, so `scan::blobs` could never
raise `large-blob` against the real adapter no matter how `max_blob_bytes` was
set. It now comes from one recursive git-tree request at the head revision —
one round trip for the whole revision rather than a blob request per file,
because the file that most needs a size is the one whose blob is most expensive
to serve. When the tree is truncated or unreadable the field stays `None`, which
`scan::blobs` reads as unknown. A zero would have read as *safely small*.

`ReviewComment::line` mapped GitHub's `null` to the literal `0`, which is
indistinguishable from a real line zero and sorts first in anything ordered by
line. It is now `Option<u64>`: `None` on the read path for a comment GitHub does
not attach to a line, and `Some` on the write path, which `apply` only ever
builds for a finding that resolved to one.

`PullRequest::approvals` was hard-coded `0`. It is now each reviewer's *latest*
verdict, counted once — a plain tally of `APPROVED` keeps counting a reviewer
who later requested changes, and double-counts one who approved twice. A
`COMMENTED` review does not overwrite a standing approval, because GitHub lets a
reviewer comment without withdrawing one.

## A missing patch is not an empty change

`patch` is absent for two unrelated reasons and only one of them is innocent. A
binary file has no textual diff to give, and GitHub reports it as zero lines
added and zero removed. A *truncated* patch is the forge saying lines changed
while declining to say which.

`ChangedFile::evidence_missing()` separates them: no patch, but changed lines.
`is_opaque()` still answers the weaker question and must not be used as proof of
binary content.

This mattered because the second case used to read as "no reviewable content".
The lane skipped the file, the blob scanner only inspects added files, and
the review reported success over a change nobody had seen. The paths now reach
`Proposal::unreviewed`, every lane's summary names them, and `apply` refuses to
approve while any remain. Deliberately not a *blocking* verdict: we do not know
there is a problem, only that we did not look, and blocking a merge on our own
blind spot punishes the contributor for the forge's truncation.
