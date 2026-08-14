#!/bin/bash
[ "${SCRIBE_E2E_SANDBOX:-0}" = "1" ] || { echo "FATAL: this script only runs inside the scribe e2e container (use just e2e-func)." >&2; exit 99; }
# @lat: [[test#Test Harness#E2E Functional Tests#Real Beads Board Refresh]]
set -euo pipefail

fail() {
    echo "FAIL: $*" >&2
    exit 1
}

ROOT=/tmp/scribe-beads-root
PROJECT="$ROOT/real-board"
mkdir -p "$PROJECT"
git -C "$PROJECT" init --quiet
git -C "$PROJECT" config user.email e2e@example.invalid
git -C "$PROJECT" config user.name 'Scribe E2E'

(
    cd "$PROJECT"
    bd init --quiet --stealth --prefix e2e
    bd create 'Real board refresh' --type task --priority 1 >/dev/null
)

scribe-test daemon stop
scribe-test server stop
printf '[workspaces]\nroots = ["%s"]\n' "$ROOT" >"$HOME/.config/scribe/config.toml"
export BD_NO_DAEMON=1
scribe-test server start
scribe-test daemon start

SESSION=$(scribe-test session create --cwd "$PROJECT")
scribe-test wait-cwd "$SESSION" "$PROJECT"
BOARD=$(scribe-test beads-board)

printf '%s\n' "$BOARD" | grep -q '"Ready"' \
    || fail "board refresh did not reach Ready: $BOARD"
printf '%s\n' "$BOARD" | grep -q '"ready_total":1' \
    || fail "seeded issue was not classified as ready: $BOARD"
printf '%s\n' "$BOARD" | grep -q '"title":"Real board refresh"' \
    || fail "seeded issue was absent from the board: $BOARD"

echo 'PASS: real bd refreshed the rooted workspace board'
