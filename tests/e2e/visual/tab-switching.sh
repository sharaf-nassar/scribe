#!/bin/bash
[ "${SCRIBE_E2E_SANDBOX:-0}" = "1" ] || { echo "FATAL: this script only runs inside the scribe e2e container (use just e2e-func / e2e-visual)." >&2; exit 99; }
# Scripted E2E: keyboard and titlebar tab switching reach the live session
# switch path.
#
# The GPUI shell already knew how to move its `TabSessions`, but the running
# client still had two ways to strand that state:
#   * the titlebar emitted `SelectTab` / `CloseTab` without a subscriber, so a
#     click changed only titlebar-local state and never switched the live pane;
#   * a keyboard tab switch only mattered if the binding path reached
#     `switch_tab`, which is a live-client property no headless table test can
#     prove.
#
# This run keeps the assertions on the real wire. It starts from the shared-pane
# rig so the client is attached to a known session id (`$SESSION`), opens a
# second tab through the real new-tab chord, and then requires keyboard
# `ctrl+Prior` / `ctrl+Next` and titlebar clicks to emit `AttachSessions` for
# the expected session ids. A mere visual underline move would not be enough:
# only `AttachSessions` proves the shell asked the server to make a different
# terminal session live.
#
# Requires: visual container with SCRIBE_SHARED_PANE=1 and SCRIBE_SHARE_TAP=1.
set -euo pipefail

RECORD="${SHARE_WIRE_RECORD:-/output/share-wire.jsonl}"
CLIENT_LOG="${SCRIBE_CLIENT_LOG:-/output/client.log}"
SERVER_LOG="${SCRIBE_SERVER_LOG:-/output/server.log}"
SESSION="${SESSION:?the entrypoint must export a created SESSION}"

# Titlebar geometry from crates/scribe-client/src/titlebar.rs: the client band
# is 34 px tall and each tab is a fixed 176 px wide. This shared-pane rig runs
# in single-workspace mode, so no workspace badge offsets the strip; the first
# two tab centres are therefore 88 px and 264 px from the client origin.
TITLEBAR_HEIGHT=34
TAB_WIDTH=176
TITLEBAR_Y=$(( TITLEBAR_HEIGHT / 2 ))
FIRST_TAB_X=$(( TAB_WIDTH / 2 ))
SECOND_TAB_X=$(( TAB_WIDTH + TAB_WIDTH / 2 ))

TERM_X=0
TERM_Y=0

count_frames() {
    python3 - "$RECORD" "$1" "$2" <<'PY'
import json
import sys

path, direction, wanted = sys.argv[1:]
total = 0
try:
    handle = open(path)
except OSError:
    print(0)
    sys.exit(0)
with handle:
    for line in handle:
        try:
            row = json.loads(line)
        except ValueError:
            continue
        if row.get("dir") != direction:
            continue
        if row.get("message", {}).get("type") == wanted:
            total += 1
print(total)
PY
}

count_client() { count_frames client "$1"; }
count_server() { count_frames server "$1"; }

count_attach_to() {
    python3 - "$RECORD" "$1" <<'PY'
import json
import sys

path, session_id = sys.argv[1:]
total = 0
try:
    handle = open(path)
except OSError:
    print(0)
    sys.exit(0)
with handle:
    for line in handle:
        try:
            row = json.loads(line)
        except ValueError:
            continue
        if row.get("dir") != "client":
            continue
        message = row.get("message", {})
        if message.get("type") != "AttachSessions":
            continue
        if session_id in message.get("session_ids", []):
            total += 1
print(total)
PY
}

wait_for_count_growth() {
    local command="$1" baseline="$2" timeout_secs="$3" started now
    started=$(date +%s)
    while true; do
        now=$(eval "$command")
        if [ "$now" -gt "$baseline" ]; then
            return 0
        fi
        if [ $(( "$(date +%s)" - started )) -ge "$timeout_secs" ]; then
            return 1
        fi
        sleep 0.3
    done
}

wait_for_attach_to() {
    local session_id="$1" baseline="$2" timeout_secs="${3:-15}"
    wait_for_count_growth "count_attach_to '$session_id'" "$baseline" "$timeout_secs"
}

latest_created_session_after() {
    python3 - "$RECORD" "$1" "$SESSION" <<'PY'
import json
import sys

path, baseline, original = sys.argv[1], int(sys.argv[2]), sys.argv[3]
seen = 0
found = None
with open(path) as handle:
    for line in handle:
        try:
            row = json.loads(line)
        except ValueError:
            continue
        if row.get("dir") != "server":
            continue
        message = row.get("message", {})
        if message.get("type") != "SessionCreated":
            continue
        seen += 1
        if seen <= baseline:
            continue
        session_id = message.get("session_id")
        if session_id and session_id != original:
            found = session_id
if found is None:
    sys.exit(1)
print(found)
PY
}

list_terminal_windows() {
    xdotool search --name '^Scribe$' 2>/dev/null || true
}

focus_terminal() {
    local wid
    wid=$(list_terminal_windows | tail -1)
    if [ -z "$wid" ]; then
        fail "no Scribe terminal window found"
    fi
    xdotool windowactivate --sync "$wid" 2>/dev/null \
        || xdotool windowfocus --sync "$wid" 2>/dev/null || true
    sleep 0.8
    local info
    info=$(xwininfo -id "$wid")
    TERM_X=$(printf '%s\n' "$info" | awk '/Absolute upper-left X/ { print $4 }')
    TERM_Y=$(printf '%s\n' "$info" | awk '/Absolute upper-left Y/ { print $4 }')
}

