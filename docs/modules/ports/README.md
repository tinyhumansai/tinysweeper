# `ports`

The kernel's dependency-inverted seams. One port is one trait in one file.

Every port has an always-compiled offline implementation, so the default build
links no HTTP client and `cargo test` never touches the network. Real adapters
live in sibling modules behind Cargo features.

## The read/write split is a security boundary

`ForgeRead` and `ForgeWrite` are two traits, not one, and that is load-bearing.

Lanes take a `ForgeRead`. Only `src/apply` takes a `ForgeWrite`. A lane
therefore *cannot* mutate a pull request even by mistake, because it never holds
a handle that could. The invariant in `AGENTS.md` — "the model never holds a
write token" — is enforced by the type system rather than by discipline, so it
survives contributors who have not read `AGENTS.md`.

If you find yourself wanting to pass a `ForgeWrite` into a lane, the answer is
that the lane should return a proposal and the apply path should act on it.

## `Model`

Lanes never talk to a provider SDK. They describe what they want — messages, a
JSON schema the answer must satisfy, a token ceiling — and get back a parsed
`serde_json::Value` plus usage accounting.

Structured output is not optional. A lane that parses prose is a lane that
silently misbehaves when a model phrases something differently, and that failure
is invisible until it posts something wrong on someone's pull request.

`Usage` carries `cached_tokens` separately because prompt-cache hit rate is the
difference between a cheap re-review and a ruinous one.

## Files

| File | Port |
| --- | --- |
| `forge.rs` | `ForgeRead`, `ForgeWrite` |
| `model.rs` | `Model`, plus `Message`, `ModelRequest`, `ModelResponse`, `Usage` |
