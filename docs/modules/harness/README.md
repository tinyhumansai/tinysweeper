# `harness` — prompts, schemas, and the models that answer them

Lanes never talk to a provider. They hand [`ports::model::Model`] a prompt, a
JSON schema and a token ceiling, and get back a parsed value. This module holds
the two implementations of that port — `MockModel` for tests, `GatewayModel`
behind the `harness` feature for the real thing — plus the prompt assembly and
schema that every lane shares.

## The transport: OpenHuman's `Completer`

`GatewayModel` makes each call through `openhuman_embed::complete::Completer`:
one stateless, structured completion against the configured OpenAI-compatible
`base_url`. Not an OpenHuman agent turn, deliberately. A turn runs a
prompt-injection guard that would reject the adversarial diffs a review exists
to read, can call tools, and may fall back to another model without saying so.
A completion has no tools, no session and no fallback, so "the model never
acts" holds by construction: it only ever answers a schema.

The request bodies this sends are pinned by `harness::parity_test`, which drives
the adapter against a loopback fake gateway (`harness::fake_gateway`) and
compares every body and parsed response with the fixtures in
`src/harness/fixtures/wire/`. Those fixtures were recorded from the tinyagents
adapter this replaced; the only one that changed is `truncation_retry`, for the
reason given below. `TINYSWEEPER_RECORD_WIRE=1` re-records them — review the
diff as a wire-format change.

## Who enforces the schema

`models.structured_output` picks between two ways of getting a structured answer,
and they are not equally strong.

- **`schema`** — the default. The provider is handed the JSON Schema and
  constrains generation to it. A response of the wrong shape is never generated.
- **`json_object`** — the provider guarantees only that the answer is a
  well-formed JSON object. The schema travels in the system prompt instead, via
  `schema::json_mode_instruction`, and `schema::parse` is what rejects a
  mismatch.

Under `json_object` a wrong shape is *caught* rather than *prevented*. That is a
real reduction in strength, and it is still a long way from parsing prose: the
answer is always valid JSON or a hard error, never a best-effort read of English.

The setting exists because the strong form is not universally available.
`deepseek-v4-pro-0813` answers a strict schema request with
`400 — "This response_format type is unavailable now"`, so a deployment that
selects it on `schema` never reaches DeepSeek at all — every call fails and the
fallback chain answers instead, which looks indistinguishable from a healthy
deployment. That was measured: a corpus re-record produced 29 of 29 answers from
the fallback model.

Two halves, one decision. `json_object` mode changes the wire `response_format`
*and* appends the schema to the prompt; `wire_messages` is split out and tested
precisely so the two cannot drift apart. Changing one without the other either
sends a schema nobody reads or asks for a shape nobody described. The instruction
text must also contain the literal word "json" — DeepSeek's JSON mode rejects a
request without it, so a reword that drops the word breaks every call.

The setting applies to the whole chain, fallbacks included: any model in it can
answer a given review, and the prompt is assembled before the answering model is
known.

## The output ceiling, and what happens when an answer hits it

`models.max_tokens` is a ceiling on *generated* tokens, and the hidden reasoning
channel is billed against the same allowance. Two failures follow from that, and
they look nothing alike:

- **Nothing comes back.** The model spends the whole budget thinking and returns
  empty content with `finish_reason = "length"`.
- **Half comes back.** The answer is cut off part way through the findings
  array. Anything that "repairs" the unterminated JSON makes it *parse*, and the
  review reads exactly like one that found fewer things. Quiet, and the one
  worth engineering against.

`GatewayModel::call_until_complete` handles both. The finish reason is checked
before anything is parsed, and a `length` finish is never turned into findings: the call is retried against the same model with a
doubled ceiling, twice, so the last attempt runs at 4x `models.max_tokens`. A
rung that is never reached costs nothing — tokens are billed as produced, so the
ladder is headroom rather than spend. An answer that still does not fit fails the
call with an error naming `models.max_tokens` and reporting how much of the
budget went to reasoning, and only then does the fallback chain take over.

