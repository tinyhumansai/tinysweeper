# `graph`

The repository graph: what a change can reach.

Similarity retrieval finds code that *reads* like the diff. It does not find the
caller three files away that the diff breaks, because that caller shares no
vocabulary with it. The graph is the other half: seed it with the symbols a pull
request touched and walk outwards.

**This graph exists to be traversed during a review, not to be drawn.** That is
the whole design constraint, and it is worth stating because the obvious version
of this feature fails it. The obvious version is a dashboard endpoint —
`buildRepoGraph`, called from exactly one HTTP route, never from a review — whose
structural edges come from a regex that gives up on any specifier that does not
begin with `.` or `..`. Applied to its own codebase, which writes almost every
internal import as `@/lib/…`, it produces a graph with essentially no edges. It
looks like a feature and answers no question.

## Files

| File | Contents |
| --- | --- |
| `types.rs` | `Language`, `SourceFile`, `Definition`, `ImportStmt`, `Usage`, `Heritage`, `ParsedFile`, `Unresolved`, `Coverage`, `RepoGraph` |
| `lang.rs` | Grammar selection and the tree-sitter queries, one set per language |
| `extract.rs` | Parsing: one file in, one `ParsedFile` out. The only tree-sitter caller |
| `aliases.rs` | `AliasConfig` — path aliases read from `tsconfig.json`, `go.mod`, `Cargo.toml` |
| `path.rs` | Repo-relative path arithmetic, and `is_test_path` |
| `resolve.rs` | `Resolver` — a specifier and a file in, the file it names out |
| `build.rs` | Nodes and edges out of parsed files; `sync_all` / `build_paths` + `sync_paths` / `rebuild_set` to store them |
| `traverse.rs` | `NeighbourQuery`, the uncapped `walk`, and the capped `neighbours` |
| `impact.rs` | `Impact` — the same walk read inbound only: what breaks, and what nothing tests |

Storage goes through the `GraphStore` port (`src/ports/graph.rs`). The node,
edge and neighbourhood wire types live in `src/index/types.rs`, alongside the
MongoDB adapter and the always-compiled `MockGraphStore` that backs the tests.

## Nodes and edges

Two node kinds: `File` (id is the repo-relative path) and `Symbol` (id is
`path#name`). Six edge kinds:

| Kind | From | To |
| --- | --- | --- |
| `imports` | file | file |
| `defines` | file | symbol |
| `calls` | symbol (or file) | symbol |
| `references` | symbol (or file) | symbol |
| `extends` | symbol | symbol |
| `tests` | symbol (or file) | symbol |

A usage is attributed to the *innermost* definition containing it, so a call
inside a method is an edge out of the method rather than out of the class.

`extends` is one kind for what four languages spell four ways — TypeScript
`extends`/`implements`, Python base classes, Rust `impl Trait for Type` and
supertrait bounds, Go struct and interface embedding. The question a review
asks of all of them is the same one, so splitting them would make every
consumer enumerate the variants to ask it. Generic arguments are deliberately
not followed: `extends Repository<User>` inherits from `Repository`, and
recording `User` would put every class that merely mentions it one hop away.

`tests` is **derived, never asserted**. It is emitted for a resolved *call*
out of a test scope into code that is not itself test code — so a file claims
to cover exactly what it actually reaches, and `foo_test.rs` claims nothing
about `foo.rs` on the strength of its name. A test scope is a whole file whose
path matches a convention a real runner enforces (`_test.go`, `test_*.py`,
`.test.`/`.spec.`, a `tests/` directory), or a declaration the language's own
runner would collect: a `#[test]`/`#[cfg(test)]` item, a Go `TestX`, a pytest
`test*`. References are excluded — a test that names a type does not exercise
it.

The direction of `extends` is child to parent, so walking *inbound* from a
changed base class lists what implements it. That is the direction a review
asks in, and it is the direction `impact.rs` reads.

## Resolution is the whole value

Each language gets the rules its own toolchain uses.

| Language | Resolves |
| --- | --- |
| TypeScript / TSX / JS | relative paths, extension inference, `index` files, `compilerOptions.paths` patterns, `baseUrl`, root-relative `/…` |
| Rust | `crate::` / `self::` / `super::` / the crate's own name, `foo.rs` vs `foo/mod.rs`, bodyless `mod foo;`, `#[path = "…"]` |
| Python | dotted absolute modules matched against the tree (so `src/` layouts work with no packaging metadata), explicit relative imports, `__init__.py` packages |
| Go | package directories rooted at the `go.mod` module path; an import resolves to every non-test `.go` file in the directory, because that is what a Go package is |

Two details that only look like details:

* **`tsconfig.json` is parsed as JSONC.** Real ones carry comments and trailing
  commas, `serde_json` rejects both, and bailing would drop every alias in the
  repository — the exact failure this module exists to avoid.
* **A specifier that matched a configured alias and still found nothing is
  `NoSuchFile`, not `External`.** Classifying it as a package would hide a real
  gap behind an expected one.

Ambiguity is never guessed. Two equally plausible Python modules resolve to
`Ambiguous` and produce no edge, because a wrong edge sends retrieval into the
wrong file with full confidence, which is worse than a missing one.

## Coverage is measured, not assumed

