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
- `passes = 2` — a large group's first council reviewer gets one coverage
  pass: told what it already found in this unit, asked once more for what a
  first pass misses. One extra model call per group over the threshold in
  `docs/modules/lanes/README.md`.
- Draft pull requests are reviewed too.
- Explicit rules for workflow files and Dockerfiles, which is where the
  expensive mistakes actually happen.

## The trade

You will get findings you disagree with. Downvote them: a 👎 adds the
fingerprint to `.tinysweeper/learned.toml` and suppresses that class thereafter,
so the noise decays as the repository teaches the bot its preferences.

If it never decays, this preset is wrong for the repository — move to the
defaults and add targeted `path_instructions` instead.
