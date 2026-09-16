# `polyglot`

For a repository that is not one language: a Go or Python service, a
TypeScript frontend, Terraform for the infrastructure, SQL migrations, a
Dockerfile, some shell scripts — all reviewed by the same bot.

## What it assumes

- Every changed path belongs to exactly one of the languages this preset
  knows about, or to none of them, in which case it gets the base review with
  no extra rules.
- The languages are distinct enough that a rule written for one is noise for
  another — a Go reviewer told about Python's `subprocess.shell=True` has
  nothing to do with the file in front of it.

## The ordering, and why it is that order

`path_instructions` is evaluated top to bottom and a path takes the **first**
glob it matches — see `presets/rules/README.md`. This preset orders entries
from most specific to least, on purpose:

1. `Dockerfile*` and `*.tf` — a handful of infrastructure files that would
   otherwise fall through to a language glob (a `Dockerfile` has no
   extension; a `main.tf` would match nothing else) or, worse, get treated as
   plain text.
2. `.github/workflows/**` — the deterministic scanner already checked
   permissions and pinning; the workflow rules adjudicate what it found.
3. Dependency manifests (`Cargo.toml`, `package.json`, `go.mod`,
   `requirements*.txt`) — reused verbatim from `security-strict`, because a
   manifest is a manifest regardless of which language it declares
   dependencies for.
4. Test files, by filename shape (`_test`, `test_`, `.test`, `.spec`) —
   ahead of every language extension, so a `handler_test.go` is judged as a
   test first and a Go file never.
5. One entry per language extension, ending in Rust — this repository's own
   dogfood case, and the reason the ordering comment in every preset here
   points back at `rust-library`.

Reorder these at your own risk: move a language entry above the test glob and
every test file in that language stops seeing the test rules.

## Combining with `security-strict` habits

This preset does not turn the `security` lane's taxonomy on — that document
is broad enough to cover every language already and duplicating it per
language would just be the same list eight times. If the repository also
wants that taxonomy, copy `security-strict`'s `[[path_instructions]]` entry
(`rules = "security"`, `lanes = ["security"]`) into this table, **before**
the language-specific entries. Lane scoping is applied before first-match
selection (see `presets/rules/README.md`), so the language entries above are
unscoped and still visible to the security lane's own table; putting the
catch-all after them means every file that also matches a language extension
never reaches it, and the taxonomy never fires. Every other lane filters this
entry out entirely, so moving it first costs those lanes nothing.

## When not to use it

If the repository is genuinely one language, `rust-library` or a
hand-written preset targeted at that language will produce fewer irrelevant
`path_instructions` entries and a shorter file to reason about.
