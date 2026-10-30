#!/usr/bin/env bash
# verify-live.sh — prove a review fix against live pull requests, fast.
#
# Two tiers, both fanned out in parallel so a verdict arrives in minutes:
#
#   dry     Run a tinysweeper build's `review --propose-to` against a set of
#           live pull requests. The CLI reads through a read-only forge and
#           never runs `apply`, so nothing is written to GitHub. It has no
#           retrieval index, so it cannot reproduce index-dependent gating;
#           that is canary's job. Records per-PR wall
#           time, lane completion and finding counts, then asserts every lane
#           completed. Run it with the baseline build and the candidate build
#           and compare the two tables: that is the live half of a regression
#           test, next to the in-crate one.
#
#   canary  Queue manual reviews on the deployed server through
#           `/admin/reviews` (this DOES publish check runs on those PRs, like
#           the Manual review button), then poll GitHub and the server log in
#           parallel until every PR settles. Asserts no cancelled runs, no
#           stale git locks, no zombie growth, and that every head settled
#           with a review that ran (not "Not reviewed", incomplete or failed).
#           It is the only tier that exercises the scheduler heartbeat, the
#           repository cache and PID 1, which the CLI does not touch.
#
# Usage:
#   scripts/verify-live.sh dry    --bin target/release/tinysweeper [--config .tinysweeper.toml] [--repo R] [-P N] [--out DIR] [PR...]
#   scripts/verify-live.sh canary [--repo R] [--host robot1] [--timeout 1800] [PR...]
#
# dry needs GITHUB_TOKEN (or a logged-in `gh`) and the model key the config
# names (SURPLUS_API_KEY for this repository's .tinysweeper.toml). canary needs
# `ssh <host>` to the box in deploy/README.md; the admin token is read and used
# there, never copied off the box.
set -euo pipefail

# The default set covers each failure seen on 2026-10-30 that is still open:
# findings withheld behind a stale index (8495), a large PR that ended "Not
# reviewed" (8425), large-budget omissions (8511) and small controls. Merged
# PRs are skipped by both tiers' callers; add `--newest N` for fresh traffic.
DEFAULT_PRS=(8495 8425 8511 8510 8494)

die() { echo "verify-live: $*" >&2; exit 2; }
need() { command -v "$1" >/dev/null || die "needs $1"; }
need jq; need gh

mode=${1:-}; shift || true
repo=tinyhumansai/openhuman
parallel=4
out=""
bin=""
config=""
host=robot1
timeout_s=1800
prs=()
while [ $# -gt 0 ]; do
  case $1 in
    --repo) repo=$2; shift 2 ;;
    -P) parallel=$2; shift 2 ;;
    --out) out=$2; shift 2 ;;
    --bin) bin=$2; shift 2 ;;
    --config) config=$(realpath "$2"); shift 2 ;;
    --host) host=$2; shift 2 ;;
    --timeout) timeout_s=$2; shift 2 ;;
    --newest) mapfile -t -O "${#prs[@]}" prs < <(gh pr list -R "$repo" -L "$2" --json number -q '.[].number'); shift 2 ;;
    -*) die "unknown flag $1" ;;
    *) prs+=("$1"); shift ;;
  esac
