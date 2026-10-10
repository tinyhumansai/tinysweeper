# OpenHuman Embed spike — issue #197, Phase 0

This is an experimental, **no-merge** consumer, not a production migration.
It leaves tinysweeper's manifest, lockfile, lanes and feature graph unchanged.
It also avoids duplicating the migration already proposed in PR #191.

## Reproduce

Python 3.11+, Rust 1.96.1 and a recursively initialised OpenHuman checkout at
`a1075723249eb6965acca95f3ca3a694e056c9e8` are required. `run.py` verifies the
revision, tracked cleanliness and recursive submodule pins. It generates a
standalone consumer manifest beneath this checkout's `target/`, mirrors the
pinned workspace's applicable path patches, and uses `default-features = false`.
The baseline lane is consumed at a pinned git revision; the standalone
lockfile is committed and Cargo runs with `--locked`. This explicitly exposes
the current packaging gap; it is not the intended
production dependency setup.

```sh
export OPENHUMAN_CHECKOUT=/absolute/path/to/pinned/openhuman
python3 spikes/embed/run.py test
python3 spikes/embed/run.py clippy --no-deps -- -D warnings
SPIKE_VISION=1 python3 spikes/embed/run.py run
```

The live probe defaults to OpenRouter's `openai/gpt-4.1-mini` and reads
`OPENROUTER_API_KEY`. It prints accounting and probe status, never prompts,
responses or credentials. Override the route without placing the key in an
argument:

```sh
SPIKE_MODEL=moonshotai/kimi-k2.5 python3 spikes/embed/run.py run
SPIKE_MODEL=minimax/minimax-m3 python3 spikes/embed/run.py run
# Direct provider routing, when the matching key is available:
SPIKE_ENDPOINT=https://api.moonshot.ai/v1 SPIKE_KEY_ENV=MOONSHOT_API_KEY \
  SPIKE_MODEL=kimi-k2.5 python3 spikes/embed/run.py run
```

A successful exit requires an answered description lane, an answering-model
attribution, at least one agent repository read, a structured agent finding,
and unchanged fixture contents. Both live operations have a 120-second host
watchdog. This watchdog does **not** establish Embed-native cancellation or
budget enforcement. Calls incur provider charges.

## What it exercises

- The existing `Description` lane receives an experimental `Arc<dyn Model>`
  backed by `Completer`; the lane itself is unchanged.
- A runtime-owned agent uses `Access::readonly()`, `ToolScopeSpec::HostOnly`
  and `untrusted_input(true)` to inspect a host-owned fixture checkout.
- The only advertised tool reads `src/math.rs`. Its arguments are checked
  exactly; it refuses traversal, URLs, extra arguments and a final symlink.
  It scrubs secrets and labels returned code as untrusted data.
- A scripted provider requests `read_file`, `write_file`, `shell` and
  `http_request`. The test checks the advertised tool list, actual host-tool
  execution, refusal results and filesystem markers.
- A loopback completion proves answering-model and cost accounting while
  demonstrating that parseable schema-invalid JSON is currently accepted.
- Loopback probes check vision content blocks and gateway options, and confirm
  that HTTP 429 is an unstructured `CoreError::Rpc` with no automatic retry.

The fixture tool is deliberately narrow. It is not the production tree,
range-read, grep, symbol, graph or git-show toolset. It runs only against a
host-created fixture with trusted parent directories; its symlink check is
not a general defence against concurrent changes to an attacker-owned tree.

## Security note

No GitHub credentials or write port enter the adapter or agent. The agent
receives exactly one host-provided read-only tool, with no shell, MCP,
workspace write or network tool. Only the configured provider transport and
host-created loopback backend communicate over HTTP. Contributor code is
never built or executed. The test fixture's source is read as data. This
spike cannot establish production safety for broader repository tools.

## Upstream gates

At the inspected pin, the production migration remains gated by:

| Issue | Required before production migration |
| --- | --- |
| openhuman#7300 | Schema validation, bounded repair and typed failures; the spike demonstrates the gap. |
| openhuman#7298 | Cancellation propagation and turn deadlines; `Completer::timeout` already exists. |
| openhuman#7297 | Enforced token/cost budget, including children and concurrent work. |
| openhuman#7304 | Ordered, isolated fan-out with shared budget and bounded child depth. |
| openhuman#7303 | Agent-turn observer/journal parity; `Completer::observer` already exists. |
| openhuman#7302 | Confirm direct-provider routes, ladder mapping and vision parity. |
| openhuman#7296 | Production read-only repository tools; the fixture reader is insufficient. |
| openhuman#7301 | Consumer packaging without recursive submodules or mirrored patches; shared server runtime. |
| openhuman#7299 | Confirm embedding signature and Cortex memory semantics. |

The baseline before the spike passed 2,036 library tests and 7 CLI tests.
Live provider results and the final go/no-go are recorded in `RESULTS.md`.
Do not treat unexecuted probes as evidence of parity. In particular, this
spike does not establish real-PR evaluation parity, native budgets,
Langfuse parity, fallback-ladder parity, provider-level vision parity or embedding parity.
