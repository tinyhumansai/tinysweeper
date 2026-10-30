# Verifying a review fix against live pull requests

Unit and golden tests prove a fix offline. `scripts/verify-live.sh` adds the
check those tests cannot make: that the fix holds against real pull requests,
real models, and the deployed server. It fans out in parallel, so a verdict
arrives in minutes rather than after the next day's traffic.

It exists because of the 2026-10-30 outage. The review path had stayed green in
CI while production posted almost nothing. Critique findings were withheld
behind a stale index, a stale `shallow.lock` froze one repository's cache, and
host stalls cancelled about a quarter of all runs. Every one of those was
visible in live check runs and server logs. None was visible to the test suite.

## Two tiers

| Tier | Runs | Writes to GitHub | Exercises |
| --- | --- | --- | --- |
| `dry` | a local build of the CLI, on any machine | nothing | lanes, model routing, prompts, finding yield, per-PR latency |
| `canary` | the deployed server, through `/admin/reviews` | check runs on the chosen PRs | scheduler heartbeat and leases, the repository cache, retrieval, PID 1, publish |

`dry` runs `tinysweeper review --propose-to`. That command reads through a
read-only forge and never runs `apply`, so it is safe to point at any pull
request. It has no retrieval index, so its retrieval status is always `Off`.
Anything that depends on index state (the stale-index gate, cache locks) is
canary-only.

`canary` queues manual reviews exactly as the **Manual review** workflow does.
The admin token is read on the box and fed to `curl` on stdin; it is never
copied off the box. The script then polls GitHub and the server log until every
PR settles.

## Running it

```sh
# Build the candidate and a baseline, then compare their tables.
cargo build --locked --release --features harness,github --bin tinysweeper
SURPLUS_API_KEY=… scripts/verify-live.sh dry \
  --bin target/release/tinysweeper --config .tinysweeper.toml -P 4

# After a deploy: needs `ssh robot1` (deploy/README.md) and a go-ahead,
# because it publishes check runs.
scripts/verify-live.sh canary --host robot1
```

Both tiers take an explicit PR list, or `--newest N` for the repository's N
newest open PRs. The default set is chosen to cover each failure seen on
2026-10-30:

| PR (openhuman) | Why it is in the set |
| --- | --- |
| #8495, #8496 | Critique findings were produced but withheld behind a stale index |
| #8425, #8460, #8477 | Large PRs that ended "Not reviewed" after repeated ownership-loss cancels |
| #8511 | Large-review budget omissions |
| #8510, #8494 | Small controls that reviewed cleanly |

## What each tier asserts

`dry` fails when:
- any review errors (a PR refused for exceeding the review limits is a pass),
- or any lane does not complete.

It prints, per PR:
- wall time
- lanes completed
- `findings/noted` per lane

It also prints p50 and p95 wall time. Run it once with the baseline build and
once with the candidate build. The difference between the two tables is the
live half of the fix's regression test.

`canary` fails when, between the burst and settlement:
- the server log shows any `outcome=cancelled` run, any "ownership expired or
  was replaced", or any `Unable to create …lock`;
- the zombie count under the server process rose;
- or any PR's umbrella check settled as "Not reviewed" or never settled.

It prints time from enqueue to verdict, p50 and p95.

## Speed

Both tiers parallelise.
- `dry` runs `-P` reviews at once, with model calls going straight to the
  provider. Eight PRs at `-P 4` finished in about three minutes on 2026-10-30,
  at about 60s per PR.
- `canary` sends every queue request concurrently and polls all PRs on one
  15-second loop. It therefore measures the server's real concurrency, bounded
  by `server.review_concurrency`, rather than the script's.
