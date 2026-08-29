# Repository Guidelines

## Project Structure & Module Organization

- `src/` — the single crate. One responsibility per module, core types in a
  module-local `types.rs`, and every port in `src/ports/` is one trait in one
  file.
- `src/bin/tinysweeper.rs` — the CLI. Every subcommand is declared even when its
  milestone has not landed, so scripts and runbooks can be written against a
  stable surface. `src/server/` is the only production surface: tinysweeper is
  not distributed or run as a GitHub Action. The two deliberate exceptions are
  operator buttons in *this* repository: `.github/workflows/manual-review.yml`,
  which POSTs to the deployed server's `/admin/reviews` route, and
  `.github/workflows/deploy.yml`, which restarts the cluster workload so it
  re-pulls the published image. Neither builds anything, runs a lane, or holds a
  model or GitHub write credential; anything that would need one belongs in
  `src/server/`, not in a workflow.
- `presets/` — review policy as **data**, not code. A preset is a folder with a
  `preset.toml`, a `README.md`, and optional prompt overrides. Adding a preset
  is a new folder, never a new module.
- `vendor/tinyagents` — the agent harness, as a git submodule. Never edit it
  here; change it upstream and bump the pin.
- `vendor/tinyflows` — the orchestration graph, as a git submodule. Same rule.
  Lane concurrency, the consensus panel and sub-agents are all expressed as
  graphs against it; see `docs/modules/flows/README.md`.
- `docs/modules/<module>/README.md` — one document per `src/` module.
- `examples/` — declared explicitly in `Cargo.toml` with `required-features`, so
  credential-needing smoke tests never build in CI.

## Build, Test, and Development Commands

```sh
git submodule update --init --recursive
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
cargo check --locked --all-features --all-targets
```

The default build is offline: it links no HTTP client and the test suite never
touches the network. Anything that needs the network goes behind a feature
(`harness`, `github`, `serve`).

## Coding Style & Naming Conventions

- rustfmt output, Rust 2024 idioms. `snake_case` modules and files,
  `PascalCase` types.
- Return `Result<T>` using the crate error type from `src/error.rs`.
- Every file opens with a `//!` module doc describing its role and any feature
  gating.
- Comments explain the *decision*, not the code. When a flag or an ordering is
  load-bearing, say so and say why.
- Public items carry doc comments. Clap fields use `///` — it becomes the help
  text.

## Testing Guidelines

- Tests live **in-crate**: a `#[cfg(test)] mod tests` block at the bottom of the
  module, moving to a sibling `test.rs` or `<name>_test.rs` when they grow.
  There is no `tests/` directory.
- Every port has an always-compiled offline mock. `MockForge` records what would
  have been written so tests can assert on exact check runs and comments.
- Lane behaviour is covered by golden tests: fixture diff plus a canned
  structured model response, asserting the findings that survive filtering,
  dedupe and capping. These are the tests that keep noise control honest.
- Maintain at least 80% coverage for meaningful library behaviour.

## Documentation Expectations

Keep every Markdown file, including this one, at 500 lines or fewer. When a
topic grows past that limit, split it into focused files and link them from the
module's `README.md`.

## Security Boundary

These are invariants, not preferences. Changing any of them needs an explicit
discussion in the pull request:

- The model never holds a write token. Lanes run against a read-only checkout;
  write credentials are minted only in `src/apply/`, after every model call has
  returned. A lane that leaves a change in the checkout fails.
- Contributor code is never executed. We read the diff and the tree; we do not
  build, install dependencies, or run the target repository's scripts.
- Pull request bodies, comments and diffs are untrusted input. Fence and label
  them as data in prompts. A model verdict is advisory — GitHub is only ever
  mutated by deterministic policy, and only from a module whose whole job is
  that write: `src/app/apply.rs`, `src/automerge/`, `src/issues/apply.rs`,
  `src/pr_triage/apply.rs`, `src/threads/` and `src/sentry/promote.rs`. Adding
  to that list is a decision to argue for in a pull request, and the bar is the
  same each time: the module holds a `ForgeWrite` and *only* executes a plan
  some other module already decided on.
- Secrets found by the scanners are reported by type and location only. The
  value never reaches a comment, a check-run summary, or a log.

## Commit & Pull Request Guidelines

Use concise, imperative commit subjects. Keep commits small; commit each
coherent, validated slice on its own. Pull requests state what changed, any
behaviour change, and how it was verified.
