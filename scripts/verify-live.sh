#!/usr/bin/env bash
# verify-live.sh — prove a review fix against live pull requests, fast.
#
# Two tiers, both fanned out in parallel so a verdict arrives in minutes:
#
#   dry     Run a tinysweeper build's `review --dry-run` against a set of live
#           pull requests. Nothing is written to GitHub. Records per-PR wall
#           time, lane completion and finding counts, then asserts every lane
#           completed. Run it with the baseline build and the candidate build
#           and compare the two tables: that is the live half of a regression
#           test, next to the in-crate one.
#
#   canary  Queue manual reviews on the deployed server through
#           `/admin/reviews` (this DOES publish check runs on those PRs, like
#           the Manual review button), then poll GitHub and the server log in
#           parallel until every PR settles. Asserts no cancelled runs, no
#           stale git locks, no zombie growth and no head left "Not reviewed".
#           It is the only tier that exercises the scheduler heartbeat, the
#           repository cache and PID 1, which the CLI does not touch.
#
# Usage:
#   scripts/verify-live.sh dry    --bin target/release/tinysweeper [--repo R] [-P N] [--out DIR] [PR...]
#   scripts/verify-live.sh canary [--repo R] [--host robot1] [--timeout 1800] [PR...]
#
# dry needs GITHUB_TOKEN (or a logged-in `gh`) and the model key the config
# names (SURPLUS_API_KEY for this repository's .tinysweeper.toml). canary needs
# `ssh <host>` to the box in deploy/README.md; the admin token is read and used
# there, never copied off the box.
set -euo pipefail

# The default set covers each failure seen on 2026-10-30: findings withheld
# behind a stale index (8495, 8496), large PRs that ended "Not reviewed"
# (8425, 8460, 8477), large-budget omissions (8511) and small controls.
DEFAULT_PRS=(8495 8496 8425 8460 8477 8511 8510 8494)

die() { echo "verify-live: $*" >&2; exit 2; }
need() { command -v "$1" >/dev/null || die "needs $1"; }
need jq; need gh

mode=${1:-}; shift || true
repo=tinyhumansai/openhuman
parallel=4
out=""
bin=""
host=robot1
timeout_s=1800
prs=()
while [ $# -gt 0 ]; do
  case $1 in
    --repo) repo=$2; shift 2 ;;
    -P) parallel=$2; shift 2 ;;
    --out) out=$2; shift 2 ;;
    --bin) bin=$2; shift 2 ;;
    --host) host=$2; shift 2 ;;
    --timeout) timeout_s=$2; shift 2 ;;
    --newest) mapfile -t -O "${#prs[@]}" prs < <(gh pr list -R "$repo" -L "$2" --json number -q '.[].number'); shift 2 ;;
    -*) die "unknown flag $1" ;;
    *) prs+=("$1"); shift ;;
  esac
done
[ ${#prs[@]} -gt 0 ] || prs=("${DEFAULT_PRS[@]}")

pct() { # pct <percentile> < numbers
  sort -n | awk -v p="$1" '{a[NR]=$1} END {if (NR==0) {print "-"; exit} i=int((NR-1)*p/100)+1; print a[i]}'
}

dry_one() { # dry_one <pr>  — runs in a subshell under xargs
  local pr=$1 start end rc
  start=$(date +%s%N)
  set +e
  RUST_LOG=${RUST_LOG:-tinysweeper=info} "$bin" review --repo "$repo" --pr "$pr" \
    --dry-run --propose-to "$out/$pr.json" >"$out/$pr.log" 2>&1
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
  export -f dry_one; export bin repo out
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
  echo "canary: queueing ${#prs[@]} manual reviews on $repo via $host (zombies under server: $z0)"
  local heads=()
  for pr in "${prs[@]}"; do
    heads+=("$(gh pr view -R "$repo" "$pr" --json headRefOid -q .headRefOid)")
    # Token stays on the box: read from .env and fed to curl on stdin.
    remote "printf 'header = \"Authorization: Bearer %s\"\n' \"\$(grep ^TINYSWEEPER_ADMIN_TOKEN= .env | cut -d= -f2-)\" | curl --config - -fsS -X POST http://127.0.0.1:\$(grep ^TINYSWEEPER_HOST_PORT= .env | cut -d= -f2- || echo 8081)/admin/reviews/$repo -H 'Content-Type: application/json' -d '{\"number\": $pr}' >/dev/null" &
  done
  wait
  local queued_at; queued_at=$(date +%s)

  declare -A done_at verdict
  while :; do
    local pending=0
    for i in "${!prs[@]}"; do
      local pr=${prs[$i]} sha=${heads[$i]}
      [ -n "${done_at[$pr]:-}" ] && continue
      local row
      row=$(gh api "repos/$repo/commits/$sha/check-runs?check_name=tinysweeper/review&filter=latest" \
        -q '.check_runs[0] | "\(.status)\t\(.started_at)\t\(.output.title)"' 2>/dev/null || true)
      local status=${row%%$'\t'*} started; started=$(cut -f2 <<<"$row")
      if [ "$status" = completed ] && [[ "$started" > "$since" || "$started" == "$since" ]]; then
        done_at[$pr]=$(( $(date +%s) - queued_at )); verdict[$pr]=$(cut -f3 <<<"$row")
      else
        pending=$((pending + 1))
      fi
    done
    [ "$pending" = 0 ] && break
    [ $(( $(date +%s) - queued_at )) -gt "$timeout_s" ] && { echo "timed out with $pending PRs unsettled"; break; }
    sleep 15
  done

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
    case $v in "Not reviewed"|"(unsettled)") failed=1 ;; esac
  done
  local p50 p95
  p50=$(for p in "${prs[@]}"; do [ -n "${done_at[$p]:-}" ] && echo "${done_at[$p]}"; done | pct 50)
  p95=$(for p in "${prs[@]}"; do [ -n "${done_at[$p]:-}" ] && echo "${done_at[$p]}"; done | pct 95)
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
