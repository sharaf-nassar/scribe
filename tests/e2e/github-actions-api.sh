#!/bin/bash
[ "${SCRIBE_E2E_SANDBOX:-0}" = "1" ] || { echo "FATAL: this script only runs inside the scribe e2e container (use just e2e-func / e2e-visual)." >&2; exit 99; }
set -euo pipefail

PORT=8098
API_URL="http://127.0.0.1:$PORT"
LOG=/output/github-actions-api-requests.jsonl
SERVER_LOG=/output/github-actions-api.log
SHA=1111111111111111111111111111111111111111
API_PID=""

cleanup() {
    kill "$API_PID" 2>/dev/null || true
}
trap cleanup EXIT

[ "${SCRIBE_GITHUB_API_URL:-}" = "$API_URL" ] \
    || { echo "FAIL: SCRIBE_GITHUB_API_URL was not passed into the container" >&2; exit 1; }

fail() {
    echo "FAIL: $*" >&2
    exit 1
}

http_get() {
    local target="$1" conditional="${2:-}"
    exec 3<>"/dev/tcp/127.0.0.1/$PORT"
    printf 'GET %s HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n' "$target" >&3
    [ -z "$conditional" ] || printf '%s\r\n' "$conditional" >&3
    printf '\r\n' >&3
    cat <&3
    exec 3>&- 3<&-
}

status_is() {
    local response="$1" expected="$2"
    [[ "$response" == "HTTP/1.1 $expected "* ]] \
        || fail "expected HTTP $expected, got ${response%%$'\r\n'*}"
}

body_of() {
    printf '%s' "${1#*$'\r\n\r\n'}"
}

scribe-test github-actions-api \
    --scenario /tests/fixtures/github-actions-api.json \
    --request-log "$LOG" \
    --port "$PORT" >"$SERVER_LOG" 2>&1 &
API_PID=$!

for _ in {1..50}; do
    grep -q "listening on 127.0.0.1:$PORT" "$SERVER_LOG" 2>/dev/null && break
    kill -0 "$API_PID" 2>/dev/null || { cat "$SERVER_LOG" >&2; fail "fixture exited"; }
    sleep 0.1
done
grep -q "listening on 127.0.0.1:$PORT" "$SERVER_LOG" \
    || fail "fixture did not start"
grep -Eq "^[[:space:]]*[0-9]+: 0100007F:1FA2 " /proc/net/tcp \
    || fail "fixture is not bound to IPv4 loopback"
! grep -Eq "^[[:space:]]*[0-9]+: 00000000:1FA2 " /proc/net/tcp \
    || fail "fixture is bound to all IPv4 interfaces"

RUNS="/repos/acme/widget/actions/runs?head_sha=$SHA&event=push&per_page=100"
response=$(http_get "$RUNS")
status_is "$response" 200
body=$(body_of "$response")
grep -q '"id":101' <<<"$body" || fail "first run snapshot omitted matching head"
grep -q '"status":"queued"' <<<"$body" || fail "first run snapshot was not queued"
! grep -q '"id":999' <<<"$body" || fail "run response did not filter head_sha"

response=$(http_get "$RUNS")
status_is "$response" 200
grep -q '"status":"in_progress"' <<<"$(body_of "$response")" \
    || fail "second run snapshot did not progress"

response=$(http_get "$RUNS")
status_is "$response" 200
grep -q '"conclusion":"success"' <<<"$(body_of "$response")" \
    || fail "third run snapshot did not complete"

response=$(http_get "$RUNS" 'If-None-Match: "runs-2"')
status_is "$response" 304

JOBS=/repos/acme/widget/actions/runs/101/jobs
response=$(http_get "$JOBS")
status_is "$response" 200
grep -q '"status":"in_progress"' <<<"$(body_of "$response")" \
    || fail "first job snapshot was not in progress"

response=$(http_get "$JOBS")
status_is "$response" 200
grep -q '"conclusion":"success"' <<<"$(body_of "$response")" \
    || fail "second job snapshot did not complete"

response=$(http_get "$JOBS" 'If-None-Match: "jobs-101-1"')
status_is "$response" 304

response=$(http_get /unknown)
status_is "$response" 404

[ "$(wc -l <"$LOG")" -eq 8 ] || fail "request log did not record every request"
grep -q '"target":"/repos/acme/widget/actions/runs?head_sha=' "$LOG" \
    || fail "request log omitted run query"
grep -q '"if_none_match":"\\\"runs-2\\\"","status":304' "$LOG" \
    || fail "request log omitted conditional request"
grep -q '"target":"/unknown","if_none_match":null,"status":404' "$LOG" \
    || fail "request log omitted rejected route"

# @lat: [[test#GitHub Actions API Fixture#Loopback scripted API]]
echo "PASS: loopback GitHub Actions API fixture filters, progresses, caches, and logs"
