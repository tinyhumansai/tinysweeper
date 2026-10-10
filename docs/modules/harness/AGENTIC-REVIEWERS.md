# Opt-in repository exploration

`models.agentic_reviewers = true` lets council reviewers explore a borrowed
read-only repository through OpenHuman Embed agents. It remains **false by
default** until scripted evaluations and credentialed real-PR comparisons
establish finding, cost and latency parity. Enable `models.structured_output =
"schema"` alongside it; configuration validation rejects JSON-object mode.
Authenticated agent routes require HTTPS or a loopback HTTP endpoint. The
repository's Docker-host HTTP gateway needs a TLS proxy before agentic rollout.

One-shot calls, preview captions, MockModel and Cassette retain the completion
port. An absent tree or disabled lookup policy also uses completion. The default
`Model::review` delegates to `complete`, so offline fixtures remain unchanged.

The runner skips automatic definition pre-bundling and its JSON lookup loop
for an agentic council turn. Embed owns the inference/tool loop. TinySweeper owns the reviewer prompts, the five repository tools, their argument
validation and untrusted-data envelopes. It drives a bounded channel of repository
queries alongside the neutral Embed turn; the tree
stays borrowed and never enters a static tool object or detached task.

## Permissions and bounds

Each call registers a unique agent on one lazily initialized shared runtime,
with `Access::readonly()`, `ToolScopeSpec::HostOnly`, untrusted input enabled,
strict structured output, a required tool call and no allowed sub-agent IDs.
Only `repo_read`, `repo_search`, `repo_list`, `repo_lookup` and `repo_git_show`
are advertised. Listing and literal symbol lookup are supported by directory,
mock and forge snapshots. Forge history reads use full immutable commit IDs;
directory history is available only for its recorded current revision. Hosts
without a requested capability return an explicit unavailable result. No shell, network,
workspace write, MCP, memory or delegation tool is exposed. Completion requires
at least one successful supported repository lookup; denied tools do not count.

The existing lookup policy limits each reviewer to `rounds * per_round` host
queries and `max_chars` of scrubbed source across primary, fallback and unpinned
attempts. Failed or cancelled reads also consume the query allowance. Reads retain the 200-line port cap,
search and symbol results retain the 30-hit cap, and listings allow 200 paths.
All query attempts consume the host query allowance, including unavailable ones.
Sensitive paths, scanner-shaped credentials and PEM bodies are scrubbed before
character bounds and before TinySweeper wraps results in its untrusted-data envelope.
Host errors use generic messages so provider or repository diagnostics cannot
leak secrets into prompts.

Redacted, bounded source is carried in `Answer.looked_up`, including into the
existing one-level question/answer follow-up and falsification stages. Capture
keeps complete fences even when its character ceiling truncates source.

## Lifetime and accounting

Model turns run on shared Tokio workers with 20 MiB stacks. Completed calls
remove and purge their agents. Cancellation aborts the turn, drops the borrowed
host dispatcher, and schedules agent removal/purge on the shared workers.
Repository reads already executing on cancellation are dropped with the caller.
Each turn has a 60-second deadline and forwards the optional host observer;
Embed emits metadata-only observations by default.

`Model::scoped_budget` lets live adapters create a fresh lane ledger. The lane
capability scopes once from the host's model and replaces that scope when a lane
share is applied. Direct positioning/falsification calls receive that same
scoped model. The agent helper accepts the adapter's ledger and per-call
reservation bounds; it creates no process-wide spending budget.

Budgeted gateway aliases need explicit `models.budget_prices.<alias>` input,
cached and output rates in dollars per million tokens. Output caps must be
positive. The checked-in gateway configuration supplies conservative bounds
for its current `flash` and `deep` routes and caps the formerly uncapped deep
route at 16,000 tokens. Operators must reverify every reachable seller and
long-context tier when changing routes or prices; admission bounds cannot
constrain a provider's eventual bill.

Failed attempts retain safe billed usage, even when no reviewer returns a valid
answer. The lane spend tally and failed answer include these totals. Unknown
failures use an isolated attempt ledger, so concurrent reviewers cannot inflate
one another's accounting; unmetered failures retain a bounded estimate.

The response attributes usage to the provider's reported answering model.
Missing cost uses the existing model-price estimate and the conservative
unknown-model ceiling; missing model or usage is an error. Scripted gateway
checks exercise tool advertisement/refusal, redaction, strict schemas, model
attribution, spending and agent cleanup without provider credentials.

## Ownership

OpenHuman Embed supplies application-neutral agents, host-tool registration,
structured answers, routing, cancellation, observers and spending ledgers. It
contains no TinySweeper repository tools or review schema.

`src/harness/agentic/repository/` owns the `RepositoryHost`/`RepositoryQuery`
contract, tool schemas, lexical validation, redaction and result envelopes.
`bridge.rs` binds these tools to borrowed snapshots and the reviewer lookup
allowance. See [repository tools](REPOSITORY-TOOLS.md) for the local contract.
