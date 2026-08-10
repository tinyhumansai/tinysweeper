# `src/sentry` — Sentry promotion

Unresolved Sentry issues become tracked GitHub issues, deduplicated, scrubbed,
and linked back. Four steps, in one order, with the redaction boundary in front
of all of them.

## The one-sentence rule

> tinysweeper promotes a Sentry issue only when `sentry.enabled` is on, the
> project has a `[[sentry.route]]`, the issue clears `min_events` and
> `min_users`, its culprit matches no `ignore_culprits` glob, no GitHub issue
> already carries its marker, and the run has not yet hit `max_per_run` — and
> what it writes is the allow-listed field set and nothing else.

## Files

| File | Responsibility |
| --- | --- |
| `types.rs` | The payload structs. **These are the allow-list** — see below. |
| `pii.rs` | Personal-data shapes the credential scrubber does not know |
| `redact.rs` | The boundary: allow-list → scrub → size cap. The only constructor of `SafeIssue` |
| `select.rs` | `min_events` / `min_users` / `ignore_culprits`, and the cap |
| `dedupe.rs` | The marker, and the GitHub search that decides |
| `promote.rs` | Title, body, labels, priority, issue type |
| `link.rs` | Annotating Sentry, and resolving it when GitHub closes |
| `sweep.rs` | The orchestration |
| `mock.rs` | `MockSentry`, the offline implementation |
| `client.rs` | The HTTP adapter. Behind the `sentry` feature |

The port is `src/ports/sentry.rs`.

## The PII boundary

**A Sentry event is the most dangerous input this product will ever handle.**
Everything else tinysweeper reads is source code and diffs, written to be
published. An event payload is captured from a running production process:
request bodies, headers, cookies, query strings, session ids, user emails,
environment variables, and local variable values in every stack frame.
Promoting one into a GitHub issue moves that from an access-controlled tool
into a tracker with a much wider audience — and **GitHub keeps the edit history
of an issue body**, so a leak cannot be fixed by editing afterwards.

Three layers, each of which can only ever remove:

### 1. The parse is the allow-list

`types.rs` declares only promotable fields, so `request.data`,
`request.cookies`, `request.headers`, `user`, `contexts`, `extra`, breadcrumb
payloads and frame-local `vars` are **never deserialized**. There is no code
path that could promote them, which is stronger than one that chooses not to.

This is an allow-list rather than a deny-list because a deny-list fails *open*
on every field Sentry adds after we write it, and Sentry adds fields. For the
same reason none of these structs carry `deny_unknown_fields`: ignoring the
unknown is the desired behaviour, and a strict parse would turn every schema
addition into a hard failure whose only fix is to relax the parse.

`RawFrame` is the one to read: it has `filename`, `function` and `lineno`, and
deliberately no `vars`, `context_line`, `pre_context` or `post_context`. A
stack trace worth promoting is file, function and line.

### 2. Scrubbing, before the text is used for anything

`redact::scrub_text` runs, in order:

1. `scan::secrets::scrub` — the shared credential rulepack.
2. `pii::redact` — email addresses, `Bearer` tokens, card-shaped digit runs and
   long opaque tokens.
3. `sentry.scrub_patterns` — case-insensitive literal substrings.

Neither built-in pass takes a flag, so **there is no configuration that reaches
this function with them off**. `scrub_patterns` layers on top and can only
tighten the result — the failure mode being a config that looks like hardening
and is a hole. `scrub_patterns_cannot_disable_the_built_in_scrubbing` pins it.

Scrubbing happens at *projection*, not at posting. By the time anything is
posted it has already been hashed into a marker, traced, and buffered, so
scrubbing later would be scrubbing the last copy of several.

#### Why `pii.rs` exists separately

`scan::secrets::scrub` is a **credential** rulepack: it matches known prefixes
(`AKIA`, `ghp_`, `sk-proj-`, …) and nothing else. It does not know what an
email address, a session id or a card number looks like — verified by
`the_credential_scrubber_does_not_catch_personal_data`, which fails if that
ever changes and the duplication can be removed.

