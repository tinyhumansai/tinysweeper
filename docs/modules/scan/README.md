# `src/scan`

Deterministic scanners. They run before any model call — cheap, certain, and
offline — so a committed private key fails for free and the model is only ever
asked to adjudicate what a scanner already flagged. See `docs/modules/lanes`
for how a lane republishes a scanner's findings and hands them to the model as
evidence.

| module | what it finds |
|---|---|
| `secrets.rs` | credential shapes: a rulepack of known vendor prefixes, an entropy heuristic for secret-looking assignments |
| `blobs.rs` | committed junk: build output, dependency trees, oversized or opaque files |
| `workflows.rs` | a GitHub Actions workflow made more permissive or less pinned |
| `paths.rs` | [`is_sensitive_path`] — whether a path is a secret by convention, regardless of content |
| `types.rs` | `Finding`, `ScanKind`, and [`redact`] — the shared token format |

## Findings never carry the value

A [`Finding`] has nowhere to put the matched text — see `types.rs`'s doc
comment. What it carries instead is a `redacted_hint`: a known vendor prefix,
when there is one, and a length. That is enough for a human to recognise which
credential to rotate, and nothing more.

## Redaction

Two matchers apply the same redaction, at two different moments:

- [`scan::secrets::redact_line`] (the `scrub` a model's *output* has always
  gone through) replaces a recognised credential in a string with
  [`redact`]'s token. It matches on **shape** — the rulepack — so it also
  catches a secret a model quotes back into a summary.
- [`crate::evidence::redact::mask`] runs on the **parsed diff**, before any
  request is built — see `docs/modules/lanes` and the module doc on
  `src/evidence/redact.rs`. It uses `redact_line` for a line a scanner
  flagged, and additionally masks every added and removed line of a path
  [`is_sensitive_path`] names — `.env`, a private key — because a scanner
  looking at shape can miss a credential that does not look like the ones it
  knows, and a file's *path* is sometimes the only reliable signal.

Both use [`redact`]'s token format: a known vendor prefix and a length, never
the value. A human or a model reading either sees one vocabulary for "a secret
was here" — see `harness::prompt::SHARED_RULES`, which tells a reviewer what
the marker means and that the line is still present.

The two are independent on purpose. `redact_line`/`scrub` is the second line
of defence — it still runs on model output, because a lane can compose new
text from evidence that was itself masked, quoting a hint or restating a
title — but by the time a request is built, `mask` is the reason there is
nothing left in the diff for it to find.
