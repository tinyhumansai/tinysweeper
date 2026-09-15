# Turning on UI previews for a repository

Three files to copy, five secrets to set, nothing to build. The server does
the planning and the writing; this repository's CI does the browsing.

## 1. Once per organisation

- An S3-compatible bucket that serves objects **publicly** — the images in
  the pull request comment are fetched by GitHub's image proxy, anonymously.
  On AWS S3 that is a bucket policy allowing `s3:GetObject` on the preview
  prefix (with the policy-related *Block public access* settings off; ACLs
  can stay disabled), and `public_base_url` on the server is
  `https://<bucket>.s3.<region>.amazonaws.com`. On R2 it is a custom domain
  in front of the bucket.
- Organisation secrets: `PREVIEW_S3_BUCKET`, `PREVIEW_S3_ENDPOINT` — the
  *service* endpoint the job uploads to, `https://s3.<region>.amazonaws.com`
  on AWS or `https://<account>.r2.cloudflarestorage.com` on R2, never the
  public URL (the SDK adds the bucket itself) — `PREVIEW_S3_ACCESS_KEY_ID`,
  `PREVIEW_S3_SECRET_ACCESS_KEY` — a key that can write that one bucket and
  nothing else — and `TINYSWEEPER_PREVIEW_TOKEN`, the same value the server
  has as its `TINYSWEEPER_PREVIEW_TOKEN` environment variable.
- Organisation variables: `TINYSWEEPER_SERVER_URL`, and `PREVIEW_S3_REGION`
  for an AWS bucket (`us-east-1`, say; leave it unset for R2).
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
