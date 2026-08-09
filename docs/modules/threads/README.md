# `src/threads` — closing our own review conversations

Every thread tinysweeper opens has to be resolved by hand today. This module
closes the ones that are demonstrably settled, and refuses everything else.

## The deterministic rule

A thread is resolved when **all** of these hold:

1. It is not already resolved.
2. Its **first** comment is ours — exact login match through
   `findings::prior::is_own_login`. A prefix match would count
   `tinysweeper-anything` as ourselves, and anyone can register that account.
3. That comment carries a well-formed fingerprint marker
   (`<!-- tinysweeper:fp=… -->`, sixteen lowercase hex characters).
4. A **human** has replied: some comment after the opener whose author is not a
   bot and is not us. Two bots replying to each other is a loop nobody watches.
5. GitHub reports the thread as **outdated** — the code it anchors to changed.
6. The fingerprint is **absent** from the findings this run produced.

Nothing in that list asks a model anything. Steps 5 and 6 together are the
evidence: the code moved and the objection stopped reproducing.

## What the model may and may not decide

When the code did **not** change (step 5 fails), no fingerprint can settle the
thread — the only evidence is the reply itself. That case, and only that case,
reaches `threads::advise::ask`, behind `threads.ask_model`, which **defaults to
off**. Its answer is advisory in the literal sense the security boundary
requires: it can only add an entry to a plan, the plan only ever names threads
tinysweeper itself opened, and the mutation is performed by `apply_plan` from
`src/app/apply.rs` after every model call has returned.

With the flag off, such a thread is simply left for a human.

## Where the volatile content sits

`advise::prompt` returns a `harness::prompt::Prompt`:

- **prefix (system message)** — the instructions, byte-identical for every
  thread on every pull request.
- **suffix (user message)** — the conversation, fenced and labelled
  `untrusted-thread`.

The comment bodies are attacker-controlled: anyone who can reply to a pull
request writes them. They are fenced as data and never placed in the half the
model is told to obey. `advise_test.rs` asserts both properties — a stable
prefix across different threads, and a hostile reply that never reaches it.

The layering is what a provider needs to serve the prefix from cache. Whether it
is actually served that way is a separate question: OpenRouter's Kimi and MiniMax
routes need explicit `cache_control` breakpoints, which `vendor/tinyagents` does
not send today, so the measured hit rate is currently near zero. The structure is
correct and the benefit is pending that upstream change.

## Cost

`threads::plan` returns a `Spend` alongside the plan. `app::review` merges it
into the run's spend **before** the proposal's totals are read, so an advisory
call reaches `Proposal::usage()` and the rendered `findings::render::cost_table`
like any lane's call. A model call whose spend is not merged is invisible money.

## Ports

- `ForgeRead::review_threads` — GraphQL `reviewThreads`, because REST cannot say
  whether a thread is resolved.
- `ForgeWrite::resolve_review_thread` — the `resolveReviewThread` mutation.

Both have offline mock implementations; `MockForge` records every resolve as
`Write::ThreadResolved` and reflects it in its state, so a run that resolved the
same thread twice cannot pass unnoticed.

## Trigger

`pull_request_review_comment` with action `created`, from a non-bot sender, on a
comment that is a **reply** (`in_reply_to_id` present). Any other action would
queue a paid run on every edit; a non-reply comment starts somebody else's
thread, which this module never touches.
