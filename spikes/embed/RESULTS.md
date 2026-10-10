# Phase 0 result: no-go for production migration at this pin

Inspected and tested on 2026-10-10 against OpenHuman
`a1075723249eb6965acca95f3ca3a694e056c9e8` and tinysweeper baseline
`2afdd90d1ced1ea8433d4f6f825ac148c6b783c7`.

The basic integration is viable: a `Completer` can implement the current
`Model` port, and a `HostOnly` agent can inspect a checkout without exposing
built-in tools. The migration cannot proceed on this evidence alone.
Provider-independent agent exploration, strict schema enforcement and the
remaining upstream gates are not established.

## Real-key probes

All calls used OpenRouter's OpenAI-compatible endpoint. Moonshot and MiniMax
were tested as routed model families, **not** against their direct endpoints;
matching direct-provider keys were not available in the session environment.
The checkout was a host-created source fixture, not a live GitHub PR. No
GitHub writes occurred during these probes.

| Requested and answering model | Existing description lane | HostOnly agent | Vision |
| --- | --- | --- | --- |
| `openai/gpt-4.1-mini` | Answered; usage and charged cost recorded | Read the fixture once and returned a structured finding; model and usage recorded | Passed with inline PNG, structured caption, model and cost |
| `moonshotai/kimi-k2.5` | Answered; usage and charged cost recorded | Returned structured JSON, but made **zero** repository tool calls; probe exits unsuccessfully | Not run |
| `minimax/minimax-m3` | Answered; usage and charged cost recorded | Returned structured JSON, but made **zero** repository tool calls; probe exits unsuccessfully | Not run |
| `minimax/minimax-m2.5` | HTTP 400; no answered lane | Not reached | Not run |

For a successful GPT-4.1-mini run, the lane reported 2,237 input tokens,
44 output tokens, 2,048 cached tokens and $0.0003508; the agent reported
521 input tokens, 161 output tokens and $0.000466; vision reported
46 input tokens, 16 output tokens and $0.000044. These are individual probe
charges, not the total spend across retries and investigation.

The latest Kimi lane reported 2,371 input / 278 output / 2,304 cached and
$0.00118863. Its non-exploring agent reported 305 input / 53 output and
$0.000342. MiniMax M3 reported 1,693 input / 304 output / 1,664 cached and
$0.00047334 for the lane, and 523 input / 631 output / $0.0018282 for its
non-exploring agent. Answering-model attribution matched each requested id.

The exact reason for the two models' missing reads is not established. Do
not infer a tool-dispatch bug or a routing fix from this spike. They fail the
requested exploration contract under this configuration, and require
provider/tool/structured-output investigation upstream. The M2.5 HTTP status
is an error-text classification, not a typed status supplied by Embed.

## Offline verification

Six spike tests pass with its committed lockfile:

- The existing description lane runs through the experimental Embed adapter,
  preserving answering-model and token/cost accounting.
- A schema-invalid reply (`{"summary":123}`) is accepted as structured JSON.
  This confirms openhuman#7300 remains a production gate.
- Vision content blocks, JSON Schema and gateway options reach the wire.
- HTTP 429 returns unstructured `CoreError::Rpc`; one request is sent, with
  no automatic fallback. Typed fallback classification remains unproven.
- The fixture tool rejects traversal, URLs and extra arguments.
- A scripted agent executes the host reader and refuses built-in write,
  shell and HTTP tools. Advertised tools, refusal results and filesystem
  markers are checked.

Repository validation passed:

```text
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked: 2,036 library tests + 7 CLI tests
cargo check --locked --all-features --all-targets
```

Spike validation passed:

```text
rustfmt +1.96.1 --check --edition 2024 spikes/embed/main.rs
python3 spikes/embed/run.py test: 6 passed
python3 spikes/embed/run.py clippy --all-targets --no-deps -- -D warnings
```

The Python runner parsed successfully with Python 3. Its generated package
uses a committed lockfile and `--locked`. Embed's dependency core emits
existing warnings; `--no-deps` limits the spike's Clippy gate to its own code.

## Confirmed gap list and next step

Keep the migration blocked on the upstream issues listed in `README.md`.
Refine the original gap descriptions with the APIs that already exist:

- `Completer::timeout` exists; cancellation tokens, turn deadlines and
  propagation to children still need confirmation/implementation (#7298).
- `Completer::observer` exists; agent-turn journal/observer parity and
  Langfuse integration remain unverified (#7303).
- `ChatMessage::with_image`, `provider_options`, `answered_model` and charged
  cost work in the tested OpenRouter path. Direct providers, fallback policy
  and multi-provider agent exploration remain unverified (#7302).
- `HostOnly` plus `untrusted_input` work with the fixture and scripted calls.
  Production repository tools and their path/secret boundaries still need
  upstream implementation and adversarial tests (#7296).
- Consumer builds still require recursive submodules, mirrored path patches,
  direct use of core's `ToolResult`/stack constants and Rust 1.96.1 (#7301).

The other gates remain: enforced budgets (#7297), ordered/budgeted fan-out
(#7304), and embedding/memory compatibility (#7299). No equivalence proof for
real PR findings, golden/cassette parity, Langfuse, embeddings or server
runtime sharing was attempted. No production dependencies or code were
changed, and PR #191 was left untouched.

This is the requested no-merge spike. Phase 1 should close the upstream gates
before Phase 3–6 replace any production adapter, orchestration or embedding
surface. Agentic reviewers must additionally pass scripted and real-PR evals
before their flag can become the default.