send_keys() {
    focus_terminal
    xdotool key --clearmodifiers "$@"
    sleep 0.8
}

press_keys() {
    xdotool key --clearmodifiers "$@"
    sleep 0.8
}

type_text() {
    xdotool type --clearmodifiers --delay 40 "$1"
    sleep 0.5
}

click_terminal_at() {
    focus_terminal
    xdotool mousemove "$(( TERM_X + $1 ))" "$(( TERM_Y + $2 ))"
    sleep 0.3
    xdotool click 1
    sleep 0.8
}

shot() {
    sleep 0.3
    scrot -o "$1"
    echo "captured $1"
}

fail() {
    echo "FAIL: $1"
    echo "--- client log tail ---"
    tail -40 "$CLIENT_LOG" 2>/dev/null || true
    echo "--- server log tail ---"
    tail -20 "$SERVER_LOG" 2>/dev/null || true
    exit 1
}

# ── Phase 0: shared-pane attach is live ───────────────────────────
if ! wait_for_attach_to "$SESSION" 0 20; then
    fail "phase 0: the client never attached to the shared-pane session $SESSION"
fi
shot /output/00-tab-switching-attached.png
echo "PHASE 0 PASS: client attached to shared-pane session $SESSION"

# ── Phase 1: create a second tab through the live chord ───────────
CREATES_BEFORE=$(count_client CreateSession)
CREATED_BEFORE=$(count_server SessionCreated)
send_keys ctrl+shift+t
if ! wait_for_count_growth "count_client CreateSession" "$CREATES_BEFORE" 15; then
    fail "phase 1: ctrl+shift+t never sent CreateSession"
fi
if ! wait_for_count_growth "count_server SessionCreated" "$CREATED_BEFORE" 15; then
    fail "phase 1: the server never answered with SessionCreated"
fi
NEW_SESSION=$(latest_created_session_after "$CREATED_BEFORE") \
    || fail "phase 1: could not identify the newly created tab session"
shot /output/01-tab-switching-new-tab.png
echo "PHASE 1 PASS: ctrl+shift+t created tab session $NEW_SESSION"

# ── Phase 2: keyboard switch back to the original tab ─────────────
ATTACH_ORIGINAL_BEFORE=$(count_attach_to "$SESSION")
send_keys ctrl+Prior
if ! wait_for_attach_to "$SESSION" "$ATTACH_ORIGINAL_BEFORE" 15; then
    fail "phase 2: ctrl+Prior never switched back to $SESSION"
fi
shot /output/02-tab-switching-key-prev.png
echo "PHASE 2 PASS: ctrl+Prior attached $SESSION"

# ── Phase 3: keyboard switch forward to the new tab ───────────────
ATTACH_NEW_BEFORE=$(count_attach_to "$NEW_SESSION")
send_keys ctrl+Next
if ! wait_for_attach_to "$NEW_SESSION" "$ATTACH_NEW_BEFORE" 15; then
    fail "phase 3: ctrl+Next never switched forward to $NEW_SESSION"
fi
shot /output/03-tab-switching-key-next.png
echo "PHASE 3 PASS: ctrl+Next attached $NEW_SESSION"

# ── Phase 4: clicking the first titlebar tab switches back ────────
ATTACH_ORIGINAL_CLICK_BEFORE=$(count_attach_to "$SESSION")
click_terminal_at "$FIRST_TAB_X" "$TITLEBAR_Y"
if ! wait_for_attach_to "$SESSION" "$ATTACH_ORIGINAL_CLICK_BEFORE" 15; then
    fail "phase 4: clicking the first titlebar tab never attached $SESSION"
fi
type_text "echo TAB_CLICK_FIRST_FOCUS"
press_keys Return
if ! scribe-test wait-output "$SESSION" "TAB_CLICK_FIRST_FOCUS" --timeout 8000 >/dev/null 2>&1; then
    fail "phase 4: the first tab click switched tabs but left keyboard focus outside the terminal"
fi
shot /output/04-tab-switching-click-first.png
echo "PHASE 4 PASS: first titlebar tab attached $SESSION and kept typing live"

# ── Phase 5: clicking the second titlebar tab switches forward ────
ATTACH_NEW_CLICK_BEFORE=$(count_attach_to "$NEW_SESSION")
click_terminal_at "$SECOND_TAB_X" "$TITLEBAR_Y"
if ! wait_for_attach_to "$NEW_SESSION" "$ATTACH_NEW_CLICK_BEFORE" 15; then
    fail "phase 5: clicking the second titlebar tab never attached $NEW_SESSION"
fi
type_text "echo TAB_CLICK_SECOND_FOCUS"
press_keys Return
if ! scribe-test wait-output "$NEW_SESSION" "TAB_CLICK_SECOND_FOCUS" --timeout 8000 >/dev/null 2>&1; then
    fail "phase 5: the second tab click switched tabs but left keyboard focus outside the terminal"
fi
shot /output/05-tab-switching-click-second.png
echo "PHASE 5 PASS: second titlebar tab attached $NEW_SESSION and kept typing live"

echo ""
echo "PASS: tab switching and post-click typing stay live for keyboard and titlebar"
