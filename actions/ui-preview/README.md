# `actions/ui-preview` — the hands

The half of tinysweeper's UI preview that runs in *your* repository's CI. It
serves your pull request's base and head, drives a browser through the user
flows the tinysweeper server plans, draws numbered callouts on what changed,
records a clip, and uploads the lot to an object store. The server writes the
comment. See `docs/modules/preview/README.md` for the whole picture.

It is deliberately dumb. It builds nothing itself (your `serve` script does),
decides nothing itself (the server does), and holds no model or GitHub-write
credential. Everything it sends the server is treated there as untrusted.

## Using it

Copy the three files from `templates/ui-preview/` into your repository and
read that folder's README for the secrets. The workflow calls this action:

```yaml
- uses: tinyhumansai/tinysweeper/actions/ui-preview@main
  with:
    server: ${{ vars.TINYSWEEPER_SERVER_URL }}
    token: ${{ secrets.TINYSWEEPER_PREVIEW_TOKEN }}
    before-dir: ../before
    base-sha: ${{ steps.base.outputs.sha }}
    head-sha: ${{ github.event.pull_request.head.sha }}
    bucket: ${{ secrets.PREVIEW_S3_BUCKET }}
    endpoint: ${{ secrets.PREVIEW_S3_ENDPOINT }}
    access-key-id: ${{ secrets.PREVIEW_S3_ACCESS_KEY_ID }}
    secret-access-key: ${{ secrets.PREVIEW_S3_SECRET_ACCESS_KEY }}
```

`head-sha` must be `github.event.pull_request.head.sha`, never `github.sha`:
the latter is the synthetic merge commit, and the server checks the head.

## The config: `.tinysweeper/ui-preview.json`

```json
{
  "serve": "bash scripts/ui-preview/serve.sh",
  "ready": "/",
  "timeout_s": 420,
  "viewport": [1440, 900],
  "auth": {
    "cookies": [{ "name": "session", "value": "preview-session" }],
    "localStorage": { "token": "preview-token" }
  },
  "mocks": [{ "url": "**/api/**", "dir": ".tinysweeper/fixtures/api" }],
  "mask": ["[data-testid=clock]"],
  "entry_points": [{ "name": "settings", "path": "/settings" }],
  "max_flows": 4
}
```

| key | what it is |
| --- | --- |
| `serve` | The command that serves one checkout. Started with `TS_CHECKOUT` (the directory) and `TS_PORT` in its environment; must keep running. Both sides are served at once, so it must not assume a fixed port. |
| `ready` | A path that answers 2xx/3xx once the app is up. |
| `timeout_s` | How long to wait for `ready`. |
| `viewport` | `[width, height]` in CSS pixels. Screenshots are taken at 2x. |
| `auth.cookies` | Cookies set on the browser context before the first page. `domain` defaults to `127.0.0.1`. |
| `auth.localStorage` | Keys seeded into `localStorage` on every page, if unset. |
| `mocks` | Routes intercepted with Playwright's `page.route`. A `dir` mock answers `GET /api/me` with `<dir>/api/me.json` (also `me.GET.json`, `me/index.json`); a `body` mock answers inline. |
| `mask` | CSS selectors hidden in screenshots — clocks, avatars, anything nondeterministic. |
| `entry_points` | Hints for the planner about where the app's areas live. Not a capture list. |
| `max_flows` | Repository-side cap; the server has its own. |

`auth` values are whatever your app accepts against the mocked backend. They
are committed to the repository, so they must be preview-only credentials
that unlock nothing real.

## Running it locally

```sh
cd actions/ui-preview && npm ci
TS_TOKEN=… node run.mjs --server https://sweeper.example.org \
  --before ../before --after . --repo owner/name --pr 123 \
  --head-sha "$(git rev-parse HEAD)" --base-sha "$(git merge-base HEAD origin/main)" \
  --out ./ui-preview-out --no-upload
```

`--no-upload` leaves the run in `--out`; the server still publishes a comment
whose images will not resolve, so use a test pull request. `TS_BEFORE_PORT`
and `TS_AFTER_PORT` move the two servers off 3000/3001. If Chromium dies with
"Target crashed" on a screenshot, `/dev/shm` is too small or quota'd on your
machine; the runner already passes `--disable-dev-shm-usage`, and a
disk-backed `TMPDIR` fixes the rest.

The clip is `.mp4` when a full `ffmpeg` is on the path and `.webm` otherwise
(Playwright's bundled ffmpeg encodes only VP8, and cannot write a gif at all —
on a CI runner the action installs the real one).

## Files

| file | role |
| --- | --- |
| `action.yml` | The composite action: Node, Playwright, ffmpeg, then `run.mjs`, then the run as an artifact |
| `run.mjs` | The loop: serve both sides, open the session, drive each flow, replay it on the base, draw, cut, upload, finish |
| `src/config.mjs` | The config, defaulted and checked |
| `src/serve.mjs` | Spawn the repository's `serve` script and wait for `ready` |
| `src/session.mjs` | The three `/preview` routes, with retries on 5xx only |
| `src/driver.mjs` | One arm per server command; mirrors `src/preview/types.rs::Command` |
| `src/annotate.mjs` | Pill geometry and the SVG overlay, composited with `sharp`; the crop |
| `src/clip.mjs` | Marks in Playwright's video, cut with ffmpeg into a video and a gif |
| `src/upload.mjs` | `PutObject` per file under `{owner}/{name}/{head_sha}/{run}/` |
| `src/manifest.mjs` | The manifest the server validates, and the job summary |
| `test/` | `node --test`: geometry, command mapping, manifest, config, client |
