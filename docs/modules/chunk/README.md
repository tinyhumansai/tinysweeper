# `chunk`

Turning a source tree into indexable chunks: which files, which spans, and what
each span honestly claims about itself.

## Files

| File | Contents |
| --- | --- |
| `types.rs` | `SourceChunk`, `ChunkOptions`, `SkipReason`, `SkippedFile`, `Selection` |
| `lang.rs` | `Language`, the extension allowlist, `is_indexable` |
| `select.rs` | `Selector` — ignore globs, size cap, checkout walk, `ignore_globs` |
| `lines.rs` | The fallback line splitter |
| `tree.rs` | The tree-sitter splitter. Behind `treesitter` |
| `chunker.rs` | `Chunker` — spans, hashing, `Chunk` construction |

## The failure this module exists to avoid

The obvious chunker accumulates characters until it hits a limit, cuts, and
repeats a couple of lines into the next chunk as "overlap". It is content-blind:
a span routinely starts halfway through one function and ends halfway through
another. Retrieval then returns a chunk that does not contain the symbol it was
retrieved for, and the reviewer quotes a fragment that never compiled.

So boundaries come from a grammar wherever there is one, definitions are never
cut in half, and where there is no grammar the chunk *says* it was cut on lines
rather than presenting itself as parsed.

## `ChunkMethod` is a correctness signal

Every `Chunk` carries `chunked_by`. `Parsed` means a grammar chose the
boundaries and the span contains whole definitions. `Lines` means it does not
promise that. The field lives on the chunk — rather than being inferred from the
file's language at query time — because the two are not the same question: a
`.rs` file whose definition exceeded the size ceiling is line-split too, and
retrieval that treated it as parsed would present a truncated body with full
confidence. A document missing the field reads back as `Lines`, the weaker
claim.

## How `tree.rs` chooses spans

Not "walk the tree and emit every function" — that loses the text between
definitions, which is where the imports, module comments and constants live.
Instead the grammar places **cut points** and the file is tiled between them:
every byte lands in some chunk and no cut lands inside a body.

- A cut point moves backwards over the comment lines directly above a
  definition, so a doc comment travels with what it documents.
- Only *small* segments merge — under a quarter of the target size. Packing two
  whole functions into one chunk because they happen to fit costs the chunk its
  name and blunts hits on both.
- A container (`impl`, class, module) larger than the target is opened up and
  its members become cut points too. `CONTAINERS` is a closed list, and that is
  the load-bearing decision in the file: adding a function or a block to it
  would permit a cut inside a body.
- A definition past `max_chars` is line-split and labelled `Lines`, because an
  embedder silently truncates its input and a "parsed" chunk whose tail was
  never embedded is a lie.

A chunk is named only when one definition dominates it — more than half its
bytes. A chunk that merged three equal functions is not the first one.

## Languages

Symbol-aware: Rust, TypeScript, TSX, JavaScript (and JSX), Python, Go. TSX is
separate from TypeScript because tree-sitter ships two grammars and the
TypeScript one fails on the first JSX element.

Everything else on the extension allowlist — Markdown, SQL, Java, YAML and the
rest of `EXTRA_EXTENSIONS` — is indexed through the line splitter, labelled with
its extension and marked `Lines`. Indexed-and-honest beats invisible.

## Skips are reported, never silent

A chunker that quietly drops everything over a size cap makes a large
hand-written service file invisible to review permanently, and nobody can tell
the difference between "there was nothing to say" and "we never looked". So a
skip is a `SkippedFile` value with the numbers behind it, and
`Selection::report()` renders the ones a human should see.

The split is deliberate: an ignore glob and an unsupported extension are the
operator's own stated policy and reporting them is noise. A file that was *meant*
to be indexed and was not — too large, unreadable, not UTF-8 — is always
reported. The default cap is 1 MiB, well above hand-written source and well
below a minified bundle.

`Selector` builds its matcher with `ignore_globs`, which `app::review` also
uses: `paths.ignore` must mean the same thing to the indexer and to the lanes.

## Feature gating

`treesitter` is optional but **on by default**, unlike every other optional
dependency in the crate. The rule the others follow is "anything that needs the
network goes behind a feature"; a grammar parses a string in-process. Leaving it
off by default would mean the default `cargo test` exercised only the fallback
splitter, and the property that matters most would go untested in the build
everyone runs. With the feature off everything still works, on lines, and says
so.
