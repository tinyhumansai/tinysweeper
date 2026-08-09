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
