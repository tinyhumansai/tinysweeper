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

## Two hard properties

**It rejects only.** The response schema has one field: a list of indices, with
a reason each. There is no channel through which a finding can come back
altered, re-scored, merged or invented. A filter that can rewrite what it
filters is a second reviewer nobody gated.

**It fails open.** A model error, a timeout, or an answer that does not parse
means every finding survives, recorded in the outcome as `failed_open`. A noise
filter that can silence a review by breaking is worse than no noise filter, and
a provider outage must not look like a clean review.

Both properties are asserted directly in `src/falsify/test.rs`.

## Cost

One call per lane, on the cheap tier `Config::model_for_workload(Workload::Falsify)` resolves to, skipped entirely when the lane
produced no findings. It sees the rendered diff and the findings, and nothing
else of the run: no repository policy, no prior findings, no pull request
description. Both inputs are fenced with `harness::prompt::push_fenced` — the
lane model read attacker-controlled text before writing those titles, so a
finding body is no more trustworthy than the diff that produced it.
