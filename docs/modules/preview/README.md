# `src/preview` — UI previews

What a pull request changes, shown the way a user would see it. For each
user flow the change touches: a clip, a screenshot with numbered callouts on
the elements that changed, a crop of that region, a title and a one-line
caption — as a two-column gallery in one comment per pull request, edited
in place on every run, plus a `tinysweeper/ui-preview` check run carrying
the same pictures.

The reference for the output is the kind of comment a dedicated preview bot
leaves: not "here is the page before and after" but "here is the setting you
added, toggled, with the banner it reveals", found and driven by an agent.

## Brain and hands

Producing that needs a running copy of the application and a browser, and
tinysweeper never runs contributor code (`AGENTS.md`). So the work is split,
and the split is the whole design:

- **The brain** is this module and `src/server/preview.rs`. It plans the flows
  from the diff, chooses each next browser action from an accessibility
  snapshot, names the elements to call out, writes the captions, and
  publishes. It holds the model key and the GitHub write token. It never
  runs the app.
- **The hands** are `actions/ui-preview/`, a composite action the reviewed
  repository runs in its own CI, with its own secrets — exactly the trust
  domain its tests already run in. It builds and serves the merge-base and
  the head, runs Playwright, executes whatever the brain says, draws the
  callouts, records the clip, and uploads to an object store the operator
  owns. It holds the bucket credential and a bearer for the `/preview`
  routes, and nothing else.

```
hands (repo CI)                                     brain (tinysweeper server)
POST /preview/sessions {repo, pr, head, base}  ───► verify the head against GitHub,
  ◄── {session, flows: [{id, title, start, goal}]}   plan flows from the UI diff (1 call)
for each flow, on the head:
  POST …/flows/{f}/step {url, aria, results}  ───► next commands from the snapshot (1 call)
  ◄── [{goto|click|fill|…|screenshot|annotate|done}]
  execute; draw callouts; record
then replay the same script on the merge-base (no calls)
upload {owner}/{repo}/{head}/run-…/ to the bucket
POST …/finish {manifest}                       ───► validate, caption (1 call per flow),
                                                     mint the write token, publish
```

### Why a step protocol and not a plan

A plan written from the diff alone breaks on the first selector it guessed
wrong. Playwright's ARIA snapshot — the tree a screen reader gets, a few
kilobytes of YAML — is what lets the brain see the page and choose the next
click, in the same vocabulary of roles and names its locators use, without a
pixel crossing the wire. A turn may carry several commands, so a confident
flow costs a handful of round trips and a cautious one costs one per step;
plan-ahead is the degenerate case of the same protocol.

### Before and after

Every flow runs on the head first, then the recorded script is replayed on
the merge-base with no model in the loop. A step that fails there is the
evidence — the control does not exist yet — and the flow is marked *new*,
not broken. Callouts are drawn on the head's screenshots; the base's shot of
the same step rides beside it for the captioner.

## Trust

Everything the hands send is untrusted: a same-repository pull request can
edit the job that runs them. The rules, each closing a specific door:

- **The server accepts no URL.** A manifest carries relative paths of one or
  two plain segments and a renderable extension; every published URL is
  composed from the operator's `preview.public_base_url`, the commit and the
  run (`manifest.rs`). A repository cannot override the base URL
  (`config::remote`): one that could would have the bot embed pictures from
  a host it controls into every reviewer's browser.
- **Snapshots and manifests are fenced as data** before a model sees any of
  it, like a diff. Titles and labels pass a safe alphabet and are HTML-escaped
  again at render time.
- **Counts are capped and the cap is reported**: flows, screenshots per flow,
  callouts per screenshot, commands per turn, steps per flow, dollars per
  session. What falls past a cap is counted into the comment, never dropped
  silently.
- **The bearer proves an organisation, not a pull request.** A session opens
  only after the head commit named matches the one on GitHub, read through
  the App; everything after is bounded by that session.
- **The write is one module.** `apply.rs` holds the `ForgeWrite` and executes
  what `manifest`, `caption` and `render` already decided — the same bar as
  every other write module in `AGENTS.md`.
- **Model calls are shaped, not merely capped.** Recording brackets the flow
  and the step ceiling ends it whatever the model says (`step.rs`); a vision
  model never shares the review ladder's provider pin or fallbacks
  (`GatewayModel::for_vision`), because a text fallback handed an image
  captions a picture it never saw.

## Configuration

```toml
[preview]
enabled = true                                  # repository-overridable
public_base_url = "https://previews.example.org" # operator-only: the trust anchor
max_flows = 4                                   # repository-overridable
max_steps = 25
budget_usd = 0.50
caption = true

[models]
vision = "google/gemini-3-flash"                # optional; captions read the crop
```

Plus `TINYSWEEPER_PREVIEW_TOKEN` in the server's environment — a token of
its own, because it is handed to every repository's CI and the admin token
must never be. Unset, the `/preview` routes are not mounted.

Rolling a repository out is three files and no code: see
`templates/ui-preview/README.md`.

## Files

| file | role |
| --- | --- |
| `types.rs` | The wire contract: `Command`, `Locator`, `Observation`, `Manifest`, `Gallery`; the test pins every `op` spelling `driver.mjs` dispatches on |
| `plan.rs` | One call over the UI diff → the flows; `is_ui_path` |
| `step.rs` | One call per turn → the next commands; the ceilings; `FlowState` the server persists |
| `caption.rs` | One call per flow after the pictures exist; images attached when `models.vision` is set |
| `manifest.rs` | Parse, validate, compose URLs — the only path from the wire to a `Gallery` |
| `render.rs` | The two-column gallery, `MARKER`, the check-run images |
| `apply.rs` | The write module: one comment edited in place, one neutral check |
| `session.rs` | What the server remembers between calls |
| `src/server/preview.rs` | The three routes, behind their own bearer, bodies capped |
| `actions/ui-preview/` | The hands, with their own README |

## Testing

Everything in `src/preview` is offline: `MockModel` answers the plan, the
steps and the captions; `MockForge` records the comment and the check. The
golden tests pin the exact comment body for a fixture gallery, the refusal of
every hostile manifest path, the cap counts, and that a moved head publishes
nothing. `src/server/preview.rs` is tested against a recorder behind the
`Previews` trait, so the routes are covered without a database, an App key
or a model. The action has its own `node --test` suite for the geometry, the
command mapping, the manifest and the client; the CI `node` job runs it.

`tinysweeper preview render --manifest <file>` prints the comment a manifest
would produce, offline; `tinysweeper preview plan --repo o/r --pr N` prints
the flows the server would plan.
