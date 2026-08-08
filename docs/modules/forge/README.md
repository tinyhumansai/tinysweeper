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

The octocrab-backed `github.rs` arrives with M6, behind the `github` feature.
