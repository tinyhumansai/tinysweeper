## GitHub Actions workflows

The deterministic scanner has already checked permission blocks, unpinned
actions and `pull_request_target` checkout shapes. Adjudicate what it found;
look only for what it cannot see.

### Report

- A job that both runs on `pull_request_target` (or a comparable
  fork-writable trigger) **and** reaches contributor-controlled content: a
  checkout of the head ref, a script built from the PR title or body, an
  artifact downloaded from the triggering run.
- A secret made available to a job that executes contributor code — a build
  step, a test run, a linter installed from the pull request's own lockfile.
  Name the secret by its variable, never its value.
- An expression interpolating attacker-controlled text straight into `run:`.
  `${{ github.event.pull_request.title }}` inside a shell line is a shell
  injection with extra steps.
- A permission widened beyond what the job's steps use, when the scanner did not
  already flag it — for instance `contents: write` on a job that only reads.
- A newly added third-party action from an account with no release history,
  where the workflow can reach a secret or a write token.

### Do NOT report

- An unpinned `actions/checkout@v4` or another first-party `actions/*` action.
  Pinning those to a SHA is a defensible policy and a preference, not a finding.
- `permissions:` blocks that are already minimal, or a job whose permissions the
  scanner has confirmed match its steps.
- A workflow triggered only by `push`, `workflow_dispatch`, `schedule`, or
  `pull_request` (as opposed to `pull_request_target`) reading contributor code.
  That is the safe trigger, and it is what it is for.
- Matrix size, runner choice, caching strategy, step ordering, or job names.
- A secret used by a job that runs no contributor code at all — a deploy job on
  the default branch, a release job gated on a tag.
- Repeating a finding the scanner already reported. Say whether it is real; do
  not restate it as your own.
