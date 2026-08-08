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

## Files

| File | Role |
| --- | --- |
| `doctor.rs` | `check` and `doctor` |
