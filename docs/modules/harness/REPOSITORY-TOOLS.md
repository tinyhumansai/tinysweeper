# Host-backed repository tools

`harness::agentic::repository::repository_tools` builds five read-only tools
from an `Arc<dyn RepositoryHost>`. The host supplies repository snapshots or
indexes; TinySweeper supplies argument validation and the model-facing envelope.
This is an internal reviewer adapter, rather than an OpenHuman Embed API.
This module opens no files, runs no commands, makes no network requests, and
has no workspace or write API.

## Host contract

Implement the async `RepositoryHost` trait using `async_trait`:

- `query(RepositoryQuery) -> anyhow::Result<String>` handles a validated
  `List`, `Read`, `Search`, `Lookup`, or `GitShow` request.
- `redact(String) -> anyhow::Result<String>` removes secrets before any data
  enters the tool result or conversation. This method is required; an identity
  implementation is appropriate only for a source already known to be safe.

The host is trusted code. It must enforce repository scope, permissions,
symlink containment, and snapshot identity, bound its own CPU/memory/IO costs,
and never execute contributor code. A path's lexical validation cannot enforce
filesystem containment. Prefer an immutable tree/index to filesystem access.
Treat search terms as literal data, and never interpolate them into commands.
The facade also re-exports `ToolResult` for custom host tools; consumers need
no direct dependency on `openhuman-core`.

## Model-facing tools

| Tool | Arguments | Semantics |
| --- | --- | --- |
| `repo_list` | `path`, `limit` | Tree or directory entries; `.` means the repository root. |
| `repo_read` | `path`, `start_line`, `end_line` | Inclusive, one-based file range. |
| `repo_search` | `path`, `query`, `limit` | Literal text search beneath a path; `.` means root. |
| `repo_lookup` | `symbol`, `limit` | Host-defined symbol, callers, references, or graph lookup. |
| `repo_git_show` | `commit`, `path`, `start_line`, `end_line` | Inclusive file range at an immutable commit ID. |

All arguments are required; unknown fields are refused. Limits are 1–200
results, 1–1,000 lines per range, 4,096 UTF-8 bytes per path, and 1,024 UTF-8
bytes per nonempty search term or symbol. Paths must be normalized relative
paths: absolute paths, backslashes, colons, tildes, controls, empty components,
`.`/`..` components, and `.git` components are refused. Root `.` is permitted
only for listing and search. Commits must be full 40- or 64-character ASCII
hexadecimal IDs; symbolic refs, abbreviations and revision expressions are
refused. Validation happens before calling the host.

Every successful query passes through redaction, then becomes compact JSON
inside a Markdown fence labeled `UNTRUSTED_REPOSITORY_DATA`. JSON escapes
embedded newlines, preventing repository text from ending the fence. The
model-facing text is bounded to 65,536 bytes, including its envelope; long
results are truncated at a UTF-8 boundary and carry `truncated: true`.
Host and redactor failures return generic errors, never their diagnostic text.
No raw host output is logged or included as metadata.

## Attach to a reviewer

`harness::agentic` installs these host-owned tools through Embed's neutral
`HostTurnTools::advertised` API, using `ToolScopeSpec::HostOnly` and
`Access::readonly()`. Turns use fenced evidence and `untrusted_input(true)`.
HostOnly keeps built-in shell, write, network, memory, MCP and delegation tools
out of both the advertised and executable belt. TinySweeper decides which
repository data is allowed and how to redact it. Fence and label the original
PR text separately; the repository envelope covers tool results only.

## Verification

`src/harness/agentic/repository/test.rs` exercises delegation, invalid inputs, redaction
failures, delimiter injection and bounded Unicode output without a runtime.
`src/harness/agentic/repository/runtime_test.rs` uses a scripted provider against a real
HostOnly, read-only, untrusted-input agent, asserting the exact tool catalogue,
refusals of built-in shell/write/network calls, filesystem and HTTP inactivity,
and secret-free fenced results in the next inference request.

```bash
cargo test --locked --features harness harness::agentic::repository --lib
```
