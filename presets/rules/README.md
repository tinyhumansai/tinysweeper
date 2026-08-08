# Rule documents

Review rules for one kind of file, as **data**. Adding a rule document is a new
Markdown file in this folder and one line of TOML — never a new module.

A preset points at one from its ordered `[[path_instructions]]` table:

```toml
[[path_instructions]]
glob = "**/*.rs"
rules = "rust"
```

`rules = "rust"` resolves to `presets/rules/rust.md` at config-load time and is
appended to that entry's `instructions`. A missing document is a configuration
error, reported once by `tinysweeper check`, rather than a silently weaker
review on every run.

## The table is ordered and first match wins

A changed path takes the rules of the **first** entry it matches and no other,
so a `.rs` file's reviewer never sees the workflow rules. That saves tokens, but
the reason it exists is precision: every rule a reviewer is shown is another
thing it can find an opinion about, and an opinion formed from rules written for
another language is noise with a citation attached.

Put the specific globs first. A `**/*.rs` entry above a `src/ports/**` entry
means the ports rules are dead.

## Write the negative list first

Roughly half of every document here is the list of cases **not** to report. That
is not padding, and it is not politeness — it is where the precision comes from.
A rule that says "flag unsynchronised shared state" fires on every local
variable in the diff. The same rule with

> Do NOT report: local variables within a function (inherently confined),
> read-only access to immutable data, values already behind a lock or a channel

fires on the two places that matter.

A rule document with no negative list will make the review worse. Write the
"do NOT report" section before the "report" section, and be specific: name the
shapes that look like the problem and are not.

## Keep them short

A document that no longer fits on two screens is doing more than one job. Split
it and give each half its own glob.
