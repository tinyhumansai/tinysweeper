# Review timing

`src/review_timing.rs` collects per-review phase spans and emits one structured
timing event when the final handle for a run is dropped. Its fields and
operator-facing log filters are documented in the [server guide](../server/README.md#review-timing-logs).

The terminal event keeps physical model-call counters from the observer ledger.
For a completed proposal, `usage_source=proposal` identifies authoritative
aggregate cost, embedding tokens, filtered model identifiers, and per-lane
usage attribution from the finalized proposal. Aggregate totals are cumulative
for the worker's shared execution; lane usage describes the latest finalized
proposal. Retrieval embeddings are included even though they do not pass
through the model observer. Each retry clears the prior proposal snapshot, so
if an attempt fails or is cancelled before a proposal exists,
`usage_source=observed` identifies only the known model usage available so far;
model and lane breakdowns are empty. Finalized totals are a separate snapshot
and never increase observer call counters.
