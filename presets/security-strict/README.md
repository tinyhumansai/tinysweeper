# `security-strict`

For repositories where a missed vulnerability costs more than a false positive:
anything handling credentials, anything deployed to production, anything taking
outside contributions.

## What it changes

- `strictness = 3` — medium-severity findings at 0.55 confidence or above are
  posted, not folded into the summary. This is the main source of extra noise,
  and it is as loud as any preset can make a review: the dial is
  authoritative, so a preset's `severity_gate` or `confidence_min` can only
  make it stricter. (This preset used to ask for `low`/0.4; that request was
  the dial being overridden from below, and it is no longer honoured.)
- `max_comments = 10` — twice the default inline-comment budget for the whole
  pull request. Findings over it are listed in the review hub, not dropped.
- `security` fails the check at **medium**, not high.
- `passes = 2` — a large group's first council reviewer gets one coverage
  pass: told what it already found in this unit, asked once more for what a
  first pass misses. One extra model call per group over the threshold in
  `docs/modules/lanes/README.md`.
- Draft pull requests are reviewed too.
- Explicit rules for workflow files and Dockerfiles, which is where the
  expensive mistakes actually happen.

## The trade

You will get findings you disagree with. Downvote them — a 👎 is counted, not
suppressed: there is no fingerprint list a dismissal writes to, and no class of
finding this preset will stop raising on its own. What a 👎 changes is what an
operator sees when they look at a repository's dismissal rate, not what the
next review says.

To actually silence a class of finding, write a targeted `[[path_instructions]]`
entry naming the rule and the paths it should stop applying to — see
`presets/rules/README.md`. If you find yourself writing several of these, this
preset is wrong for the repository — move to the defaults instead.
