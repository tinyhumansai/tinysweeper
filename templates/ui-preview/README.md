# Turning on UI previews for a repository

Three files to copy, five secrets to set, nothing to build. The server does
the planning and the writing; this repository's CI does the browsing.

## 1. Once per organisation

- On the tinysweeper GitHub App: **`contents: write`** (it was read). The
  server commits every run's pictures to a store branch of the reviewed
  repository — `tinysweeper/ui-previews` by default — through the App, and
  the pull request comment embeds them from there. No bucket, no CDN, no
  storage credential anywhere. The branch is a store, not a line of
  development; deleting it costs old comments their pictures and nothing
  else.
- Organisation secret `TINYSWEEPER_PREVIEW_TOKEN`, the same value the server
  has as its `TINYSWEEPER_PREVIEW_TOKEN` environment variable; organisation
  variable `TINYSWEEPER_SERVER_URL`.
- On the server: `[preview] enabled = true` in its `.tinysweeper.toml` and
  `TINYSWEEPER_PREVIEW_TOKEN` in its environment. See `deploy/README.md`.

An object store can replace the branch: set `preview.public_base_url` on the
server and pass the action's `bucket`/`endpoint`/`access-key-id`/
`secret-access-key` inputs from secrets. That is the only reason to give the
job a storage credential.

## 2. Per repository

| copy | to | then |
| --- | --- | --- |
| `ui-preview.yml` | `.github/workflows/ui-preview.yml` | adjust the `paths:` filter and the setup step to the stack |
| `ui-preview.json` | `.tinysweeper/ui-preview.json` | fill in auth, mocks, entry points — see `actions/ui-preview/README.md` |
| `serve.sh` | `scripts/ui-preview/serve.sh` | make it serve one checkout on `$TS_PORT` against a mocked backend |

Then commit a fixture directory for the mocks (`.tinysweeper/fixtures/api/…`)
holding the JSON your app needs to render its screens logged in.

A repository can also turn previews off for itself, or lower the flow count,
in its `.tinysweeper.toml`:

```toml
[preview]
enabled = false
max_flows = 2
```

## What the job may not do

Fork pull requests get no preview: the job cannot read the secrets, and the
workflow's `if:` skips it rather than failing. Draft pull requests are
skipped too, like every other tinysweeper lane.

The `serve` script runs your code with your own CI's permissions, exactly as
your tests do. The tinysweeper server never runs it; it only reads what the
browser saw and treats all of it as untrusted.
