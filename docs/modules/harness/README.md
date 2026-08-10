# `harness` — prompts, schemas, and the models that answer them

Lanes never talk to a provider. They hand [`ports::model::Model`] a prompt, a
JSON schema and a token ceiling, and get back a parsed value. This module holds
the two implementations of that port — `MockModel` for tests, `GatewayModel`
behind the `harness` feature for the real thing — plus the prompt assembly and
schema that every lane shares.

## The output ceiling, and what happens when an answer hits it

`models.max_tokens` is a ceiling on *generated* tokens, and the hidden reasoning
channel is billed against the same allowance. Two failures follow from that, and
they look nothing alike:

- **Nothing comes back.** The model spends the whole budget thinking and returns
  empty content with `finish_reason = "length"`. tinyagents retries this on its
  own with a larger budget, and if that fails the call errors. Loud.
- **Half comes back.** The answer is cut off part way through the findings
  array. tinyagents' repair ladder closes the unterminated JSON, so it *parses*,
  and the review reads exactly like one that found fewer things. Quiet, and the
  one worth engineering against.

`GatewayModel::call_until_complete` handles the second case. A `length` finish
is never turned into findings: the call is retried against the same model with a
doubled ceiling, twice, so the last attempt runs at 4x `models.max_tokens`. A
rung that is never reached costs nothing — tokens are billed as produced, so the
ladder is headroom rather than spend. An answer that still does not fit fails the
call with an error naming `models.max_tokens` and reporting how much of the
budget went to reasoning, and only then does the fallback chain take over.

Every call logs its numbers at `info` — input, cached, output and reasoning
tokens, the ceiling, and the finish reason. A call whose reasoning took more than
half the budget also warns: it is one larger diff away from the ladder above.

## Prompt tracing

`TINYSWEEPER_PROMPT_LOG=<dir>` writes one JSON line per model call to
`<dir>/prompts-<pid>.jsonl`: the messages as sent, the raw text, the parsed
structured output, the finish reason and the usage. It is what makes a bad
review reproducible after the process has exited.

**Off unless the variable is set.** A traced line contains the pull request's
diff and body verbatim — untrusted input, which may itself contain a credential
the contributor committed. That is fine in a checkout and is not fine on a shared
host, so enabling it is an operator's decision. The API key is never written: the
trace is built from the request and the response, and the key lives on
`GatewayModel`, whose `Debug` redacts it.

## Cost

Every request asks OpenRouter for the cost it charged (`usage: {include: true}`),
and `Usage::cost_usd` is that figure when it comes back. It is read out of the
raw response body, because the OpenAI wire shape tinyagents parses has no cost
field — this one is OpenRouter's extension. A negative or non-numeric figure is
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
