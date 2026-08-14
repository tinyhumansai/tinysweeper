# `src/threads` — closing our own review conversations

Every thread tinysweeper opens has to be resolved by hand today. This module
closes the ones that are demonstrably settled, and refuses everything else.

## The deterministic rule

A thread is resolved when **all** of these hold:

1. It is not already resolved.
2. Its **first** comment is ours — exact login match through
   `findings::prior::is_own_login`. A prefix match would count
   `tinysweeper-anything` as ourselves, and anyone can register that account.
3. That comment has the renderer-owned finding title (`**title**`) that the
   review agent receives as prior context.
4. GitHub reports the thread as **outdated** — the code it anchors to changed.
5. The review agent explicitly reports that exact prior title as fixed after
   examining the new diff.

Steps 4 and 5 are the evidence: the code moved, and the review agent examined
the updated code and said that this particular objection no longer reproduces.
A new commit therefore closes a fixed finding without requiring the author to
also reply to the old comment. The agent verdict is advisory; deterministic
code verifies ownership and the outdated state before the write side acts.

## What the model may and may not decide

When the code did **not** change (step 4 fails), the new diff cannot settle the
thread — the only evidence is a **human** reply. That case, and only that case,
reaches `threads::advise::ask`, behind `threads.ask_model`, which **defaults to
off**. Bot-only replies never reach the model. Its answer is advisory in the
literal sense the security boundary requires: it can only add an entry to a
plan, the plan only ever names threads tinysweeper itself opened, and the
mutation is performed by `apply_plan` from `src/app/apply.rs` after every model
call has returned.

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

## Saying why, and naming the commit

A resolve used to happen silently. GitHub indexed that as resolved and the
author read it as the bot losing interest: the objection was tinysweeper's, so
the retraction should be too, and it should name the commit it is crediting.

`apply_plan` therefore posts `threads::resolution_note` into the thread before
resolving it. The note carries the deterministic reason and the abbreviated head
SHA of the reviewed commit — the same SHA `app::apply` has already checked
against live state, so a note cannot credit a commit nobody is looking at:

> **Resolved** — the review agent found this finding fixed in the new code, as
> of `abc1234`.

Both halves of that string are crate-owned. `reason` is a `&'static str` from
`Decision`, and the SHA is read off the forge, so nothing a contributor writes
reaches the rendered comment.

The note comes **first** and its failure does not stop the resolve. Both
orderings lose something when the second call fails; this one loses the
explanation for a thread that did close, rather than leaving a thread open
underneath a comment announcing it was resolved.

`threads.comment_on_resolve` (default on) turns the note off without turning off
the resolving — deliberately two behaviours behind one switch would mean
silencing the noise silently disabled the feature.

## Ports

- `ForgeRead::review_threads` — GraphQL `reviewThreads`, because REST cannot say
  whether a thread is resolved.
- `ForgeWrite::reply_to_review_thread` — the `addPullRequestReviewThreadReply`
  mutation. GraphQL rather than REST's `pulls/comments/{id}/replies` because the
  thread is already identified by the node id the resolve takes, and REST would
  mean carrying a second identifier for the same thread.
- `ForgeWrite::resolve_review_thread` — the `resolveReviewThread` mutation.

All three have offline mock implementations; `MockForge` records every reply as
`Write::ThreadReply` and every resolve as `Write::ThreadResolved`, reflecting the
latter in its state, so a run that resolved the same thread twice cannot pass
unnoticed.

## Trigger

`pull_request_review_comment` with action `created`, from a non-bot sender, on a
comment that is a **reply** (`in_reply_to_id` present). Any other action would
queue a paid run on every edit; a non-reply comment starts somebody else's
thread, which this module never touches.
