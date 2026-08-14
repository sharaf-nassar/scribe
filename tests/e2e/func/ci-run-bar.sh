#!/bin/bash
# e2e-timeout: 60
[ "${SCRIBE_E2E_SANDBOX:-0}" = "1" ] || { echo "FATAL: Docker E2E only" >&2; exit 99; }
set -euo pipefail

PORT=8098
API_URL="http://127.0.0.1:$PORT"
API_LOG=/output/ci-run-bar-requests.jsonl
API_SERVER_LOG=/output/ci-run-bar-api.log
OBSERVER_LOG=/output/ci-run-bar-observer.log
OBSERVER_OUT=/output/ci-run-bar-observer.json
REPO=/tmp/ci-run-bar
REMOTE=/tmp/ci-run-bar.git
SCENARIO=/tmp/ci-run-bar-scenario.json
FIXTURE_PID=""
OBSERVER_PID=""
WAIT_ELAPSED=0

cleanup() {
    [ -z "$FIXTURE_PID" ] || kill "$FIXTURE_PID" 2>/dev/null || true
    [ -z "$OBSERVER_PID" ] || kill "$OBSERVER_PID" 2>/dev/null || true
}
trap cleanup EXIT

fail() {
    echo "FAIL: $*" >&2
    tail -80 "$API_SERVER_LOG" 2>/dev/null || true
    tail -80 "$OBSERVER_LOG" 2>/dev/null || true
    tail -80 /output/server.log 2>/dev/null || true
    exit 1
}

request_count() {
    [ -f "$API_LOG" ] && wc -l <"$API_LOG" || printf '0\n'
}

assert_no_requests() {
    local phase="$1" count
    count=$(request_count)
    [ "$count" -eq 0 ] || fail "$phase made $count GitHub requests"
}

push_head() {
    local branch="$1"
    git -C "$REPO" remote set-url origin "$REMOTE"
    git -C "$REPO" push origin "$branch:main" >/dev/null
    # Keep transport offline while leaving the production rescan the real
    # github.com push target that gates the Actions tracker.
    git -C "$REPO" remote set-url origin git@github.com:acme/widget.git
}

state_matches() {
    local json="$1" head="$2" rollup="$3" stale="$4"
    python3 -c '
import json, sys
try:
    observed = json.load(sys.stdin)
    state = observed.get("state") or {}
except (AttributeError, json.JSONDecodeError):
    raise SystemExit(1)
expected_stale = sys.argv[3] == "true"
raise SystemExit(not (
    state.get("head_sha") == sys.argv[1]
    and state.get("rollup") == sys.argv[2]
    and state.get("stale") is expected_stale
))
' "$head" "$rollup" "$stale" <<<"$json"
}

wait_state() {
    local head="$1" rollup="$2" stale="$3" timeout_ms="$4" label="$5"
    local started now elapsed json
    started=$(date +%s%3N)
    while true; do
        json=$(scribe-test daemon ci-state --repo-root "$REPO" 2>/dev/null || true)
        if state_matches "$json" "$head" "$rollup" "$stale"; then
            now=$(date +%s%3N)
            elapsed=$(( now - started ))
            WAIT_ELAPSED=$elapsed
            printf '%s=%sms\n' "$label" "$elapsed"
            return 0
        fi
        now=$(date +%s%3N)
        [ "$(( now - started ))" -le "$timeout_ms" ] \
            || fail "timed out after ${timeout_ms}ms waiting for $label ($json)"
        sleep 0.1
    done
}

export SCRIBE_GITHUB_API_URL="$API_URL"
! grep -Eq '^[^[:space:]]+[[:space:]]+00000000[[:space:]]' /proc/net/route \
    || fail "container has a default route despite --network none"

rm -rf "$REPO" "$REMOTE"
git init --bare --initial-branch=main "$REMOTE" >/dev/null
git init --initial-branch=main "$REPO" >/dev/null
git -C "$REPO" config user.email scribe@example.com
git -C "$REPO" config user.name 'Scribe E2E'
git -C "$REPO" config core.hooksPath .git-hooks-disabled
git -C "$REPO" remote add origin git@github.com:acme/widget.git
git -C "$REPO" commit --allow-empty -m disabled >/dev/null
git -C "$REPO" branch state-disabled
git -C "$REPO" commit --allow-empty -m flow >/dev/null
git -C "$REPO" branch state-flow
git -C "$REPO" commit --allow-empty -m stale >/dev/null
git -C "$REPO" branch state-stale

FLOW_HEAD=$(git -C "$REPO" rev-parse state-flow)
STALE_HEAD=$(git -C "$REPO" rev-parse state-stale)
python3 - /tests/fixtures/github-actions-api.json "$SCENARIO" \
    "$FLOW_HEAD" "$STALE_HEAD" <<'PY'
import json, sys

source, target, flow_head, stale_head = sys.argv[1:]
with open(source, encoding="utf-8") as handle:
    scenario = json.load(handle)
for snapshot in scenario["runs"]:
    for run in snapshot["workflow_runs"]:
        if run["head_sha"] == "1111111111111111111111111111111111111111":
            run["head_sha"] = flow_head
            run["head_branch"] = "main"
scenario["runs"][-1]["workflow_runs"].append({
    "id": 102,
    "name": "stale",
    "head_sha": stale_head,
    "head_branch": "main",
    "status": "in_progress",
    "conclusion": None,
})
with open(target, "w", encoding="utf-8") as handle:
    json.dump(scenario, handle)
