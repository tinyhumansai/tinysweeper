# `security-strict`

For repositories where a missed vulnerability costs more than a false positive:
anything handling credentials, anything deployed to production, anything taking
outside contributions.

## What it changes

- `strictness = 3` and `severity_gate = "low"` — low-severity findings are
  posted, not folded into the summary.
- `confidence_min = 0.4` — the model is allowed to raise something it is only
  moderately sure about. This is the main source of extra noise.
- `security` fails the check at **medium**, not high.
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
