# Turning on UI previews for a repository

Three files to copy, five secrets to set, nothing to build. The server does
the planning and the writing; this repository's CI does the browsing.

## 1. Once per organisation

- An S3-compatible bucket with a **public** domain in front of it — a
  Cloudflare R2 bucket with a custom domain is the reference setup. The
  images in the pull request comment are served from that domain, so it has
  to be reachable by whoever reads the pull request.
- Organisation secrets: `PREVIEW_S3_BUCKET`, `PREVIEW_S3_ENDPOINT`
  (`https://<account>.r2.cloudflarestorage.com`), `PREVIEW_S3_ACCESS_KEY_ID`,
  `PREVIEW_S3_SECRET_ACCESS_KEY` — a key that can write that one bucket and
  nothing else — and `TINYSWEEPER_PREVIEW_TOKEN`, the same value the server
  has as its `TINYSWEEPER_PREVIEW_TOKEN` environment variable.
- Organisation variable: `TINYSWEEPER_SERVER_URL`.
- On the server: `[preview] enabled = true` and
  `public_base_url = "https://<the public domain>"` in its `.tinysweeper.toml`,
  and `TINYSWEEPER_PREVIEW_TOKEN` in its environment. See `deploy/README.md`.

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
