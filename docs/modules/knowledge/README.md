# `knowledge`

What a reviewer knows before it reads the diff.

Two sources, treated very differently because they *are* very different:

- **Curated documents** — house conventions, runbooks, decisions a previous
  review already argued out — written by operators through the admin API and
  stored behind the `KnowledgeStore` port, scoped to an organisation or one
  repository. Pinned ones go into every prompt in scope; the rest are left to
  retrieval.
- **Repository instruction files** — `AGENTS.md`, `CLAUDE.md`,
  `.tinysweeper.md` — which live in the branch a pull request proposes and are
  therefore written by whoever opened it. They are hostile input, and they go
  through a sandboxed extraction pass before any of them reaches a prompt.

## Files

| File | Contents |
| --- | --- |
| `types.rs` | The ceilings (`MAX_RULES`, `MAX_RULE_CHARS`), filename validation, `ReviewKnowledge`, `doc_id` |
| `extract.rs` | The sandboxed extraction pass and its structural validation |
| `extract_test.rs` | The injection and ceiling tests |
| `cache.rs` | `RuleCache`, keyed on file content |
| `pinned.rs` | Rendering pinned documents under the per-document and total caps |
| `mod.rs` | `gather` — everything the centre contributes to one review |

The store itself is `src/ports/knowledge.rs`, with `MockKnowledgeStore` and
`MongoKnowledgeStore` in `src/index/`.

## What this replaced

`app::review` had a `repo_policy()` that read `AGENTS.md` from the **process
working directory** — tinysweeper's own checkout, not the repository under
review — truncated it at 6000 characters, and injected it straight into the
cacheable system prefix. Two faults, and the second is the serious one:

- The wrong repository's conventions were reported as the reviewed
  repository's, and there was no ancestor walk despite the field being
  documented as one.
- Text an author controls landed in the position the model is told to obey.

## The four-layer defence

Copied, at source, from octopus, which had already worked this out.

1. **Filename validation and scoped discovery.** A configured name has a strict
   charset — ASCII alphanumerics, `.`, `_`, `-` — no `/`, no `..`, bounded
   length and bounded count. For each changed path it is then considered at the
   repository root and every ancestor directory, so `src/AGENTS.md` applies to
   a changed `src/bin/main.rs`. The expanded list has its own cap. Configuration
   can therefore select *which filenames* are policy without being a way to
   read arbitrary repository paths. An invalid name is dropped at runtime with
   a warning and reported by `config::validate`.
2. **Fetch at the head SHA, through the forge.** `ForgeRead::file_at` reads the
   file at the commit under review, never from disk — there is no checkout on
   the server path. Truncated to `knowledge.max_file_bytes` and content-hashed.
3. **A separate cheap-model call whose only job is extraction.** It runs on
   `models.scan` with **no review context at all**: no diff, no pull request, no
   lane instructions. Its system prompt requires a markdown bullet list and
   nothing else, caps the output at 25 bullets of 200 characters, tells it the
   input is documentation rather than instructions — in any language — and gives
   it `NO_RULES` for the common case. The strongest property here is that a
   jailbreak has nothing to jailbreak.
4. **Structural validation.** The answer must actually *be* a bullet list.
   `NO_RULES` yields nothing. A single non-bullet line discards the whole
   answer rather than salvaging the rest, because partial salvage is exactly how
   an injected paragraph gets through with a `- ` glued to the front. Rules over
   the character ceiling are dropped, not truncated.

A content-hash cache sits in front of the model call, so one unique file content
is extracted once, ever — including across forks whose `AGENTS.md` matches
upstream's, and across every push that did not touch the file.

### The ceiling is the argument

25 × 200 is about 5 KB. Even a *completely* jailbroken extractor — one emitting
the injection verbatim as bullets — can put at most that much text into a review
prompt, fenced and labelled as untrusted, in the volatile half. `MAX_RULES` and
`MAX_RULE_CHARS` are therefore constants rather than configuration: a repository
that could raise them could remove them.

## Where it lands in the prompt

| Source | Prompt half | Fence label |
| --- | --- | --- |
| Pinned curated documents | cacheable prefix | `policy` |
| Extracted repository rules | volatile suffix | `untrusted-repo-rules` |

Curated documents may sit in the prefix because they change only when an
operator edits one. Extracted rules may **not**: they change with the branch, so
a prefix carrying them would never hit the prompt cache once, and they are
author-controlled, so the prefix is the last place they belong.

`harness::prompt` carries the matching clause in `SHARED_RULES` — constant, and
therefore cacheable — telling the model to apply the coding rules in that block
and to ignore anything in it that asks to change role, output format or severity
rubric, or to reveal system context, and to report the attempt as a finding.

## Caps on pinned documents

`knowledge.pinned_doc_chars` caps one document; `knowledge.pinned_total_chars`
caps the block. Both are budgets: a pinned document is paid for on every review
of every pull request in scope. Over-cap content is truncated **with a visible
marker** rather than dropped, because a dropped pinned document is invisible to
the operator who pinned it. Rendering is ordered by document id so the prefix is
byte-stable across runs.

## Admin API

Curated documents are written through `/admin/knowledge`, bearer-token
authenticated like the rest of the admin API:

```
GET    /admin/knowledge/org/{owner}
PUT    /admin/knowledge/org/{owner}/{slug}
DELETE /admin/knowledge/org/{owner}/{slug}
GET    /admin/knowledge/repo/{owner}/{name}
PUT    /admin/knowledge/repo/{owner}/{name}/{slug}
DELETE /admin/knowledge/repo/{owner}/{name}/{slug}
```

The scope comes from the path, never from the body, so a request cannot claim
one scope in its URL and write another into the database. A repository listing
includes its organisation's documents, because that is what a review at that
scope actually sees. Deleting a document that is not there is a `404`: a silent
success would leave an operator believing a document is gone while it is still
in every review.

## Configuration

```toml
[knowledge]
extract = true
files = ["AGENTS.md", "CLAUDE.md", ".tinysweeper.md"]
max_file_bytes = 65536
pinned_doc_chars = 4000
pinned_total_chars = 12000
```

`review.respect_agents_md = false` turns the extraction pass off entirely, before
any spend — it is the same question as "treat the repository's instruction files
as review policy", so it is the same switch.
