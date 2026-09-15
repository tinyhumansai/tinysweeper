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

## Commit patches are a second call

`ForgeRead::commits` returns metadata; `ForgeRead::commit_patch` returns one
commit's patch. Every forge worth supporting bills them as separate requests, so
the port says so rather than pretending otherwise. `pull_request_context` makes
the second call for the first `MAX_PATCHED_COMMITS` commits and leaves the rest
with `patch: None` — one request per commit means a two-hundred-commit branch
would otherwise spend a review's whole rate-limit budget on patches nothing
renders. `None` means "not fetched", never "changed nothing"; the `commits` lane
depends on being able to tell the difference.

## `Model`

Lanes never talk to a provider SDK. They describe what they want — messages, a
JSON schema the answer must satisfy, a token ceiling — and get back a parsed
`serde_json::Value` plus usage accounting.

Structured output is not optional. A lane that parses prose is a lane that
silently misbehaves when a model phrases something differently, and that failure
is invisible until it posts something wrong on someone's pull request.

`Usage` carries `cached_tokens` separately because prompt-cache hit rate is the
difference between a cheap re-review and a ruinous one, and `embed_tokens`
separately again — embeddings are priced on their own scale, and folding them
into the prompt total would make that hit rate quietly wrong.

`Spend` pairs a `Usage` with the models that produced it. The pairing is the
point: reporting usage without the model that answered is how the cost line came
to name whichever model the config *asked* for, while a fallback did the work.
The model id is recorded where the response arrives and never re-derived from
config afterwards. Lanes report a `Spend`, and `LaneProposal` carries it through
to the check-run summary so the breakdown says which lane cost what.

Prices live in `src/harness/pricing.rs`, always compiled so budget enforcement is
tested offline. A model with no price is billed at the most expensive rate in the
table rather than at zero: the previous behaviour made an unpriced model free and
therefore exempt from `models.budget_usd_per_pr`, which is the one model whose
cost nobody had checked.

## Files

| File | Port |
| --- | --- |
| `forge.rs` | `ForgeRead`, `ForgeWrite` |
| `model.rs` | `Model`, plus `Message`, `ModelRequest`, `ModelResponse`, `Usage` |
| `embed.rs` | `Embedder` — returns `Embedded { vectors, usage }`, never bare vectors |
| `index.rs` | `ChunkIndex` |
| `graph.rs` | `GraphStore` |
| `knowledge.rs` | `KnowledgeStore` |
| `tree.rs` | `TreeReader` — read a file range, search a literal; `MockTree`, `DirTree`, `RecordingTree`, `ChainTree` |

## The retrieval ports

Four traits, one adapter module. Their value types live in `src/index/types.rs`
and the reasoning behind their shape — why `Embedder` must be able to name
itself, why `ChunkIndex` needs deletes, and what MongoDB has to be — is in
[`docs/modules/index/README.md`](../index/README.md).

## `TreeReader`

What a reviewer may look up before it answers: a range of one file at the
reviewed commit, or every line containing a literal. Two verbs, both reads,
so the port cannot be argued into running anything. `DirTree` serves a
checkout and searches in-process; `forge::tree::ForgeTree` serves the
forge API one file at a time, following one level of submodule through
`ForgeRead::submodule_at`, and answers *unavailable* for search. See
[`docs/modules/lanes/lookup.md`](../lanes/lookup.md).

## `Memory`

One trait, one adapter, four sections. What the reviewer remembers about a
repository *between* pull requests: its code, the conventions its own guides
state, what the maintainers did with the reviewer's earlier findings, and what
was said on its issues and pull requests by anyone — humans and other agents
alike, except the reviewer's own remarks, which are excluded. The last is fed
through two read-only `ForgeRead` methods added for it,
`issues_updated_since` (the whole history, closed items and pull requests
included) and `remarks` (one conversation as a single timeline). The
index answers what the code looks like and the graph what a change reaches;
neither learns. This one accumulates, and it is consulted by question
(`answer`, a cited synthesis) as well as by query (`recall`). The reasoning,
the section split and the CortexDB adapter are in
[`docs/modules/memory/README.md`](../memory/README.md).

Every call in the review path is best-effort at the call site: the port returns
errors so the adapter can be honest, and `memory::recall` turns them into a
status the check-run summary states.
