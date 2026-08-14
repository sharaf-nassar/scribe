#!/bin/bash
[ "${SCRIBE_E2E_SANDBOX:-0}" = "1" ] || { echo "FATAL: this script only runs inside the scribe e2e container (use just e2e-visual)." >&2; exit 99; }
# e2e-timeout: 120
# Scripted E2E: a second plain launch exits and focuses the existing GPUI client
# without creating a window, server attachment, or session.
#
# Requires: visual container with SCRIBE_SHARE_TAP=1 and the window-lifecycle
# config, so WindowList records expose the server's exact window/session state.
set -e

# shellcheck source=tests/e2e/visual/relaunch-common.bash
. /tests/visual/relaunch-common.bash

DUPLICATE_LOG=/output/relaunch-duplicate.log
PROBE_PID=""
trap 'kill "$PROBE_PID" 2>/dev/null || true' EXIT

focus_gain_count() {
    python3 - "$RECORD" <<'PY'
import json, sys

total = 0
with open(sys.argv[1]) as handle:
    for line in handle:
        try:
            row = json.loads(line)
        except ValueError:
            continue
        message = row.get("message", {})
        if (
            row.get("dir") == "client"
            and message.get("type") == "FocusChanged"
            and message.get("gained") is not None
        ):
            total += 1
print(total)
PY
}

focus_gains_exceed() { [ "$(focus_gain_count)" -gt "$1" ]; }

build_two_window_owner
TARGET=$(scribe_windows | head -1)
focus_window "$TARGET"
LISTS_BEFORE=$(count_frames server WindowList)
wait_for_window_lists "$LISTS_BEFORE" 20 || fail "setup received no WindowList after settling"
BASE_STATE=$(latest_window_state) || fail "setup could not read server window state"
read -r _ BASE_SERVER_SESSIONS <<<"$(window_state_counts "$BASE_STATE")"
[ "$BASE_SERVER_SESSIONS" -ge 2 ] || fail "setup has fewer than two live sessions"
PTYS_BEFORE=$(count_log "$SERVER_LOG" "created new PTY session")
HELLOS_BEFORE=$(count_frames client Hello)

# @lat: [[test#Test Harness#Visual E2E Tests#Client relaunch handling#Plain relaunch focuses the live owner]]
# Put an unrelated X client in front, then require the singleton owner's window
# to regain real EWMH activation and emit its focus gain.
command -v xmessage >/dev/null || fail "xmessage is missing from the visual image"
xmessage -geometry 180x70-0-0 'relaunch focus probe' >/output/relaunch-xmessage.log 2>&1 &
PROBE_PID=$!
sleep 1.0
PROBE_WID=$(xdotool search --name '[Xx]message' 2>/dev/null | tail -1)
[ -n "$PROBE_WID" ] || fail "focus probe never mapped"
focus_window "$PROBE_WID"
window_is_active "$PROBE_WID" || fail "focus probe never took activation"
GAINS_BEFORE=$(focus_gain_count)
LISTS_BEFORE=$(count_frames server WindowList)
: >"$DUPLICATE_LOG"
STATUS=0
timeout 15 scribe-client >"$DUPLICATE_LOG" 2>&1 || STATUS=$?
[ "$STATUS" -eq 0 ] || fail "second plain client exited $STATUS instead of 0"
grep -qF "terminal client already running; sent focus and exiting" "$DUPLICATE_LOG" \
    || fail "second client did not report the singleton focus handoff"
wait_until 15 window_is_active "$TARGET" \
    || fail "focus handoff did not activate owner window $TARGET"
wait_until 15 focus_gains_exceed "$GAINS_BEFORE" \
    || fail "activation emitted no FocusChanged gain"
window_count_is 2 || fail "plain relaunch opened another GPUI window"
[ "$(pgrep -xc scribe-client || true)" -eq 1 ] \
    || fail "plain relaunch left another client process"
[ "$(count_frames client Hello)" -eq "$HELLOS_BEFORE" ] \
    || fail "plain relaunch attached to the server"
[ "$(count_log "$SERVER_LOG" "created new PTY session")" -eq "$PTYS_BEFORE" ] \
    || fail "plain relaunch created a session"
wait_for_window_lists "$LISTS_BEFORE" 20 || fail "received no post-handoff WindowList"
[ "$(latest_window_state)" = "$BASE_STATE" ] \
    || fail "plain relaunch changed the server window/session state"
kill "$PROBE_PID" 2>/dev/null || true
PROBE_PID=""
scrot -o /output/relaunch-focus.png

echo "PASS: second plain client exited 0 and focused $TARGET without server state changes"
