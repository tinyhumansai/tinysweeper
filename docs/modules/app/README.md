# `app`

The work behind each CLI subcommand, kept out of `src/bin/tinysweeper.rs` so it
is testable. The binary parses arguments and nothing else.

## `check`

Answers "is this config usable". Prints every problem at once and exits non-zero
when there is one, so it is usable as a CI step.

Finding no config file is a pass, not a failure — running on built-in defaults
is a supported mode, and `check` says which mode you are in.

## `doctor`

Answers the harder question: "what is actually going to happen". It renders the
effective values, which layer set each one, which model each lane will call, and
which credentials are present.

Both commands read from the same merge, so neither can describe a configuration
the other would not produce.

The `overridden` section is the point of the command. Every value has a default;
the ones worth a human's attention are the ones a preset or the repository moved
off it.

### Credentials

Only *presence* is reported, never a value, and the list is derived from the
config — `SENTRY_AUTH_TOKEN` appears only when Sentry promotion is enabled, so
the report never nags about a credential this repository does not need.

## `apply` — the verdict ladder

`apply` publishes a proposal without changing it. The one decision it does make
is which of GitHub's three review verdicts to submit, and it is taken in this
order:

1. **Request changes** — a lane failed the gate *and* a finding reached
   `review.request_changes_at`. Both are required: `fail_on` and
   `request_changes_at` are independent knobs, so a lane may fail its check
   without blocking the merge.
2. **Approve** — every lane passed, the review was **complete**, the pull
   request is **not a draft**, and `review.approve_when_clean` is on. This is what lets a pull request satisfy a
   "review required" rule, and it is also what retires an earlier objection:
   GitHub keeps one review per reviewer, so the approval supersedes it and
   nobody has to dismiss anything by hand.
3. **Approve** — every lane passed and tinysweeper was blocking before, even
   with `approve_when_clean` off. A block it will not clear is a block that
   needs a human.
4. **Comment** — everything else.

**Complete** means `Proposal::unreviewed` is empty — no file changed that the
forge declined to show us. This is the job the aggregate `tinysweeper/gate`
check run used to do by degrading itself to `Neutral`. That check is gone, and
the approval is the whole verdict now, so it has to carry what the check
carried: a file nobody saw is not a file anyone can vouch for. Nothing blocks,
so there is nothing to object to — and nothing to endorse either, which is a
`Comment`.

**Not a draft** closes a trap. With `review.draft_prs = false` every lane skips
a draft, so the proposal comes back with nothing blocking and nothing
unreviewed — which reads as "clean" to both conditions above. Without this the
bot would endorse a pull request it had deliberately declined to look at, and on
a repository requiring a review that endorsement is what lets it merge. Note
rung 3 is *not* gated on it: refusing to endorse a draft is not the same as
refusing to unblock one, and conflating them would strand every draft that was
ever blocked.

Four bounds on approving, all deliberate. It reads the *gate*, not the blocking
threshold, so `request_changes_at = "off"` stops tinysweeper objecting without
starting it endorsing a red pull request. And an approval that already stands is
not restated, or every push to a clean pull request would add a review that
changes nothing.

## `local-review`

The same engine over a local git range: same scanners, same lanes, same
filtering, dedupe and capping, no GitHub token and no forge call. It exists so a
prompt change can be validated without opening a pull request, paying for a real
review and leaving comments on somebody's branch.

The evidence comes from `src/evidence/git.rs`, which resolves the range and
hands `src/app/local.rs` the same `ChangedFile` and `Commit` values the GitHub
adapter produces. A `MockForge` serves them, so no lane can tell the difference
— which is the property that makes this worth iterating against.

Four decisions are worth knowing before reading its output:

- **The base is the merge base.** `git diff base head` also shows whatever
  landed on the base branch meanwhile, so a lane would review code this author
  never wrote and every rule in `lanes::anchor` would be arguing about somebody
  else's lines. GitHub shows `base...head`; so does this.
- **No head means the working tree**, uncommitted and *untracked* files
  included. A file you have written but not yet `git add`ed is the most likely
  thing to be under review, and `git diff` cannot see it — so it is collected
  separately, through `git diff --no-index`, which does not touch the index. A
  read-only command leaves the index exactly as it found it.
- **It is always a first review.** There is no forge to read prior findings back
  off and no durable store, so `review.incremental` has nothing to work with.
  The cache-hit figure in its output must not be compared against what a real
  review posts.
- **The description is synthesised.** A git range has no title and no body, and
  that is the `description` lane's entire subject. The title defaults to the
  newest commit's subject and the body to empty; `--title` and `--body` exist so
  the lane can be exercised against something real rather than grading an
  invention.

## Files

| File | Role |
| --- | --- |
| `doctor.rs` | `check` and `doctor` |
| `review.rs` | the read half — runs the lanes, writes a proposal |
| `local.rs` | `local-review` — the same read half over a git range |
| `apply.rs` | the write half — publishes a proposal, decides the verdict |
