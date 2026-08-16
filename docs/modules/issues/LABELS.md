# The label vocabulary

`presets/labels.toml` is the list, as data. `scripts/sync-labels.py` applies it
to a repository:

```sh
scripts/sync-labels.py --repo owner/name --dry-run   # say what would change
scripts/sync-labels.py --repo owner/name             # apply it
scripts/sync-labels.py --repo owner/name --prune     # also remove retired ones
```

Idempotent — a second run reports `labels already match the vocabulary`.

## One axis, not two

`priority` and `severity` both existed, and the mapping between them was
one-to-one: `Critical -> p0`, `High -> p1`, `Medium -> p2`, `Low -> p3`. That is
not two facts. It is one fact written twice, in two vocabularies, with two
chances to disagree — and they did disagree, because labels were add-only for a
while and a fixed issue kept its old `severity: high` beside its new
`severity: low`.

There is now a single ordered axis, `priority: p0` … `p3`, coloured cool to hot
so a list reads at a glance without anyone learning the scheme.

The code agrees: `labels::vocabulary` offers only `priority:`, the triage schema
no longer asks a model for a severity, and pull request triage derives the
priority from the review's own worst finding and emits that one label.
`no_severity_label_is_ever_planned` fails if a `severity:` label is ever planned
again — a run that emitted one would re-create labels `--prune` has deleted.

## The facet is wider than the vocabulary

`labels::is_priority` is what triage may *apply* — the four current names.
`labels::in_priority_facet` is what a new priority *replaces* — anything spelled
`priority:`. Those two sets stop being equal the moment a name is retired, and
keeping them equal was a live bug: `openhuman` and `backend` still carry an
older axis (`priority: critical | high | medium | low`) on 539 items between
them, from before the p0–p3 scale. Matching only the current four meant an item
took `priority: p2` and kept `priority: high` beside it — two labels in one
facet, disagreeing, forever, because nothing in the add-only planner could ever
retire the older one.

Widening what `supersede` removes must not widen what triage may add. Asking for
`priority: high` is still refused as "not a label triage may apply", and
`a_retired_priority_spelling_is_never_added` is the test that holds that line.

## One facet, one owner

A label facet needs exactly one writer. CodeRabbit also applies labels on these
repositories, and with `reviews.labeling_instructions` empty it picks names
"based on prior PR patterns" — which is to say it learned `priority: pN` and
`severity: low` from tinysweeper's own history and started writing them back.
Two bots owning one facet is not two opinions; it is a ping-pong, and every
round is an add and a remove on somebody's timeline.

The fix is upstream of this repository: turn off `auto_apply_labels` in the
CodeRabbit organisation settings (it defaults to `false`; ours is on), or set
`reviews.auto_apply_labels: false` in a `.coderabbit.yaml`. Nothing in this
module can resolve it, because the other writer is not reading this policy.

## Nothing here duplicates a native field

Bug, Feature and Task are **GitHub issue types**, set on the issue itself, so
this vocabulary has no `bug` or `enhancement` label. A label shadowing a native
field is a second home for the same answer, and two homes drift.

GitHub's stock `bug` and `enhancement` labels are left alone on repositories
that already have them — they are not in this vocabulary, so the sync neither
creates nor prunes them. They are simply not what triage reaches for.

## Pruning is deliberately awkward

`--prune` only considers labels this vocabulary owns — `priority:`, `severity:`
and `tinysweeper:` — and it is opt-in. Deleting a label strips it from every
issue carrying it, and the API will not give those assignments back.

Before retiring `severity:` here, every issue holding one was checked to be
holding a `priority:` too, so nothing lost its only signal. Do that check, or
run `--dry-run --prune` first and read the list.

## The names are load-bearing

`tinysweeper:human-review` and `tinysweeper:manual-only` are kill switches read
by `app::review::kill_switch`, and `review.labels` in the configuration must
name the same strings. Renaming one here without changing the config produces a
switch that is present, documented, and silently does nothing.