This ladder did not actually run under the tinyagents harness: in `schema` mode
its repair step closed the truncated JSON, schema validation then failed, and
the call errored *before* `finish_reason` was looked at — so a cut-off answer
went straight to the fallback models instead of being retried with more room.
`parity_test::truncation_retries_at_a_doubled_ceiling` pins the fixed
behaviour.

Every call logs its numbers at `info` — input, cached, output and reasoning
tokens, the ceiling, and the finish reason. A call whose reasoning took more than
half the budget also warns: it is one larger diff away from the ladder above.

## Where a call goes to be read afterwards

Two places, for two different questions, and neither is a third log file:

- **Langfuse**, when `LANGFUSE_BASE_URL` / `LANGFUSE_PUBLIC_KEY` /
  `LANGFUSE_SECRET_KEY` are set (see the README), or through the TinyHumans
  proxy with `TINYHUMANS_LANGFUSE_PROXY_URL` / `TINYHUMANS_AUTH_TOKEN`.
  `harness::langfuse` is a `CompletionObserver`: each call is exported as its
  own trace and generation with the prompt, the model's answer, usage and cost,
  which is what answers "what did the model actually see" for a review that has
  already been published. Every rung of the truncation ladder is its own call
  and so its own trace: a retry at a larger ceiling appears as a retry rather
  than overwriting the attempt that was cut off. Export is fire-and-forget and
  never fails a review.
- **Cassettes** (`harness::cassette`), for the eval corpus: a recorded call is
  replayed offline so a scoring rule can be rewritten without paying for the run
  again. Prompts are recorded only when explicitly asked for, because a prompt
  embeds the reviewed repository's diff.

Both carry untrusted pull request text, so both are opt-in and belong in a
server's secret environment rather than a checkout.

## Cost

Every request asks OpenRouter for the cost it charged (`usage: {include: true}`),
and `Usage::cost_usd` is that figure when it comes back. It is read out of the
raw response body, because the OpenAI wire shape has no cost field — this one
is OpenRouter's extension (and Surplus's `buyer_cost_micro`, read first). A negative or non-numeric figure is
disbelieved.

`pricing.rs` is the fallback for a gateway that reports nothing: a table of
per-million-token rates verified against the provider. It is an estimate that
drifts every time a provider reprices, which is why it is no longer what
`models.budget_usd_per_pr` — a hard stop on a real bill — is enforced against.
An unknown model warns rather than silently pricing at zero.

Reasoning tokens are *not* separately priced: OpenRouter bills them as output
tokens and reports them inside `output_tokens`, so they are already in the cost.
They are logged on their own because they are what the output ceiling is
competing for.

## Prompt layering

See [`harness::prompt`] for the layering that keeps a re-review cheap: the
prefix is byte-identical across runs so the provider's cache serves it, and only
the suffix carries new evidence. Anything that reformats the prefix costs a cache
miss on every call.

## Severity is decided by a rubric, not by taste

`SHARED_RULES` carries a four-level rubric and the schema's `severity` field
points at it. Both are load-bearing. Severity is not derived from anything the
model can check, so an enum with no rubric behind it means each run re-guesses
from scratch: one unchanged finding on `tinysweeper#89` was reported medium,
then high, then critical, then high across four pushes, and on `tinymemory#13`
the wobble flipped the verdict from changes-requested to approved with nothing
between the two but a re-review.

Two rules do the work. Severity is the **consequence** if the change ships, not
the topic and not how sure the model is — confidence already carries that. And a
finding raised before keeps the level it was given: prompt layer 5 lists earlier
findings as `severity — title` precisely so that instruction has something to
refer to.

Asking is necessary and not sufficient, so `app::review` pins the level of any
finding whose title it has seen before, ahead of the check-run conclusion and the
request-changes verdict. The prompt makes the model's own answer stable; the pin
makes the *verdict* stable whatever the model answers.