For every other input tinysweeper reads that is fine. For the one allow-listed
free-text field here — the exception message — it is not, because that is
exactly where an application puts `invalid card 4111111111111111 for
alice@example.com`.

It is local to `sentry` rather than added to the shared scrubber because
extending `scan::secrets` would change what every review comment and check-run
summary renders across the whole product. That is a broader behaviour change
with its own argument to make.

Every rule fails safe — no Luhn check on the card rule, no entropy threshold on
the opaque-token rule. Both would be more precise and both fail *open* on the
input they get wrong. An over-redacted message is mildly less useful; an
under-redacted one is permanent.

### 3. A hard size cap

`MAX_EXCERPT_BYTES` is 4 KB across every text field and every frame *together*
— a per-field limit multiplied by an unbounded frame count is not a limit.
Frames are sacrificed before the exception message: a promotion that has lost
its stack trace is still actionable, one that has lost the error text is not.

### The tests that keep it honest

- `no_secret_in_the_fixture_survives_into_the_promotion` — one realistic event
  carrying an email, a bearer token, a session cookie, a card-shaped number, a
  populated `vars` block and an AWS key, in every section that can hold them.
  Matched **literally**, not by regex: a regex assertion tests the same idea
  twice and passes when both copies are wrong the same way.
- `promoted_field_set_equals_the_allow_list` — the structural half, and the one
  that matters as the code changes. A test asserting "no email appears" passes
  forever while a *new* field quietly starts carrying one.
- `frames_carry_only_file_function_and_line`.
- `the_report_carries_no_unscrubbed_text` — the spec's "tracing must never
  receive an unscrubbed event" rule, as far as a test can reach it.

## The four steps

```text
route ─▶ fetch ─▶ filter ─▶ dedupe ─▶ cap ─▶ redact ─▶ promote ─▶ link
```

### Why dedupe sits between filter and cap

The ordering decision worth reading twice. If the cap ran before the GitHub
search, ten already-tracked issues would consume the entire `max_per_run`
budget and the sweep would promote nothing — while reporting a truncation,
which reads as "there was more to do" when everything was already done. Running
dedupe first means the cap counts issues that would actually be *created*.
`the_cap_counts_promotable_issues_not_tracked_ones` pins it.

### Deduplication: GitHub is the source of truth

```text
<!-- tinysweeper:sentry=<org>/<project>/<short-id> -->
```

Stored in the GitHub issue body, the way review findings already carry
`<!-- tinysweeper:fp=… -->`. "Is this already tracked?" is then a question asked
of the system that actually holds the answer. **A cache would be an
optimisation on top and must never be the source of truth**: if the two
disagree GitHub is right, and a lost cache must degrade to a slower sweep
rather than a duplicate one. There is deliberately no cache in this module for
that reason.

A Sentry issue promoted twice is the worst outcome available, because it is the
one that scales: every subsequent sweep adds another copy.

Search **narrows**, exact match **decides**. `find_tracked` searches for the
short id, then confirms by looking for the exact marker substring in the
returned body — GitHub's issue search is a tokenised index over an
eventually-consistent store, and an HTML comment is not reliably searchable as
one phrase. The consequence: a search miss produces a duplicate, a hit that
fails the substring check merely produces a promotion. The expensive failure is
guarded by the cheaper check.

Closed issues count as tracked. Otherwise every sweep after a fix reopens the
same report.

An issue with no short id yields `Tracked::Undedupable` and is **refused**, not
promoted: its marker would name nothing, so it could never match on a later
sweep and would be recreated forever.

### Routing

```toml
[[sentry.route]]
project = "api"
repo    = "tinyhumansai/backend"
labels  = ["area: sentry"]
```

`sentry.projects` says what to sweep; `sentry.route` says where it goes. **A
project with no route is skipped and logged, never guessed** — inferring a
repository from a project slug is exactly the kind of plausible reasoning that
opens issues in someone else's tracker. `tinysweeper doctor` prints the routing
table with `NO ROUTE` against any project that has none, because a project
sweeping into nowhere is otherwise invisible.

