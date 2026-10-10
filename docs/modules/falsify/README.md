# `falsify`

One cheap call per lane that removes findings the diff **disproves**.

## Falsify, do not verify

The obvious design is a second opinion: hand the findings to another model and
ask whether they are correct. It does not work, and it fails in a way that looks
like success.

A lane's model can gather more context than a single prompt shows. A verifier
that sees less than the reviewer did cannot confirm the findings that needed
that context — which are the good ones, the observations a careful human would
have missed. Ask it "are these correct?" and it rejects everything it cannot
confirm, so the pass quietly deletes the best half of the review and leaves the
shallow findings behind.

So the prompt is asymmetric, and the asymmetry is the entire mechanism. The
filter is told, in these terms:

- these findings come from an agent that could gather more context than you can
  see;
- your task is **not** to verify them;
- reject only those you can confirm are **incorrect from the diff alone**;
- anything you cannot determine, let pass — even if it looks suspicious.

Uncertainty is not grounds for rejection. Only proof is. If the filter is
weighing a finding up, it cannot prove it wrong, which means it keeps it.

Rewriting that prompt into "check each finding" is the one change to this module
that no test other than the prompt assertion would catch, and it would silently
gut the review.

## Security scope

A true generic request for behavior tests is not a security vulnerability. The
security filter therefore reports scope separately from factual `incorrect`:
`in_scope`, `out_of_scope`, or `uncertain`, with the attacker-controlled input,
dangerous operation or trust boundary, security impact, and reason the finding
actually claims. It does not invent an exploit path or require verification of
a vulnerability whose context is outside the supplied evidence.

Only a unique, valid `out_of_scope` assessment with a nonempty reason and all
three attack-chain fields explicitly empty removes an observation. Missing,
duplicate, uncertain, contradictory, or malformed assessments keep it. A test
request exercising an identified exploit remains in scope. The critique and
commits request protocols remain unchanged; scanner facts never enter this call.

## Two hard properties

**It rejects only.** Factual falsification returns indices with a reason each.
Security adds a separate indexed scope assessment in the same call. There is no channel through which a finding can come back
altered, re-scored, merged or invented. A filter that can rewrite what it
filters is a second reviewer nobody gated.

**It fails open.** A model error, a timeout, or an answer that does not parse
means every finding survives, recorded in the outcome as `failed_open`. A noise
filter that can silence a review by breaking is worse than no noise filter, and
a provider outage must not look like a clean review.

Both properties are asserted directly in `src/falsify/test.rs`.

## No deterministic pass in front of the model

An earlier version ran a textual pre-pass that dropped "will not compile" or
"is not defined" claims whenever the evidence contained a definition keyword
followed by the symbol's name. It was removed, and this module must not grow
one back. Text cannot prove a symbol is defined:

- a `fn name` inside a comment or a string literal is not a definition;
- a `+++ /dev/null` or a deleted path header says a file is gone, not that it
  exists;
- a path component named `helpers` is not a definition of a symbol `helpers`;
- a definition in one module does not make a symbol visible in another.

Each of those turned a real undefined-symbol finding into silence. The model
filter is the only rejecter, it sees the same diff and looked-up text, and it
is told to reject only on proof, so a claim that the diff disproves is still
dropped, and a claim it cannot disprove reaches the author.

## Which lanes run it

`critique`, `security`, and `commits`. The `commits` lane was added after issue #47, where
it read a loaded phrase in a commit subject and reported a privilege escalation
that the commit's patch plainly did not make — a claim the evidence disproves,
which is precisely what this pass is for. Its "diff" there is the rendered
commit range, so a finding about a message is judged against the messages rather
than rejected for being absent from a patch.

The pass runs on the lane's **model** findings only. Scanner findings are merged
in afterwards and are never shown to the filter: a committed key found by a
regular expression is not up for a model's opinion.

Security filters strictly anchored model proposals from both the initial review
and adaptive coverage passes, with the repository evidence those reviewers
looked up. A coverage pass whose new proposals are all disproved stops rather
than unlocking another pass. Filter errors still keep every model proposal.

## The summary has to agree with the verdict

A lane's model writes its prose summary *before* this pass runs, so once the
filter empties the finding list that prose is describing findings that no
longer exist. The lane must therefore drop it rather than print it.

This is not hypothetical. A `critique` check run once opened with "One real bug:
the coverage edge is never stored", reported no findings, concluded `success`,
and approved the pull request in the same breath. The bug was a hallucination
and the filter was right to remove it; only the summary still claimed it, which
made the review assert and deny the same thing in one check run. `summarise` now
replaces the prose with the rejection reasons whenever nothing survived —
they say more than the discarded prose did, and they cannot contradict the
verdict. Covered by `a_summary_never_asserts_a_bug_the_falsifier_removed`.

Security replaces its initial prose whenever any proposal is rejected, including
when other proposals survive. The replacement states only the surviving model
finding count; scanner findings remain independent and are merged afterwards.

## Cost

One call per nonempty proposal batch, including new coverage proposals, on the
cheap tier `Config::model_for_workload(Workload::Falsify)` resolves to, skipped entirely when the lane
produced no findings. It sees the rendered diff, the findings, and repository
evidence the reviewer looked up: no repository policy, no prior findings, no pull request
description. Both inputs are fenced with `harness::prompt::push_fenced` — the
lane model read attacker-controlled text before writing those titles, so a
finding body is no more trustworthy than the diff that produced it.
