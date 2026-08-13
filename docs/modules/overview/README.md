# `overview`

The change-flow comment: one Mermaid diagram on a pull request that explains
how behaviour moves through the code touched by the change.

A reviewer can already find changed files and line counts in GitHub. The useful
missing context is semantic: which named operation changed, who calls or tests
it, and what it calls or uses next. `overview` derives that from the repository
graph that retrieval already maintains.

## Reading the diagram

The unit of the picture is a named symbol, not a file or directory. Hunk
headings identify the changed symbols, and typed graph edges connect them to
surrounding symbols.

| Arrow | Meaning |
| --- | --- |
| `calls` | The source invokes the target |
| `uses` | The source references the target |
| `implements` | The source implements, extends, or embeds the target |
| `tests` | The source test exercises the target |

Green nodes are changed behaviours. Grey nodes are unchanged behaviours that
provide upstream or downstream context. Orange and red retain the review's
finding and blocking signals.

Import and definition edges are deliberately omitted. They describe file
layout, not behavioural flow, and were the reason the old diagram read like a
visual Files tab. File names, directory components, churn totals, component
tables, and the expanded changed-file list are likewise absent from the
comment.

## Determinism and trust boundary

The diagram makes no model call. Git hunk headings and graph edges produce the
same result for the same commit, without asking a model to invent intent or
architectural names.

Symbol text is contributor-controlled input. Mermaid node ids are generated
(`n0`, `n1`, …), and labels pass through an allow-list before entering the
diagram. Source paths remain internal identifiers so same-named symbols do not
collapse together, but are never rendered.

## Degrading honestly

Nothing is posted when the diff has no named symbol or the graph has no typed
behavioural edge to explain. Falling back to directories or files would
reintroduce the inventory the flow is meant to replace.

`GraphStatus` still distinguishes an unattached graph, a cold index, a graph
outage, and a completed walk in the serialized proposal. A completed diagram
reports how many graph nodes were walked and how many surrounding behaviours it
shows.

## Where it runs

`app::review` builds the flow after findings have been filtered, so changed
nodes can carry the findings the review will actually publish. The flow rides
on `Proposal::overview`; `app::apply` posts it as one durable comment and edits
that same comment after later pushes.

The walk is bounded by `retrieval.graph_hops` and
`retrieval.max_graph_nodes`. Diagram legibility uses the existing overview
limits:

```toml
[overview]
enabled = true
max_components = 10 # changed behaviours
max_impacted = 6    # surrounding behaviours
max_links = 24      # typed relationships
```

`max_paths_per_component` remains accepted for configuration compatibility but
is not used by the behavioural renderer because it never lists paths.

## Files

| File | Contents |
| --- | --- |
| `types.rs` | Change-flow nodes, typed links, roles, and graph status |
| `mod.rs` | Deterministic construction from diff headings and graph edges |
| `mermaid.rs` | Safe Mermaid flowchart rendering |
| `render.rs` | Durable PR comment body and marker |
