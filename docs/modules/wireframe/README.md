# `src/wireframe` — ASCII wireframes of UI changes

For each screen or modal a pull request adds, removes or changes, a compact
ASCII drawing of it before this pull request and after — posted as one
durable comment, edited in place on every push.

The reference for the output is "here is roughly what this screen looked
like, and here is what it looks like now", read straight off the diff. It is
not a picture; it is a sketch a reviewer can skim without running the app.

## Independent of `src/preview`, on purpose

`src/preview` (`docs/modules/preview/README.md`) drives a real browser
against a running build of the reviewed repository, in that repository's own
CI. This module does neither. It makes one model call over the diff's UI
files — the same `+`/`-` patch text every review lane already reads — and
asks the model to reconstruct each touched screen's rough layout well enough
to draw it in text.

That means:

- No repository opt-in. A repository does not need `actions/ui-preview` or
  any workflow file for this to run; it is on by default (`[wireframe]
  enabled = true`) the same way a lane is.
- No browser, no target-repo CI, no object storage, no second commit build.
- No dependency, even in code: `is_ui_path` is duplicated from
  `preview::plan` rather than imported (`src/wireframe/mod.rs`), so a change
  to one heuristic is a deliberate decision about the other, never an
  accidental side effect of it.
- Lower fidelity. A wireframe drawn from source is a guess about layout, not
  a screenshot of one. It is meant to orient a reviewer, not replace looking
  at the real thing — `src/preview`, where a repository has it, still does
  that job.

## Shape

One model call, made once per head commit, the same shape as
`preview::plan`'s planning call:

1. Filter the diff to UI files (`is_ui_path`: markup, styles and the
   component languages — not tests, stories or helpers).
2. Ask the model, with the diff fenced as untrusted data, for the screens or
   modals it touches: a title, a status (`added` / `removed` / `changed`),
   and a `before` and/or `after` ASCII wireframe.
3. Cap and clamp the answer: at most `max_screens` screens, each wireframe at
   most `max_width` columns and `max_height` lines — a model does not always
   keep to the numbers it was asked for, so the ceiling is enforced again
   after the call, not merely requested in the prompt. An `added` screen's
   `before` and a `removed` screen's `after` are discarded regardless of what
   the model sent, so the render never shows a "before" for something that
   did not exist yet.
4. `src/app/review.rs` carries the result on `Proposal::wireframe`, the same
   way `overview::ChangeMap` rides on `Proposal::overview`.
5. `src/app/apply.rs` publishes it as its own comment, found by
   `wireframe::render::MARKER` and edited in place — not folded into the
   review hub, because a screen-by-screen gallery can be the bulkiest thing
   this bot posts, and burying it in the narrative summary is how nobody
   scrolls to it.

A pull request with no UI file at all costs no call: `build` returns an empty
set for free, the same short-circuit `preview::plan` uses.

## Trust

The diff is the only untrusted input, and it is handled exactly like every
review lane's: fenced as data in the prompt, with the system instructions
saying so explicitly. Nothing here writes to GitHub — `build` (in
`src/wireframe/mod.rs`) only ever returns a value; the comment is published by
`src/app/apply.rs`, which already holds the repository's `ForgeWrite` for
every other write this bot makes. Titles are filtered to a safe alphabet
before they reach a rendered comment, and the whole body is HTML-escaped
again at render time — the same belt-and-braces `preview::manifest` and
`overview::mermaid` use — so a hostile diff cannot use a wireframe's title or
content to break out of the `<pre>`/`<table>` it is rendered inside of.

## Configuration

```toml
[wireframe]
enabled = true      # repository-overridable
max_screens = 6      # repository-overridable, only downward
max_width = 60        # operator-only
max_height = 20        # operator-only
```

`max_width` and `max_height` stay operator-only for the reason `[grouping]`'s
ceilings do (`src/config/remote.rs`): nothing enforces a smaller wireframe
server-side, so a repository raising either arbitrarily could make one call
carry a far larger prompt than any review of the same files would otherwise
send.

## Files

| file | role |
| --- | --- |
| `types.rs` | `ScreenStatus`, `Screen`, `WireframeSet` — the shapes `Proposal::wireframe` carries |
| `mod.rs` | The model call: prompt, schema, parsing, capping, `is_ui_path` |
| `render.rs` | The comment: `MARKER`, the before/after two-column gallery |

## Testing

Everything here is offline. `MockModel` answers the wireframe call; the
golden test in `src/wireframe/mod.rs` pins that an `added` screen keeps no
`before`, a `removed` screen keeps no `after`, and a screen past `max_screens`
is dropped and counted rather than silently discarded. `src/wireframe/render.rs`
pins the exact comment body for a fixture set, offline, the same discipline
`preview::render` and `overview::render` use.
