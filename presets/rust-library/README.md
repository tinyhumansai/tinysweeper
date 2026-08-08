# `rust-library`

For Rust crates published to crates.io or consumed as a path/git dependency,
where the public API is the product.

## What it assumes

- The repository builds with `cargo`, and `Cargo.lock` is either absent or not
  worth reviewing.
- Network dependencies live behind Cargo features, so the default build stays
  offline. A new non-optional HTTP dependency is a finding, not a detail.
- Errors are returned, not panicked. `unwrap()` outside tests gets flagged.
- Ports, if the crate has them, are one trait per file with an offline mock.

## What it turns on

All five lanes at default strictness. `security` runs on the deep model and
fails the check on any confirmed high finding; `tests` runs on the cheap model,
because judging whether an assertion is vacuous does not need the expensive one.

## When not to use it

If the crate is an application rather than a library, the API-stability
emphasis will produce findings nobody acts on. Start from the defaults instead
and add `path_instructions` as you learn what you actually want flagged.
