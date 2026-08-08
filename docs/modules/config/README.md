# `config`

Discovery, layered merge, and validation of `.tinysweeper.toml`.

## Layers

Three, later winning:

1. `src/config/defaults.toml`, compiled into the binary with `include_str!`
2. the named preset, `presets/<name>/preset.toml`
3. the repository's own `.tinysweeper.toml`

A fourth layer, `Layer::Flag`, exists for command-line overrides.

## Why the merge happens at the TOML level

Merging `toml::Table`s rather than structs is what makes per-key provenance
possible: `merge::merge_layer` records the layer for every leaf it sets, and
`tinysweeper doctor` reads its explanation out of that same record. A
hand-maintained "where did this come from" table would drift; this one cannot.

Tables merge recursively so a preset can set one field of a section without
erasing the rest. **Arrays replace wholesale** — appending would make an
inherited entry impossible to remove, and silent accumulation is worse than
re-listing.

## Why validation collects everything

`validate::validate` returns a `Vec<String>`, never an early `Err`. Someone
fixing a config should need one round trip, not six, and in CI each round trip
is a push and a wait. Messages name the key, say what is wrong, and say what
would be right.

Validation also flags settings that are *valid but inert* — a `lanes.critique.
max_blob_bytes` that only the `commits` lane reads, an `issues.close.enabled`
under an `issues.enabled = false`. A silently ignored setting reads as working,
which is the worst failure mode a config file has.

## Discovery

`.tinysweeper.toml` at the repository root, then `.github/tinysweeper.toml`.
Finding nothing is not an error: running on defaults is a supported mode, and
`check` says so.

Preset names may not contain a path separator, so `preset = "../../etc"` is
rejected rather than resolved.

## Files

| File | Role |
| --- | --- |
| `defaults.toml` | The bottom layer, compiled in |
| `types.rs` | Every config type; all `#[serde(default, deny_unknown_fields)]` |
| `merge.rs` | Table merge and `Provenance` |
| `validate.rs` | Every check, collected |
| `mod.rs` | Discovery and the loader |
| `test.rs` | Tests, split out because they span three modules |
