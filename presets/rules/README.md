# Rule documents

Review rules for one kind of file, as **data**. Adding a rule document is a new
Markdown file in this folder and one line of TOML — never a new module.

## Available documents

| Document | For |
| --- | --- |
| [`rust.md`](rust.md) | `.rs` |
| [`tests.md`](tests.md) | test files, by filename shape, in any language |
| [`security.md`](security.md) | the `security` lane's taxonomy, language-agnostic |
| [`workflows.md`](workflows.md) | `.github/workflows/**` |
| [`dependencies.md`](dependencies.md) | dependency manifests and lockfiles |
| [`go.md`](go.md) | `.go` |
| [`python.md`](python.md) | `.py` |
| [`typescript.md`](typescript.md) | `.ts`, `.tsx`, `.js`, `.jsx`, `.mjs`, `.cjs` |
| [`java.md`](java.md) | `.java` |
| [`dockerfile.md`](dockerfile.md) | `Dockerfile*` |
| [`shell.md`](shell.md) | `.sh` |
| [`sql.md`](sql.md) | `.sql` |
| [`terraform.md`](terraform.md) | `.tf` |

`presets/polyglot/` is the preset that wires all of them into one ordered
table; see its `README.md` for why that order is what it is.

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

## Scope a document to the lane it was written for

```toml
[[path_instructions]]
glob = "**/*"
rules = "security"
lanes = ["security"]
```

`lanes` defaults to every lane. A document written for one lane is precision
there and pure cost elsewhere: more prefix tokens on every call, and one more
subject each of the other reviewers can form an opinion about. The filter runs
*before* first-match selection, so an entry scoped to another lane never
consumes a path's one match.

## The table is ordered and first match wins

A changed path takes the rules of the **first** entry it matches and no other,
so a `.rs` file's reviewer never sees the workflow rules. That saves tokens, but
the reason it exists is precision: every rule a reviewer is shown is another
thing it can find an opinion about, and an opinion formed from rules written for
another language is noise with a citation attached.

Put the specific globs first. A `**/*.rs` entry above a `src/ports/**` entry
means the ports rules are dead.

## Merge a specific entry with the language document beneath it

Shadowing is what the previous section wants most of the time — a `.rs` file's
reviewer should not see the workflow rules. Occasionally an entry wants both
its own rules **and** the broader language document a later entry would have
supplied, without duplicating that document's text into the specific entry:

```toml
[[path_instructions]]
glob = "src/ports/**"
instructions = "One trait per file."
merge = true

[[path_instructions]]
glob = "**/*.rs"
rules = "rust"
```

`merge = true` keeps looking, past this entry, for the next matching
(lane-scoped) entry — here, the `rust` document — and appends its instructions
after this one's, specific first, separated by a blank line. A `src/ports/x.rs`
reviewer now sees both; anything else under `src/ports/**` that is not a `.rs`
file still only sees the ports rules, because the second search still has to
match.

This is **one level only**: if the entry `merge` reaches were itself
`merge = true`, that flag is ignored — it does not keep looking for a third
entry. A `merge = true` entry with no further match simply renders alone; that
is not an error. Off by default: most entries want plain shadowing, and this
exists for the narrower "my own rule *and* the language document" case.

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
