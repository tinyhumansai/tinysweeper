# `overview`

The change map: one comment on a pull request, with a diagram of what it
touches and what that reaches.

A diff tells a reviewer which lines moved. It does not tell them which parts of
the repository those lines are *load-bearing for* — the module three files away
that imports the function whose signature just changed. That question already
has an answer in this codebase: the repository graph that `retrieve` walks to
build a lane's context. This module walks it once more, for the reader instead
of for the prompt.

## What it is not

**It is not a summary of intent.** Nothing here says what the change is *for*.
That is the author's job, the pull request body is where it goes, and the
`description` lane already objects when it is missing. A bot that writes the
summary its author should have written removes the reason to write one, and
does it from less information.

**It makes no model call.** Every number in the picture is arithmetic over the
diff and the stored graph, so two runs against one commit draw the same
diagram. That is what makes it checkable — an unreproducible picture is one
nobody can argue with — and it is why the map is on by default while almost
nothing else here is: it costs nothing.

## Files

| File | Contents |
| --- | --- |
| `types.rs` | `ChangeMap`, `Component`, `Link`, `Role`, `GraphStatus` |
| `group.rs` | Paths to components: the directory-prefix rule and the depth choice |
| `mod.rs` | `build` — diff plus neighbourhood plus findings in, `ChangeMap` out |
| `mermaid.rs` | The flowchart, and the label filter that makes a path safe to draw |
| `render.rs` | The comment body, and the `<!-- tinysweeper:change-map -->` marker |

## The unit of the picture is a directory

A component is a directory prefix, taken at the deepest level that still fits
under `overview.max_components`. A change across `src/lanes/critique.rs` and
`src/graph/build.rs` draws two boxes; the same change against a budget of one
draws a single `src`, holding both files.

Depth is chosen from the paths rather than fixed, because `src/lanes` and
`packages/web/src/app/api` are the same kind of thing in two repositories with
very different layouts, and a hard-coded depth is right for exactly one of them.

Directories, and not model-named "components", for the reproducibility reason
above — and because a directory is a claim the repository itself already made.

## Reading the diagram

| Colour | Meaning |
| --- | --- |
| Green | The pull request changes files here |
| Grey | Untouched, but reached from a changed file through an import or a call |
| Orange | Has findings from this review |
| Red | Has a finding that blocks the merge |

Arrows run from the importing side to the imported side and are labelled with
how many underlying graph edges they stand for. `defines` edges — a file to a
symbol inside it — are dropped: they are self-links by construction and would
put a loop on every component of every pull request.

## Degrading honestly

The same rule `retrieve` follows. An absent graph and a change that genuinely
reaches nothing produce the same empty diagram, so the comment states which one
happened, in four distinguishable cases:

| `GraphStatus` | Means | Whose problem |
| --- | --- | --- |
| `off` | No graph store attached — a forge-only deployment | Nobody's; supported |
| `cold` | Graph attached, knows nothing about these files | Normal for added files |
| `unavailable` | Graph attached, the walk failed | An outage; someone's |
| `walked` | The walk ran | — |

Components that do not fit are folded away and *counted* in the headline. A
picture that silently omits half a change is worse than no picture, because it
reads as complete.

## Paths are untrusted

A contributor picks their own filenames, so a path is treated exactly like a
diff: data, never syntax.

- Node ids are generated (`n0`, `n1`, …) and never derived from a path. An id
  is unquoted Mermaid syntax, so a path in that position could close the
  statement it sits in.
- Label text is filtered to an allow-list of characters rather than escaped.
  Escaping is a guess about a renderer's parser; an allow-list is a statement
  about what can appear. A file named `x"] click n0 "javascript:…` loses its
  punctuation and draws an ordinary box.
- Paths in the markdown table and file list go through
  `findings::render::escape_cell`, because a bare `|` ends a cell and GitHub
  renders inline HTML inside one.

## Where it runs

`app::review` builds the map last, from the findings that survived filtering,
so the diagram marks the components the review will actually comment on. It
rides on the proposal as `Proposal::overview` and is published by `app::apply`
— the module that holds the only write handle, as ever.

The walk is its own bounded query rather than a by-product of retrieval: the
two want different things out of the graph — retrieval wants the *chunks* of
what a change reaches so a lane can read them, the map wants the *shape* — and
a review with retrieval disabled should still get a picture. It reuses
`retrieval.graph_hops` and `retrieval.max_graph_nodes` for the bounds.

`apply` posts one comment and edits it in place forever, found by its marker
**and** by author login. The marker alone is not enough: anyone can paste it
into their own comment, and editing a contributor's comment because it quotes
one of ours is a write we were tricked into making.

A failure to draw the map never costs the verdict. It is published after the
check runs and the review, and its error is logged rather than returned.

## Configuration

```toml
[overview]
enabled = true
max_components = 10           # also sets the grain: see the depth rule above
max_impacted = 6
max_links = 24
max_paths_per_component = 12
```

Every ceiling is a legibility budget, not a cost one. Past a dozen boxes a
diagram stops being read at all.

## What is not drawn

Nothing is posted for a change that is one component with nothing reaching out
of it. The diagram would be a single box and the table would restate the files
tab — a comment that adds nothing is exactly the noise the rest of this tool
exists to avoid.
