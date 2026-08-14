#!/bin/bash
# e2e-timeout: 50
[ "${SCRIBE_E2E_SANDBOX:-0}" = "1" ] || { echo "FATAL: Docker E2E only" >&2; exit 99; }
set -euo pipefail

PORT=8098
API_URL="http://127.0.0.1:$PORT"
LOG=/output/ci-run-details-requests.jsonl
SERVER_LOG=/output/ci-run-details-api.log
PROJECT=/tmp/ci-run-details
SCENARIO=/tmp/ci-run-details-scenario.json
API_PID=""

cleanup_api() {
    kill "$API_PID" 2>/dev/null || true
}
trap cleanup_api EXIT

fail() {
    echo "FAIL: $*" >&2
    tail -80 "$SERVER_LOG" 2>/dev/null || true
    tail -80 /output/server.log 2>/dev/null || true
    exit 1
}

jobs_count() {
    grep -c '"target":"/repos/acme/widget/actions/runs/101/jobs?' "$LOG" 2>/dev/null || true
}

[ "${SCRIBE_GITHUB_API_URL:-}" = "$API_URL" ] \
    || fail "SCRIBE_GITHUB_API_URL was not passed into the container"

rm -rf "$PROJECT"
mkdir -p "$PROJECT"
git -C "$PROJECT" init --quiet --initial-branch=main
git -C "$PROJECT" config user.email e2e@example.invalid
git -C "$PROJECT" config user.name 'Scribe E2E'
printf 'first\n' >"$PROJECT/fixture.txt"
git -C "$PROJECT" add fixture.txt
git -C "$PROJECT" commit --quiet -m first
git -C "$PROJECT" remote add origin git@github.com:acme/widget.git
SHA=$(git -C "$PROJECT" rev-parse HEAD)
sed "s/1111111111111111111111111111111111111111/$SHA/g" \
    /tests/fixtures/github-actions-api.json >"$SCENARIO"

scribe-test github-actions-api \
    --scenario "$SCENARIO" \
    --request-log "$LOG" \
    --port "$PORT" >"$SERVER_LOG" 2>&1 &
API_PID=$!
for _ in {1..50}; do
    grep -q "listening on 127.0.0.1:$PORT" "$SERVER_LOG" 2>/dev/null && break
    kill -0 "$API_PID" 2>/dev/null || fail "fixture exited"
    sleep 0.1
done
grep -q "listening on 127.0.0.1:$PORT" "$SERVER_LOG" || fail "fixture did not start"

scribe-test daemon stop
scribe-test server stop
printf '[github_ci]\nenabled = true\n\n[workspaces]\nroots = ["/tmp"]\n' \
    >"$HOME/.config/scribe/config.toml"
scribe-test server start
scribe-test daemon start
SESSION=$(scribe-test session create --cwd "$PROJECT")
scribe-test wait-cwd "$SESSION" "$PROJECT"

# Moving the remote-tracking ref to a local branch tip is the logical event the
# production watcher recognizes after a successful push.
git -C "$PROJECT" update-ref refs/remotes/origin/main "$SHA"
for _ in {1..100}; do
    grep -q '"target":"/repos/acme/widget/actions/runs?' "$LOG" 2>/dev/null && break
    sleep 0.1
done
grep -q '"target":"/repos/acme/widget/actions/runs?' "$LOG" \
    || fail "tracker made no workflow-run request"

# A visible CI band is still collapsed. Multiple shared scheduler ticks must
# remain incapable of creating a per-job request without panel interest.
sleep 6
[ "$(jobs_count)" -eq 0 ] || fail "closed panel requested jobs"

scribe-test daemon ci-details \
    --repo-root "$PROJECT" \
    --head-sha "$SHA" \
    --interested true
for _ in {1..100}; do
    [ "$(jobs_count)" -gt 0 ] && break
    sleep 0.1
done
OPEN_COUNT=$(jobs_count)
[ "$OPEN_COUNT" -gt 0 ] || fail "open panel made no jobs request"

scribe-test daemon ci-details \
    --repo-root "$PROJECT" \
    --head-sha "$SHA" \
    --interested false
sleep 6
[ "$(jobs_count)" -eq "$OPEN_COUNT" ] \
    || fail "jobs requests continued after panel close"

# @lat: [[test#GPUI CI Run Bar#Job requests follow open panels]]
echo "PASS: jobs requests were closed=0, open=$OPEN_COUNT, and stopped on close"
