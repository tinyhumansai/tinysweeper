# Presets

A preset is a named review policy, stored as **data**. Adding one is a new
folder, never a new module.

```
presets/<name>/
  preset.toml   # the policy — same schema as .tinysweeper.toml
  README.md     # what this preset is for and who should use it
  prompts/      # optional per-lane prompt overrides
```

A repository opts in from its own config:

```toml
preset = "rust-library"

[review]
strictness = 3   # local settings win over the preset
```

Merge order, later wins: built-in defaults → the named preset → the
repository's `.tinysweeper.toml`. `tinysweeper doctor` reports which layer set
each effective value.

## Available presets

| Preset | For |
| --- | --- |
| [`rust-library`](rust-library/) | Rust crates: API stability, error handling, feature-gate hygiene |
| [`security-strict`](security-strict/) | Repositories where a missed vulnerability costs more than a false positive |
| [`e2e-required`](e2e-required/) | Repositories with an end-to-end suite worth holding changes to: a missing harness is a finding, not a skip |
| [`polyglot`](polyglot/) | Repositories mixing languages: each file gets the rules written for its own |

## Adding one

Start from the closest existing preset, keep it small, and write down in its
`README.md` what it assumes about the repository. A preset that encodes rules
nobody has seen fire on a real pull request does not belong here yet.
