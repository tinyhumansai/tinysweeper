# Roadmap

Engineering milestones. Each one is a shippable slice with its own verification.

## Now

- **M0 — scaffold.** Crate, toolchain, licence, community files, CI, templates,
  Docker, `vendor/tinyagents`. ✅
- **M1 — config and ports.** `config/` (discovery, preset merge, validation that
  reports every problem at once), `ports/` one trait per file, `forge/mock.rs`,
  and a working `tinysweeper check` / `tinysweeper doctor`.

## Next

- **M2 — evidence and scanners.** Git range resolution, diff hunks with a line
  map, and the deterministic prescan: secrets, large blobs, workflow permission
  widening, dependency changes. No model calls.
- **M3 — the harness.** OpenAI-compatible model provider over tinyagents, the
  read-only tool belt (`read_file`, `grep`, `list_dir`, `git_log`, `git_show`,
  `blame`), structured-output schemas, and the `critique` lane.
- **M4 — the remaining lanes.** `security`, `tests`, `commits`, `description`,
  and the deterministic `gate`.
- **M5 — publishing.** Finding filter, fingerprint dedupe, the single durable
  comment with its history ledger, check runs, inline comments, and the
  propose/apply split with revalidation. `--dry-run` renders it all without
  writing.
- **M6 — the real forge.** octocrab adapter, incremental review, the three-stage
  cache with metrics and leases, and the `@tinysweeper` command router.
- **M7 — delivery.** The GitHub App: the webhook server, its store, the admin
  API, and the Docker image. The composite action and reusable workflow that
  this milestone originally called for were built and then removed — see
  "Removed" below.
- **M8 — auto-merge.** Gate policy, label gating, and the merge path.

## Later

- **M9 — the rest of the scope.** Issue triage, the automator, and Sentry issues
  promoted into GitHub issues.

## Removed

- **The GitHub Actions distribution path.** The composite action, the reusable
  `review.yml` every other repository was told to call, and this repository's
  own `tinysweeper.yml`. All deleted; the server is the only surface.

  The reusable workflow downloaded a release binary from a `v1` tag that was
  never created and that nothing produced, so it was broken for every
  repository except this one — where `tinysweeper.yml` deliberately built from
  source and hid the gap (issue #20). The choice was to build a release
  pipeline for a path we no longer want, or to delete the path. We deleted it.

  The earlier argument for Actions-only was that Actions receives every event a
  server would. True, and not the deciding factor: a contributor whitelist is a
  fact about a *person over time* and a stateless job has nowhere to keep one,
  43 repositories' worth of runner minutes is real money for work that is
  mostly waiting on a model, and replying to a comment within seconds needs an
  event loop rather than a cold runner.

  The security boundary the two-job workflow enforced is unchanged and is now
  enforced by the type system instead: lanes take a `ForgeRead`, only
  `src/apply` takes a `ForgeWrite`, and the installation token used for writing
  is minted in `src/server/routes.rs` only after `app::review` has returned.

## Not planned

- A hosted public service. tinysweeper is meant to run in your own org, on your
  own key.
- Executing contributor code. See the security boundary in `AGENTS.md`.
