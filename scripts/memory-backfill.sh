#!/bin/sh
# Feed the deployed server's memory engine a repository's conversation history.
#
# The server remembers issues, pull requests and everything said on them as
# webhooks arrive. This asks it to walk the history from *before* it was
# listening — every issue and pull request, open or closed, and every comment,
# inline review comment and review anybody left, other agents included — and
# then polls until the walk is done. Nothing here builds or runs tinysweeper,
# holds a model or GitHub credential, or writes to GitHub: it POSTs to
# /admin/memory, and the server does the reading with its own installation
# token.
#
# Usage:
#   scripts/memory-backfill.sh <owner/name> [--since <rfc3339>] [--limit <n>]
#   scripts/memory-backfill.sh <owner/name> --number <n> [--pull-request]
#   scripts/memory-backfill.sh <owner/name> --status
#
# Environment:
#   TINYSWEEPER_SERVER_URL   e.g. https://sweeper.tinyhumans.ai
#   TINYSWEEPER_ADMIN_TOKEN  the server's admin bearer token
#
# A finished walk prints `resume_from`; pass it as --since next time to walk
# only what changed. The first walk of a repository takes minutes: one to
# three GitHub reads per conversation, well inside an installation's hourly
# budget, and the server runs one walk per repository at a time.

set -eu

TARGET="${1:-}"
SINCE=""
LIMIT=""
NUMBER=""
PULL_REQUEST=false
STATUS_ONLY=false
POLL_SECONDS="${POLL_SECONDS:-10}"

usage() {
    sed -n '2,/^$/p' "$0" | sed 's/^# \{0,1\}//'
    exit "${1:-1}"
}

[ -n "$TARGET" ] || usage
shift
while [ $# -gt 0 ]; do
    case "$1" in
        --since) SINCE="$2"; shift 2 ;;
        --limit) LIMIT="$2"; shift 2 ;;
        --number) NUMBER="$2"; shift 2 ;;
        --pull-request) PULL_REQUEST=true; shift ;;
        --status) STATUS_ONLY=true; shift ;;
        -h|--help) usage 0 ;;
        *) echo "unknown argument: $1" >&2; usage ;;
    esac
done

: "${TINYSWEEPER_SERVER_URL:?set TINYSWEEPER_SERVER_URL to the deployment, e.g. https://sweeper.example}"
: "${TINYSWEEPER_ADMIN_TOKEN:?set TINYSWEEPER_ADMIN_TOKEN to the server's admin token}"

case "$TARGET" in
    */*) ;;
    *) echo "'$TARGET' is not owner/name" >&2; exit 1 ;;
esac

BASE="${TINYSWEEPER_SERVER_URL%/}/admin/memory/${TARGET}"
AUTH="Authorization: Bearer ${TINYSWEEPER_ADMIN_TOKEN}"

# One request, body on stdout, status on the last line.
call() {
    method="$1"
    url="$2"
    body="${3:-}"
    if [ -n "$body" ]; then
        curl -sS -X "$method" -H "$AUTH" -H 'content-type: application/json' \
            -d "$body" -w '\n%{http_code}' "$url"
    else
        curl -sS -X "$method" -H "$AUTH" -w '\n%{http_code}' "$url"
    fi
}

show() {
    # Pretty when jq is there; the raw JSON is fine when it is not.
    if command -v jq >/dev/null 2>&1; then jq .; else cat; fi
}

split() {
    RESPONSE="$1"
    CODE="$(printf '%s' "$RESPONSE" | tail -n 1)"
    BODY="$(printf '%s' "$RESPONSE" | sed '$d')"
}

if [ "$STATUS_ONLY" = true ]; then
    split "$(call GET "$BASE")"
    printf '%s\n' "$BODY" | show
    [ "$CODE" = 200 ]
    exit
fi

if [ -n "$NUMBER" ]; then
    split "$(call POST "$BASE/backfill" "{\"number\":${NUMBER},\"pull_request\":${PULL_REQUEST}}")"
    printf '%s\n' "$BODY" | show
    [ "$CODE" = 200 ] || { echo "server answered $CODE" >&2; exit 1; }
    exit
fi

REQUEST="{"
[ -n "$SINCE" ] && REQUEST="${REQUEST}\"since\":\"${SINCE}\","
[ -n "$LIMIT" ] && REQUEST="${REQUEST}\"limit\":${LIMIT},"
REQUEST="${REQUEST%,}}"

split "$(call POST "$BASE/backfill" "$REQUEST")"
case "$CODE" in
    202) echo "backfill of $TARGET started" ;;
    409) echo "a backfill of $TARGET is already running; following it" ;;
    *) printf '%s\n' "$BODY" | show; echo "server answered $CODE" >&2; exit 1 ;;
esac

# Poll until `running` is false. The status is small JSON; a substring test
# avoids requiring jq for the loop.
while :; do
    sleep "$POLL_SECONDS"
    split "$(call GET "$BASE")"
    [ "$CODE" = 200 ] || { printf '%s\n' "$BODY" | show; echo "server answered $CODE" >&2; exit 1; }
    case "$BODY" in
        *'"running":false'*) break ;;
        *) printf '.' ;;
    esac
done
echo
printf '%s\n' "$BODY" | show
case "$BODY" in
    *'"error":"'*) exit 1 ;;
esac
