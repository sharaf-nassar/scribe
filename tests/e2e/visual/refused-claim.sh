#!/bin/bash
[ "${SCRIBE_E2E_SANDBOX:-0}" = "1" ] || { echo "FATAL: this script only runs inside the scribe e2e container (use just e2e-visual)." >&2; exit 99; }
# e2e-timeout: 180
# Scripted E2E: a stale restore claim refused by the server opens exactly one
# fresh empty window without replay, session duplication, or snapshot loss.
#
# The script removes only this disposable container's terminal singleton socket
# so a plain bootstrap reaches the real server with a live owner's snapshot id.
# No production bypass is added.
#
# Requires: visual container with SCRIBE_SHARE_TAP=1 and the window-lifecycle
# config, so WindowList records expose the server's exact window/session state.
set -e

# shellcheck source=tests/e2e/visual/relaunch-common.bash
. /tests/visual/relaunch-common.bash

REFUSED_LOG=/output/refused-claim-client.log
CLIENT_SOCKET="${SCRIBE_RUNTIME_DIR:?the entrypoint must export SCRIBE_RUNTIME_DIR}/client.sock"
REFUSED_PID=""
trap 'kill "$REFUSED_PID" 2>/dev/null || true' EXIT

one_new_empty_window() {
    python3 - "$1" "$2" <<'PY'
import json, sys

before, after = map(json.loads, sys.argv[1:])
old = {window[0]: window for window in before}
new = [window for window in after if window[0] not in old]
if len(after) != len(before) + 1 or len(new) != 1:
    raise SystemExit(1)
if new[0][1] != 0 or not new[0][2]:
    raise SystemExit(1)
if any(window not in after for window in before):
    raise SystemExit(1)
PY
}

# Two live windows make sibling fan-out observable instead of testing a claim
# whose restore index has no remaining entry.
build_two_window_owner
LISTS_BEFORE=$(count_frames server WindowList)
wait_for_window_lists "$LISTS_BEFORE" 20 || fail "setup received no WindowList after settling"

# @lat: [[test#Test Harness#Visual E2E Tests#Client relaunch handling#Refused stale claim stays fresh and non-destructive]]
mapfile -t ORIGINAL_SNAPSHOTS < <(
    find "$RESTORE_DIR/windows" -maxdepth 1 -type f -name '*.toml' -printf '%f\n' | sort
)
[ "${#ORIGINAL_SNAPSHOTS[@]}" -eq 2 ] || fail "expected two original snapshot files"
for snapshot in "${ORIGINAL_SNAPSHOTS[@]}"; do
    grep -qF "${snapshot%.toml}" "$RESTORE_DIR/index.toml" \
        || fail "restore index omitted ${snapshot%.toml} before the claim"
done
[ -S "$CLIENT_SOCKET" ] || fail "terminal singleton socket is missing"
unlink "$CLIENT_SOCKET"
[ ! -e "$CLIENT_SOCKET" ] || fail "could not remove the disposable singleton socket"

BEFORE_REFUSAL=$(latest_window_state) || fail "could not read pre-claim server state"
read -r BEFORE_WINDOWS BEFORE_SESSIONS <<<"$(window_state_counts "$BEFORE_REFUSAL")"
PTYS_BEFORE=$(count_log "$SERVER_LOG" "created new PTY session")
LISTS_BEFORE=$(count_frames server WindowList)
: >"$REFUSED_LOG"
scribe-client >"$REFUSED_LOG" 2>&1 &
REFUSED_PID=$!
wait_for_log_growth "$REFUSED_LOG" "server refused the restore claim" 0 20 \
    || fail "client never observed the refused restore claim"
wait_until 20 window_count_is 3 || fail "refused claim did not map exactly one fresh window"
wait_for_window_lists "$LISTS_BEFORE" 20 || fail "received no post-claim WindowList"
AFTER_REFUSAL=$(latest_window_state) || fail "could not read post-claim server state"
one_new_empty_window "$BEFORE_REFUSAL" "$AFTER_REFUSAL" \
    || fail "server did not add exactly one connected empty window"
read -r AFTER_WINDOWS AFTER_SESSIONS <<<"$(window_state_counts "$AFTER_REFUSAL")"
[ "$AFTER_WINDOWS" -eq $((BEFORE_WINDOWS + 1)) ] \
    || fail "server window count changed by more than one"
[ "$AFTER_SESSIONS" -eq "$BEFORE_SESSIONS" ] || fail "server session count changed"

# A pre-fix replay creates sessions and restore children inside this window.
sleep 3.0
[ "$(count_log "$SERVER_LOG" "created new PTY session")" -eq "$PTYS_BEFORE" ] \
    || fail "replayed or duplicated sessions"
grep -qF "replaying a cold-restart snapshot" "$REFUSED_LOG" \
    && fail "replayed the stale snapshot into the fresh window"
grep -qF "restore child" "$REFUSED_LOG" \
    && fail "spawned a restore-child after the refused claim"
[ "$(pgrep -xc scribe-client || true)" -eq 2 ] \
    || fail "produced a restore-child process storm"
window_count_is 3 || fail "produced extra GPUI windows after settling"
[ "$(snapshot_count)" -eq 2 ] || fail "changed the original snapshot file count"
[ -f "$RESTORE_DIR/index.toml" ] || fail "removed the restore index"
for snapshot in "${ORIGINAL_SNAPSHOTS[@]}"; do
    [ -f "$RESTORE_DIR/windows/$snapshot" ] || fail "removed original snapshot $snapshot"
    grep -qF "${snapshot%.toml}" "$RESTORE_DIR/index.toml" \
        || fail "removed ${snapshot%.toml} from the restore index"
done
scrot -o /output/refused-claim-empty.png

echo "PASS: refused stale claim opened one empty window and preserved both live snapshots"
