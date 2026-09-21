# MCP server

Tinysweeper can expose an authenticated HTTP Model Context Protocol endpoint at
`POST /mcp`. It gives agents constrained, repository-aware access without
giving them a GitHub token, a shell, or database credentials.

Enable it in the deployment configuration:

```toml
[mcp]
enabled = true
token_env = "TINYSWEEPER_MCP_TOKEN"
allowed_org = "tinyhumansai"
```

Set `TINYSWEEPER_MCP_TOKEN` to a random value of at least 32 characters in the
server's secret environment. When the value is absent, `/mcp` is not mounted.
The server compares its SHA-256 digest in constant time before parsing a
request. `allowed_org` is enforced on every tool call, and the GitHub App must
also be installed on the target repository.

## Agent integration

Use an MCP client that supports streamable HTTP or JSON-RPC over HTTP. Send the
normal `initialize`, `notifications/initialized`, `tools/list`, and
`tools/call` requests to the same URL with:

```text
Authorization: Bearer $TINYSWEEPER_MCP_TOKEN
Content-Type: application/json
```

The server exposes four tools:

- `search_code(repo, query, limit?)` performs the existing hybrid dense and
  lexical search over the repository's vector index. Results quote paths,
  symbols, line ranges, scores, and source text. It reports a useful error when
  embeddings have not been configured or the repository is not indexed.
- `search_issues(repo, query, limit?)` searches open and closed issues in the
  repository, excluding pull requests. Results include state, labels, comment
  count, URL, and a bounded body excerpt. The default limit is 10 and the
  maximum is 20.
- `read_docs(repo, path?)` reads Markdown, `docs/`, and issue templates from
  the default-branch commit. Supplying `path` reads exactly one file. Agents
  should call this before proposing an issue so repository conventions and
  templates are part of their reasoning.
- `create_issue(repo, title, body, labels?, force?)` takes an atomic seven-day
  repository-and-title claim and searches the repository's issue history. It
  returns likely open or closed duplicates without a write unless `force` is
  explicitly true. The agent supplies the final body. Titles are capped at 256
  bytes and bodies at 64 KiB before any provider or GitHub request.

The idempotency claims, index and issue history are persistent across MCP
requests. The claim closes the concurrency and GitHub search-index delay;
history catches older duplicates. The existing Cortex memory remains available
to review and can be backfilled through the established admin route.

## Security model

MCP never accepts a repository URL, arbitrary checkout path, or GitHub token.
Repositories are parsed as `owner/name`, checked against the configured
organisation, and resolved through the installed GitHub App. Code and docs are
read from an immutable default-branch commit. The only write is issue creation.
The planner produces an immutable issue plan after every read and policy
decision; the existing `src/app/apply.rs` write boundary alone mints the
credential and executes that plan. No model or external agent receives a write
credential.