Route labels are **additive** to `sentry.labels`, so a per-route list that
forgets `sentry` cannot drop it.

`[sentry]` is on the not-overridable list in `config::remote` and must stay
there: it names a token environment variable and it writes to GitHub. Routing
is the deployment's decision, never the reviewed repository's.

### Promotion

No model call — the spec forbids one on raw event text, and there is no useful
one to make on scrubbed text either. Priority follows **Sentry's own `level`
and nothing else**:

| Level | Priority |
| --- | --- |
| `fatal` | P0 |
| `error` | P1 |
| `warning` | P2 |
| anything else, or absent | P3 |

Deriving priority from event or user counts instead would invent a threshold
nobody chose and would make a widespread warning outrank a rare crash.

The issue type is set through `issues::kind::plan` — the issue-triage decision
rules, reused rather than restated — with the classification fixed to `bug`,
which a Sentry error is by construction. Every refusal in that function (the
owner defines no types, no type matches, the feature is off) applies unchanged,
and none of them fails the promotion: an untyped tracked issue beats a lost one.

Labels are applied directly rather than through `issues::labels::plan`. That
planner bounds *model-suggested* labels on somebody else's issue; `max_labels`
is a noise budget for suggestions. These are deterministic configuration on an
issue tinysweeper is opening itself, and running them through the budget could
drop the `sentry` label the operator configured.

Untrusted text is fenced with a delimiter sized longer than the longest
backtick run inside it, and table cells escape `|`. An exception message can
contain anything.

### Closing the loop

`annotate_sentry` comments the GitHub URL onto the Sentry issue.
`resolve_when_tracked` resolves the Sentry issue **once the tracking GitHub
issue is closed**.

That second one is a spec ambiguity resolved deliberately. The config field's
own doc comment says "resolve … once it is tracked", which would resolve at
promotion time; #90's step 4 and #91's acceptance criterion say "when the
GitHub issue is closed". This implements the second, because resolving at
promotion time would mark an error fixed the moment somebody noticed it — the
Sentry issue would leave the unresolved list while the bug is still in
production. The conservative reading is also the reversible one.

**Nothing here closes a GitHub issue.** Out of scope per #90: closing someone's
issue is the most expensive mistake this bot can make, and a Sentry event count
is weaker evidence than `issues.close` already demands. Traffic is one-way —
GitHub's state drives Sentry, never the reverse.

## Degradation

| Failure | Behaviour |
| --- | --- |
| A project has no route | Skipped, warned, recorded in `SweepReport::unrouted` |
| A route's `repo` is not `owner/name` | Refused, recorded in `failed` |
| Sentry is unreachable for one project | Recorded in `failed`; other projects continue |
| The latest event is missing or expired | Promoted without frames |
| Annotation fails | Logged; the promotion stands (the marker is the durable half) |
| A cap truncates | Reported at `info`, on stdout, and in `truncated` |

One unreachable project must not take the other five down with it, and a
partial sweep is reported as partial — `failed` is what lets a caller tell
"nothing qualified" apart from "we never looked".

## Configuration

Everything is under the pre-existing `[sentry]` section, plus the new
`[[sentry.route]]` table. `sentry.enabled` is `false` by default.

`config::validate` rejects a route with an empty project, a `repo` that is not
`owner/name`, a duplicate project, or a route naming a project not in
`sentry.projects` (a dead route, almost always a typo in one of the two lists).
It does **not** reject a project with no route — that is a loud runtime skip, so
that adding a project and routing it can be two separate changes.

## Running it

```sh
tinysweeper sentry --dry-run   # decide and report; write nothing
tinysweeper sentry             # promote
```

The dry run takes the same code path as a live run and simply does not call the
write handle, so its report is what would have happened rather than a parallel
implementation that might disagree.

Needs the `sentry` feature (to read Sentry) and `github` (to open issues). The
subcommand is declared unconditionally so runbooks can be written against a
stable surface; a build without either feature says which one is missing.
