# Contributing

## Local Checks

```sh
git submodule update --init --recursive
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
cargo check --locked --all-features --all-targets
```

The default build must stay offline — no HTTP client linked, no network in the
test suite. Put anything that needs the network behind a feature.

## Pull Requests

Keep changes small and focused. Say what changed, what behaviour changed, and
how you verified it. New behaviour needs a test; new noise-control behaviour
needs a golden test. Read `AGENTS.md` first — in particular the security
boundary, which is not negotiable without discussion.