PY

scribe-test github-actions-api --scenario "$SCENARIO" --request-log "$API_LOG" \
    --port "$PORT" >"$API_SERVER_LOG" 2>&1 &
FIXTURE_PID=$!
for _ in {1..50}; do
    grep -q "listening on 127.0.0.1:$PORT" "$API_SERVER_LOG" 2>/dev/null && break
    kill -0 "$FIXTURE_PID" 2>/dev/null || fail "GitHub API fixture exited"
    sleep 0.1
done
grep -q "listening on 127.0.0.1:$PORT" "$API_SERVER_LOG" \
    || fail "GitHub API fixture did not bind loopback"

# Default-off must suppress a real push, including auth and API work.
DISABLED_SESSION=$(scribe-test session create --cwd "$REPO")
scribe-test wait-cwd "$DISABLED_SESSION" "$REPO"
push_head state-disabled
# Longer than the 250ms debounce plus 1s first-request contract.
sleep 2
assert_no_requests disabled

scribe-test daemon stop
scribe-test server stop
printf '[github_ci]\nenabled = true\n\n[workspaces]\nroots = ["/tmp"]\n' \
    >"$HOME/.config/scribe/config.toml"
printf '\n[remote]\nsharing_mode = "shared_single_typist"\n' \
    >>"$HOME/.config/scribe/config.toml"
scribe-test server start
scribe-test daemon start
SESSION=$(scribe-test session create --cwd "$REPO")
scribe-test wait-cwd "$SESSION" "$REPO"

# One full scheduler interval while enabled but idle must remain request-free.
sleep 6
assert_no_requests idle

WINDOW=$(scribe-test daemon window-id)
scribe-test daemon ci-observer --window-id "$WINDOW" --repo-root "$REPO" \
    --head-sha "$STALE_HEAD" --stale true --timeout 30000 \
    >"$OBSERVER_OUT" 2>"$OBSERVER_LOG" &
OBSERVER_PID=$!
for _ in {1..50}; do
    grep -q "^READY: joined $WINDOW .* with 2 participants$" "$OBSERVER_LOG" 2>/dev/null \
        && break
    kill -0 "$OBSERVER_PID" 2>/dev/null || fail "shared CI observer exited before joining"
    sleep 0.1
done
grep -q "^READY: joined $WINDOW .* with 2 participants$" "$OBSERVER_LOG" \
    || fail "shared CI observer did not join the daemon window"

PUSH_STARTED=$(date +%s%3N)
push_head state-flow
wait_state "$FLOW_HEAD" queued false 10000 first_state
FIRST_LATENCY=$(( $(date +%s%3N) - PUSH_STARTED ))
[ "$FIRST_LATENCY" -le 10000 ] \
    || fail "first state arrived after ${FIRST_LATENCY}ms (limit 10000ms)"

OWNER_STATE=$(scribe-test daemon ci-state --repo-root "$REPO")
python3 -c '
import json, sys
owner = json.loads(sys.argv[1])
assert owner["action_mode"] == "owner" and owner["open_url"]
' "$OWNER_STATE" || fail "owner model omitted host actions"

wait_state "$FLOW_HEAD" running false 7000 running_state
[ "$WAIT_ELAPSED" -ge 4000 ] \
    || fail "running refresh arrived before the 5s cadence (${WAIT_ELAPSED}ms)"
wait_state "$FLOW_HEAD" success false 7000 terminal_state
[ "$WAIT_ELAPSED" -ge 4000 ] \
    || fail "terminal refresh arrived before the 5s cadence (${WAIT_ELAPSED}ms)"
[ "$(request_count)" -eq 3 ] \
    || fail "completion used $(request_count) requests instead of 3"
grep -q '"if_none_match":"\\"runs-0\\"","status":200' "$API_LOG" \
    || fail "running refresh omitted the first run ETag"
grep -q '"if_none_match":"\\"runs-1\\"","status":200' "$API_LOG" \
    || fail "terminal refresh omitted the second run ETag"

push_head state-stale
wait_state "$STALE_HEAD" running false 10000 stale_head_running
[ "$(request_count)" -eq 4 ] \
    || fail "stale-head discovery used $(request_count) requests instead of 4"
kill "$FIXTURE_PID"
wait "$FIXTURE_PID" 2>/dev/null || true
FIXTURE_PID=""
wait_state "$STALE_HEAD" running true 7000 stale_state
[ "$(request_count)" -eq 4 ] \
    || fail "idle or disabled traffic changed the four-request contract"

OWNER_STATE=$(scribe-test daemon ci-state --repo-root "$REPO")
if ! wait "$OBSERVER_PID"; then
    fail "shared CI observer did not receive final fanout"
fi
OBSERVER_PID=""
VIEWER_STATE=$(<"$OBSERVER_OUT")
python3 -c '
import json, sys
owner = json.loads(sys.argv[1])
viewer = json.loads(sys.argv[2])
assert owner["state"] == viewer["state"]
assert owner["action_mode"] == "owner" and owner["open_url"]
assert viewer["action_mode"] == "read_only" and viewer["open_url"] is None
' "$OWNER_STATE" "$VIEWER_STATE" || fail "shared viewer state or actions diverged from owner"

# @lat: [[test#GitHub CI Functional E2E#Push-gated client state progression]]
echo "PASS: real push gated four requests; state arrived in ${FIRST_LATENCY}ms," \
    "completed, became stale, and a joined viewer received read-only fanout"