Every specifier that produces no edge is recorded in `RepoGraph::unresolved`
with a reason, and `Coverage` counts imports as total / resolved / external.
A resolver that silently drops what it cannot handle reports the same perfect
success rate whether it works or not, so the counters are the feature.

External specifiers (`react`, `std::fmt`, `fmt`) are excluded from the
denominator: they will never resolve to a file here, and counting them as
failures would make the metric measure dependency count rather than resolver
quality.

Measured on this repository — its `src/` tree, 644 files including the vendored
harness — internal import resolution is 1.0 with 3,531 specifiers correctly
classified as external. `this_repository_resolves_every_internal_import` in
`build_test.rs` keeps it there.

Ambiguous *usages* are counted but not listed: `new`, `fmt` and `len` are
defined in dozens of files on any real codebase, and listing each occurrence
would bury the handful of broken imports the list exists to surface.

## Traversal is bounded twice

`traverse::neighbours` takes a hop count *and* a hard node cap, and the two are
not redundant. Hops bound the shape of the walk; the cap bounds its size. Two
hops out of a widely imported module is most of the repository, and a prompt
containing most of the repository is worse than one containing nothing.

Truncation is breadth-first from the seeds, so what survives is the closest
blast radius rather than whatever the store returned first, and edges whose
endpoints were dropped go with them. Defaults are deliberately small: one hop,
200 nodes.

Edges are walked in both directions. Whoever calls a changed function is at
least as much of the blast radius as whatever it calls — and for a leaf that
nothing calls, the inbound direction is the only useful one.

## Blast radius

`traverse` walks in both directions because a callee is context worth reading.
`impact::Impact` reads that same walk **inbound only**, for a narrower question:
what breaks if this change is wrong. Whatever the diff calls did not change
because the diff changed; whatever calls *it* might.

Each inbound edge becomes a relation — `tests` → exercised by, `extends` →
implemented by, `calls`/`references` → called by, `imports` → imported by — and
each node keeps only its strongest, because a coverage edge always accompanies a
call edge and listing the node twice would spend the cap saying one thing.

The cap fills **round-robin across relations**, not in priority order. Strict
priority looks right and fails on the commonest shape there is: a well-tested
function with twenty-six test callers spends the whole block naming tests and
never mentions the production code that would break.

`Impact::untested` names changed symbols nothing reaches from a test, and it is
emitted **only** for symbols the graph actually holds. "No test covers this" and
"this symbol is new, or in a language we do not parse" are different sentences,
and reporting the second as the first tells a reviewer we looked when we did
not. It is rendered above the retrieved chunks in the lane's prompt, because it
is the shortest part and the part that says what to look for.

## Incremental writes

`sync_paths` deletes by path and then upserts, in that order, mirroring how the
chunk index re-indexes. Without the delete, a renamed function stays in the
graph under both names forever and traversal keeps offering reviewers a
definition that no longer exists.

A push re-parses what changed, not the tree. Two things make that possible and
neither is optional:

* **`build_paths` takes the stored symbol table.** Resolving a call in a
  re-parsed file needs to know which file defines the name it calls, and that is
  a repository-wide fact. `GraphStore::symbols` returns it in one query of small
  rows, against the many-thousand-file parse it replaces. Stored symbols are
  seeded *before* freshly parsed ones and yield to them, so a renamed function
  does not survive under both names.
* **`rebuild_set` re-parses the changed files' graph neighbours too.**
  `delete_paths` removes every edge *touching* a path, including the inbound
  ones written by files that did not change — which are exactly the edges a
  blast radius is made of. Re-parsing one hop out puts them back.
  `an_incremental_rebuild_lands_the_same_graph_as_a_full_one` asserts the two
  paths agree, node for node and edge for edge, and it is the test that fails
  against the obvious implementation.

`src/server/indexing.rs` decides which path to take **from the store**, not from
a flag: a repository whose graph is empty is built whole. A flag can be wrong
about what is stored, and being wrong means a graph with edges only among the
files that push happened to touch.

## Known limits

* **Calls inside Rust macros are invisible.** tree-sitter parses a macro's
  arguments as a token tree, not as expressions, so `assert_eq!(normalise(x),
  y)` produces no `calls` edge and therefore no coverage edge. A test module
  that only ever asserts through macros covers nothing as far as the graph is
  concerned.
* **Incremental sync misses a newly-resolvable call from an untouched file.**
  A file that did not change and had no edge to the changed file gets no new
  edge, so a call that only now resolves — because this push added the symbol it
  names — waits until either file is touched again. Catching it would mean
  parsing every file on every push, which is the cost the incremental path
  exists to avoid.
* `Coverage` on a `build_paths` result describes the parsed subset, not the
  repository. A partial run's resolution rate is not the repository's.

* One crate root per repository: the shallowest `Cargo.toml` wins, so a Cargo
  workspace resolves `crate::` against the wrong `src/` for its non-primary
  members.
* `require()` and dynamic `import()` are not followed; only `import` / `export …
  from` statements are.
* Inline Rust modules are recognised well enough to know that `use super::*`
  inside one names its own file, but their contents are not namespaced.
* Method calls resolve by name, so an interface implemented in several files
  resolves to none of them rather than to the wrong one.