done
[ ${#prs[@]} -gt 0 ] || prs=("${DEFAULT_PRS[@]}")

pct() { # pct <percentile> < numbers
  sort -n | awk -v p="$1" 'NF {a[++n]=$1} END {if (n==0) {print "-"; exit} i=int((n-1)*p/100)+1; print a[i]}'
}

dry_one() { # dry_one <pr>  — runs in a subshell under xargs
  local pr=$1 start end rc
  start=$(date +%s%N)
  set +e
  RUST_LOG=${RUST_LOG:-tinysweeper=info} "$bin" review --repo "$repo" --pr "$pr" \
    ${config:+--config "$config"} --propose-to "$out/$pr.json" >"$out/$pr.log" 2>&1
  rc=$?
  set -e
  end=$(date +%s%N)
  echo "$pr $rc $(( (end - start) / 1000000 ))" >"$out/$pr.status"
}

run_dry() {
  [ -x "$bin" ] || die "--bin must point at a tinysweeper built with --features harness,github"
  out=${out:-verify-live-$(date +%Y%m%dT%H%M%S)}
  mkdir -p "$out"
  export GITHUB_TOKEN=${GITHUB_TOKEN:-$(gh auth token)}
  export -f dry_one; export bin repo out config
  echo "dry: ${#prs[@]} PRs on $repo, $parallel at a time, build $($bin --version 2>/dev/null || echo "$bin")"
  local t0; t0=$(date +%s)
  printf '%s\n' "${prs[@]}" | xargs -P "$parallel" -I{} bash -c 'dry_one {}'
  local wall=$(( $(date +%s) - t0 ))

  local failed=0
  printf '\n%-6s %-4s %8s  %-9s %s\n' PR rc secs lanes "per-lane: findings/noted [incomplete]"
  for pr in "${prs[@]}"; do
    read -r _ rc ms <"$out/$pr.status"
    local lanes="-" detail="(no proposal)"
    if [ -s "$out/$pr.json" ]; then
      lanes=$(jq -r '[.lanes[] | select(.completed)] | length | tostring' "$out/$pr.json")/$(jq '.lanes | length' "$out/$pr.json")
      detail=$(jq -r '[.lanes[] | "\(.lane):\(.findings|length)/\(.noted|length)\(if .completed then "" else "!" end)"] | join(" ")' "$out/$pr.json")
      jq -e 'all(.lanes[]; .completed)' "$out/$pr.json" >/dev/null || failed=1
    else
      # A refused oversize PR is a correct outcome, not a regression.
      grep -q "exceeds review limits" "$out/$pr.log" && detail="(refused: over review limits)" || failed=1
    fi
    [ "$rc" = 0 ] || grep -q "exceeds review limits" "$out/$pr.log" || failed=1
    printf '%-6s %-4s %8.1f  %-9s %s\n' "$pr" "$rc" "$(echo "$ms/1000" | bc -l)" "$lanes" "$detail"
  done
  local p50 p95
  p50=$(cat "$out"/*.status | awk '{print $3/1000}' | pct 50)
  p95=$(cat "$out"/*.status | awk '{print $3/1000}' | pct 95)
  echo
  echo "wall ${wall}s for ${#prs[@]} PRs · per-PR p50 ${p50}s p95 ${p95}s · results in $out/"
  [ "$failed" = 0 ] && echo "PASS: every lane completed" || { echo "FAIL: a review errored or left a lane incomplete (see $out/<pr>.log)"; return 1; }
}

# Everything the canary asks of the box, as one remote script so each poll is
# one SSH round trip. Read-only apart from the POST that queues the reviews.
remote() { ssh -o BatchMode=yes "$host" "cd /opt/tinysweeper && $1"; }
compose='docker compose -f docker-compose.yml -f docker-compose.prod.yml'

run_canary() {
  local since; since=$(date -u +%Y-%m-%dT%H:%M:%SZ)
  local pid z0
  pid=$(remote "docker inspect tinysweeper-tinysweeper-1 --format '{{.State.Pid}}'")
  z0=$(remote "ps -eo ppid=,stat= | awk -v p=$pid '\$1==p && \$2~/Z/' | wc -l")
  # Only open, non-draft PRs can be reviewed; a merged one is declined in a
  # second and would read as a "Not reviewed" failure that says nothing about
  # the server. Heads are resolved before anything is queued, so no review is
  # already running while the burst is still being assembled.
  local open=() heads=() state sha
  for pr in "${prs[@]}"; do
    read -r state sha < <(gh pr view -R "$repo" "$pr" --json state,isDraft,headRefOid \
      -q '"\(if .isDraft then "DRAFT" else .state end) \(.headRefOid)"')
    if [ "$state" = OPEN ]; then open+=("$pr"); heads+=("$sha"); else echo "skip #$pr: $state"; fi
  done
  [ ${#open[@]} -gt 0 ] || die "no open pull request in the set"
  prs=("${open[@]}")
  echo "canary: queueing ${#prs[@]} manual reviews on $repo via $host (zombies under server: $z0)"

  # The admin route admits a manual review only while a review slot is free
  # and answers 503 otherwise, so a burst larger than `review_concurrency`
  # is retried until the server takes it. Latency is measured per PR from
  # the moment the server accepted it, not from the end of the burst.
  local acks; acks=$(mktemp -d)
  trap 'rm -rf "$acks"' RETURN
  local deadline=$(( $(date +%s) + timeout_s ))
  for pr in "${prs[@]}"; do
    (
      while :; do
        # Token stays on the box: read from .env and fed to curl on stdin.
        code=$(remote "port=\$(sed -n 's/^TINYSWEEPER_HOST_PORT=//p' .env); printf 'header = \"Authorization: Bearer %s\"\n' \"\$(sed -n 's/^TINYSWEEPER_ADMIN_TOKEN=//p' .env)\" | curl --config - -sS -o /dev/null -w '%{http_code}' -X POST http://127.0.0.1:\${port:-8081}/admin/reviews/$repo -H 'Content-Type: application/json' -d '{\"number\": $pr}'" || echo 000)
        case $code in
          2??) date +%s >"$acks/$pr"; exit 0 ;;
          503) [ "$(date +%s)" -lt "$deadline" ] || { echo "#$pr: still 503 at the deadline" >&2; exit 1; }
               sleep 10 ;;
          *) echo "#$pr: admin route answered $code" >&2; exit 1 ;;
        esac
      done
    ) &
  done
  declare -A sent_at done_at verdict
  local queued_at; queued_at=$(date +%s)

  while :; do
    local pending=0
    for i in "${!prs[@]}"; do
      local pr=${prs[$i]} sha=${heads[$i]}
      [ -n "${done_at[$pr]:-}" ] && continue
      # Not accepted yet (still retrying a 503): nothing to poll.
      if [ -z "${sent_at[$pr]:-}" ]; then
        [ -s "$acks/$pr" ] && sent_at[$pr]=$(<"$acks/$pr")
        [ -n "${sent_at[$pr]:-}" ] || { pending=$((pending + 1)); continue; }
      fi
      local row
      row=$(gh api "repos/$repo/commits/$sha/check-runs?check_name=tinysweeper/review&filter=latest" \
        -q '.check_runs[0] | "\(.status)\t\(.started_at)\t\(.output.title)"' 2>/dev/null || true)
      local status=${row%%$'\t'*} started; started=$(cut -f2 <<<"$row")
      if [ "$status" = completed ] && [[ "$started" > "$since" || "$started" == "$since" ]]; then
        done_at[$pr]=$(( $(date +%s) - sent_at[$pr] )); verdict[$pr]=$(cut -f3 <<<"$row")
      else
        pending=$((pending + 1))
      fi
    done
    [ "$pending" = 0 ] && break
    [ $(( $(date +%s) - queued_at )) -gt "$timeout_s" ] && { echo "timed out with $pending PRs unsettled"; break; }
    sleep 15
  done

  # Dispatchers still retrying a 503 at the deadline have nothing left to do.
  local stragglers; stragglers=$(jobs -p)
  [ -z "$stragglers" ] || kill $stragglers 2>/dev/null || true
  wait 2>/dev/null || true

  local log; log=$(remote "$compose logs --since $since --no-color tinysweeper 2>&1 | sed -E 's/\x1b\[[0-9;]*m//g'")
  local cancelled locks lost z1
  cancelled=$(grep -c "outcome=cancelled" <<<"$log" || true)
  lost=$(grep -c "ownership expired or was replaced" <<<"$log" || true)
  locks=$(grep -c -E "Unable to create .*\.lock" <<<"$log" || true)
  z1=$(remote "ps -eo ppid=,stat= | awk -v p=$pid '\$1==p && \$2~/Z/' | wc -l")

  local failed=0
  printf '\n%-6s %8s  %s\n' PR secs "umbrella verdict"
  for pr in "${prs[@]}"; do
    local v=${verdict[$pr]:-"(unsettled)"}
    printf '%-6s %8s  %s\n' "$pr" "${done_at[$pr]:--}" "$v"
    # Pass only verdicts where a review actually ran (or a deliberate
    # oversize refusal). Everything else is a failure, including outcomes
    # like "The review could not reach a model" or "Review incomplete",
    # which also settle as `completed`.
    case $v in
      Reviewed*|"Changes requested"*|"The pull request is too large"*) ;;
      *) failed=1 ;;
    esac
  done
  local p50 p95
  # `if`, not `[ … ] && echo`: under `set -e` and `pipefail` a false test
  # as the loop's last command aborted the whole run before the summary.
  local settled; settled=$(for p in "${prs[@]}"; do if [ -n "${done_at[$p]:-}" ]; then echo "${done_at[$p]}"; fi; done)
  p50=$(pct 50 <<<"$settled")
  p95=$(pct 95 <<<"$settled")
  echo
  echo "enqueue→verdict p50 ${p50}s p95 ${p95}s · cancelled runs $cancelled · ownership lost $lost · git lock errors $locks · zombies $z0→$z1"
  [ "$cancelled" = 0 ] && [ "$lost" = 0 ] && [ "$locks" = 0 ] && [ "$z1" -le "$z0" ] || failed=1
  [ "$failed" = 0 ] && echo "PASS" || { echo "FAIL"; return 1; }
}

case $mode in
  dry) run_dry ;;
  canary) run_canary ;;
  *) sed -n '2,/^set -euo/p' "$0" | sed '$d; s/^# \{0,1\}//'; exit 2 ;;
esac
