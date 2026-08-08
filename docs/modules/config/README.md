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

## Model tiering is config data, not code

Two tiers — `models.scan` and `models.deep` — and two resolvers over them.
`Config::model_for(lane)` answers for a reviewing lane, honouring a per-lane
override that may name a tier or an explicit model id; lanes with no override
get the cheap tier, because the expensive one should be opt-in.
`Config::model_for_workload(Workload)` answers for the mechanical work that is
not a lane — snippet re-location, the falsification pass, knowledge extraction —
and always answers with the cheap tier. That `match` is exhaustive so adding a
workload forces someone to state its tier rather than inherit one.

Repositories cannot override a workload the way they override a lane. Paying
deep-tier prices to copy a snippet out of a diff is not a trade-off worth
exposing. (open-code-review runs one model for everything, which is why its
cheap operations cost the same as its expensive ones.)

tinyagents ships `ModelRouter`/`WorkloadRoute` for the same idea and it was
considered. It resolves aliases inside a tinyagents harness, with capability
gates and a `FallbackPolicy`; tinysweeper picks its tier before the `Model`
port, in the default build, where tinyagents is not linked at all. The place it
would pay for itself is `GatewayModel`'s hand-rolled fallback chain, on the
feature-gated side of the port.

## `[embeddings]` is a partition key, not a call setting

`provider`, `model` and `dimensions` are not three more fields like
`[models]`'s: together they *are* `EmbedSignature`, the key every indexed vector
is written under and every query filters on. Editing one does not reconfigure a
call, it invalidates the index. Keeping them in their own block is what makes
that legible to whoever edits it, and it is why the embedding provider is
configured here and **nowhere else** — a second place to set it (an environment
variable, say) is a second way for the key to disagree with the vectors already
written, and that disagreement is silent rather than an error.

The section is off by default. An embedding provider costs money per indexed
byte and needs a key nobody has by accident, so a deployment that has not said
otherwise runs diff-only, exactly as tinysweeper did before an index existed.
Only the API key lives in the environment, under the name `api_key_env` gives.

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
