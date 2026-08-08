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
- **M7 — delivery.** The composite action, example workflows, Docker image, and
  the release and e2e workflows.
- **M8 — auto-merge.** Gate policy, label gating, and the merge path.

## Later

- **M9 — the rest of the scope.** Issue triage, the automator, and Sentry issues
  promoted into GitHub issues.

## Not planned

- **A webhook server.** Decided against, deliberately. Actions already receives
  every repository event a server would have subscribed to, and the two things
  people usually buy a server for — reacting to a resolved review thread or a 👎
  reaction — have no webhook event at all, so a server could not do them either.
  Repositories opt in through the reusable workflow in
  `.github/workflows/review.yml`; rolling out to another repository is one file,
  and the review logic stays in one place. See `docs/triggers.md`.
- A hosted public service. tinysweeper is meant to run in your own org, on your
  own key.
- Executing contributor code. See the security boundary in `AGENTS.md`.
