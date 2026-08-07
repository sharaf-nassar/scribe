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
# Phases 6-10 extend the oracle to multi-workspace windows: a live
# ctrl+alt+backslash workspace split, then keyboard (alt+1) and pointer tab
# selection across workspace regions, then a client relaunch that rebuilds
# the two regions from the server's persisted workspace tree
# (adopt_server_topology) and the same cross-region click on that adopted
# layout. Sessions the client created itself are asserted through the wire's
# KeyInput frames rather than `scribe-test wait-output`, which can only
# observe daemon-owned sessions.
#
# Phase 11 replays the one thing a plain `xdotool click` can never produce:
# a real-mouse click whose pointer travels a few px between press and
# release, crossing GPUI's native drag threshold.
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

# `COLUMNS` in crates/scribe-client/src/main.rs — the fixed `WindowSeed`
# startup grid. Named here only so phase 3 can refuse to run its size
# assertion on a rig where the real pane happens to measure the same width.
SEED_COLUMNS=120

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

# Count client KeyInput frames addressed to one session. This is the focus
# oracle for sessions the client itself created: `scribe-test wait-output`
# can only observe sessions the test daemon owns, but the wire record shows
# exactly which session the client's encoder routed each keystroke to.
count_keys_to() {
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
        if message.get("type") != "KeyInput":
            continue
        if message.get("session_id") == session_id:
            total += 1
print(total)
PY
}

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

# Columns announced by the most recent client `AttachSessions` naming a
# session. The frame carries per-session `dimensions` parallel to
# `session_ids`, so this is literally the grid the client asked the server to
# give that tab's PTY — the size its replay is then rendered at.
attach_cols_for() {
    python3 - "$RECORD" "$1" <<'PY'
import json
import sys

path, session_id = sys.argv[1:]
cols = ""
try:
    handle = open(path)
except OSError:
    print(cols)
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
        ids = message.get("session_ids", [])
        dimensions = message.get("dimensions", [])
        if session_id in ids:
            index = ids.index(session_id)
            if index < len(dimensions):
                cols = dimensions[index].get("cols", "")
print(cols)
PY
}

# Columns the client's own layout pass last published for a pane. This is the
# measured truth the announced size is checked against: `publish_pane_sizes`
# logs it after dividing the painted pane rect by the live cell box.
#
# The client logs with ANSI colour, so the field reads `cols<ESC>[0m<ESC>[2m=`
# rather than a literal `cols=` — the escapes are stripped first. The capture
# is also kept off `set -o pipefail`'s path: a no-match here must surface as an
# empty string the caller reports, never as a silent mid-phase exit.
published_cols() {
    local line
    line=$(sed 's/\x1b\[[0-9;]*m//g' "$CLIENT_LOG" 2>/dev/null \
        | grep -a "published a pane's grid size" \
        | tail -1) || true
    printf '%s' "$line" | sed -n 's/.*cols=\([0-9][0-9]*\).*/\1/p'
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

# Press-move-release with a few px of pointer travel between press and
# release, like every real human click. The travel crosses GPUI's ~2 px
# native drag threshold (DRAG_THRESHOLD in gpui's div.rs), which engages the
# tab's drag and cancels the element-level click, so the titlebar's release
# path must select the pressed tab itself. `xdotool click` cannot cover this:
# it presses and releases at one exact point.
jitter_click_terminal_at() {
    focus_terminal
    xdotool mousemove "$(( TERM_X + $1 ))" "$(( TERM_Y + $2 ))"
    sleep 0.3
    xdotool mousedown 1
    sleep 0.1
    xdotool mousemove_relative -- 2 1
    sleep 0.1
    xdotool mousemove_relative -- 1 0
    sleep 0.1
    xdotool mouseup 1
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
# Both tabs share this rig's single pane, so each switch must announce that
# pane's own grid. An unshown tab has no placement, so `publish_pane_sizes`
# has already dropped its `pane_sizes` entry; falling back to the window's
# fixed `WindowSeed` default resized the PTY to COLUMNS (120) and the
# switched-to tab painted its replay wrapped at 120 before the layout's own
# publish corrected it one round trip later — a visible reflow on every
# single tab switch.
NEW_COLS=$(attach_cols_for "$NEW_SESSION")
PANE_COLS=$(published_cols)
if [ -z "$NEW_COLS" ] || [ -z "$PANE_COLS" ]; then
    fail "phase 3: no size to compare (announced=${NEW_COLS:-none}, published=${PANE_COLS:-none})"
fi
# Guard the oracle itself. The defect announced exactly SEED_COLUMNS, so a rig
# whose pane happens to measure that width cannot tell a fixed client from a
# broken one. Fail loudly instead of passing vacuously: change the window
# geometry rather than deleting this check.
if [ "$PANE_COLS" = "$SEED_COLUMNS" ]; then
    fail "phase 3: this rig's pane is $PANE_COLS columns, the same as the COLUMNS seed — the assertion below cannot discriminate; widen or narrow the container window"
fi
if [ "$NEW_COLS" != "$PANE_COLS" ]; then
    fail "phase 3: the switch announced $NEW_COLS columns for $NEW_SESSION but the layout published $PANE_COLS for its pane"
fi
echo "PHASE 3 PASS: ctrl+Next attached $NEW_SESSION at the pane's own $NEW_COLS columns"

# ── Phase 4: clicking the first titlebar tab switches back ────────
ATTACH_ORIGINAL_CLICK_BEFORE=$(count_attach_to "$SESSION")
click_terminal_at "$FIRST_TAB_X" "$TITLEBAR_Y"
if ! wait_for_attach_to "$SESSION" "$ATTACH_ORIGINAL_CLICK_BEFORE" 15; then
    fail "phase 4: clicking the first titlebar tab never attached $SESSION"
fi
type_text "echo TAB_CLICK_FIRST_FOCUS"
press_keys Return
if ! scribe-test wait-output "$SESSION" "TAB_CLICK_FIRST_FOCUS" --timeout 15000 >/dev/null 2>&1; then
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
# `wait-output` cannot observe this session — the client created it, so the
# test daemon does not own it. The wire record is the focus oracle instead:
# every typed key must be routed to $NEW_SESSION by the client's encoder.
KEYS_NEW_BEFORE=$(count_keys_to "$NEW_SESSION")
type_text "echo TAB_CLICK_SECOND_FOCUS"
press_keys Return
if ! wait_for_count_growth "count_keys_to '$NEW_SESSION'" "$(( KEYS_NEW_BEFORE + 20 ))" 15; then
    fail "phase 5: the second tab click switched tabs but left keyboard focus outside the terminal"
fi
shot /output/05-tab-switching-click-second.png
echo "PHASE 5 PASS: second titlebar tab attached $NEW_SESSION and kept typing live"

# ── Phase 6: split into a second workspace region ─────────────────
# ctrl+alt+backslash is the shipped workspace_split_vertical default. The new
# region asks the server for its own workspace and a session to seed it, so
# both a CreateWorkspace and a SessionCreated must land on the wire.
CREATE_WS_BEFORE=$(count_client CreateWorkspace)
CREATED_WS_BEFORE=$(count_server SessionCreated)
send_keys ctrl+alt+backslash
if ! wait_for_count_growth "count_client CreateWorkspace" "$CREATE_WS_BEFORE" 15; then
    fail "phase 6: ctrl+alt+backslash never sent CreateWorkspace"
fi
if ! wait_for_count_growth "count_server SessionCreated" "$CREATED_WS_BEFORE" 15; then
    fail "phase 6: the workspace split never created its seed session"
fi
WS_SESSION=$(latest_created_session_after "$CREATED_WS_BEFORE") \
    || fail "phase 6: could not identify the workspace-split seed session"
# The seed session is created through the old workspace and only moved into
# the new region once the pane adopts it and the server's WorkspaceInfo has
# re-keyed the region; the MoveSession frame is that settled point.
if ! wait_for_count_growth "count_client MoveSession" 0 15; then
    fail "phase 6: the seed session was never moved into the new workspace region"
fi
shot /output/06-tab-switching-workspace-split.png
echo "PHASE 6 PASS: workspace split created region session $WS_SESSION"

# ── Phase 7: keyboard-select tab 1 across workspace regions ───────
# Focus sits in the new (second) region; alt+1 targets the strip's first tab,
# whose session lives in the FIRST region. The switch must cross regions and
# attach the original session.
ATTACH_XREGION_KEY_BEFORE=$(count_attach_to "$SESSION")
send_keys alt+1
if ! wait_for_attach_to "$SESSION" "$ATTACH_XREGION_KEY_BEFORE" 15; then
    fail "phase 7: alt+1 never attached the first region's session $SESSION"
fi
shot /output/07-tab-switching-key-cross-region.png
echo "PHASE 7 PASS: alt+1 attached $SESSION across workspace regions"

# ── Phase 8: click the first tab across workspace regions ─────────
# Move focus back to the second region's session first (alt+3), then click the
# first tab. In multi-workspace mode the badge pill shifts the strip right by
# its rendered width; 135 px sits inside the first tab for any badge label up
# to ~19 characters (badge <= ~134 px, tab spans [badge, badge+176]).
ATTACH_WS_BEFORE=$(count_attach_to "$WS_SESSION")
send_keys alt+3
if ! wait_for_attach_to "$WS_SESSION" "$ATTACH_WS_BEFORE" 15; then
    fail "phase 8: alt+3 never re-attached the second region's session $WS_SESSION"
fi
BADGED_FIRST_TAB_X=135
ATTACH_XREGION_CLICK_BEFORE=$(count_attach_to "$SESSION")
click_terminal_at "$BADGED_FIRST_TAB_X" "$TITLEBAR_Y"
if ! wait_for_attach_to "$SESSION" "$ATTACH_XREGION_CLICK_BEFORE" 15; then
    fail "phase 8: clicking the first titlebar tab never attached $SESSION across regions"
fi
type_text "echo TAB_CLICK_XREGION_FOCUS"
press_keys Return
if ! scribe-test wait-output "$SESSION" "TAB_CLICK_XREGION_FOCUS" --timeout 15000 >/dev/null 2>&1; then
    fail "phase 8: the cross-region tab click switched tabs but left keyboard focus outside the terminal"
fi
shot /output/08-tab-switching-click-cross-region.png
echo "PHASE 8 PASS: first titlebar tab attached $SESSION across regions and kept typing live"

# ── Phase 9: relaunch the client and adopt the server topology ────
# The user-visible regression surfaced after a client restart, when the fresh
# window rebuilds its regions from the server's persisted workspace tree
# (adopt_server_topology → PaneShell::adopt_server_tree). Kill the running
# client and relaunch it so the new window goes through that adoption path
# with two regions and three sessions.
kill "${SCRIBE_CLIENT_PID:?the entrypoint must export SCRIBE_CLIENT_PID}" 2>/dev/null || true
for _ in $(seq 1 30); do
    kill -0 "$SCRIBE_CLIENT_PID" 2>/dev/null || break
    sleep 0.2
done
ADOPT_BEFORE=$(grep -ac "rebuilt workspace splits from the server's tree" "$CLIENT_LOG" 2>/dev/null || true)
scribe-client >>"$CLIENT_LOG" 2>&1 &
RELAUNCHED_CLIENT_PID=$!
trap 'kill "${RELAUNCHED_CLIENT_PID:-0}" 2>/dev/null || true' EXIT
if ! wait_for_count_growth "grep -ac \"rebuilt workspace splits from the server's tree\" '$CLIENT_LOG' 2>/dev/null || true" "$ADOPT_BEFORE" 25; then
    fail "phase 9: the relaunched client never adopted the server workspace tree"
fi
sleep 2
shot /output/09-tab-switching-adopted.png
echo "PHASE 9 PASS: relaunched client rebuilt two regions from the server tree"

# ── Phase 10: cross-region tab click on the adopted layout ────────
# The adopted strip's order follows the server's SessionList, which is not
# guaranteed to be creation order, so this phase is order-independent: alt+3
# moves the selection off the first tab (a fresh SessionList always activates
# tab 0), the click on tab 0 must then attach a DIFFERENT session, and every
# typed key must be routed to that clicked session.
latest_attach_session() {
    python3 - "$RECORD" <<'PY'
import json
import sys

found = None
with open(sys.argv[1]) as handle:
    for line in handle:
        try:
            row = json.loads(line)
        except ValueError:
            continue
        if row.get("dir") != "client":
            continue
        message = row.get("message", {})
        if message.get("type") == "AttachSessions" and message.get("session_ids"):
            found = message["session_ids"][0]
if found is None:
    sys.exit(1)
print(found)
PY
}
ATTACH_ANY_BEFORE=$(count_client AttachSessions)
send_keys alt+3
if ! wait_for_count_growth "count_client AttachSessions" "$ATTACH_ANY_BEFORE" 15; then
    fail "phase 10: alt+3 never switched tabs on the adopted layout"
fi
ALT3_SESSION=$(latest_attach_session) || fail "phase 10: no attach recorded after alt+3"
ATTACH_ANY_BEFORE=$(count_client AttachSessions)
click_terminal_at "$BADGED_FIRST_TAB_X" "$TITLEBAR_Y"
if ! wait_for_count_growth "count_client AttachSessions" "$ATTACH_ANY_BEFORE" 15; then
    fail "phase 10: clicking the first titlebar tab attached nothing on the adopted layout"
fi
CLICKED_SESSION=$(latest_attach_session) || fail "phase 10: no attach recorded after the click"
if [ "$CLICKED_SESSION" = "$ALT3_SESSION" ]; then
    fail "phase 10: the tab click re-attached $ALT3_SESSION instead of switching tabs"
fi
KEYS_CLICKED_BEFORE=$(count_keys_to "$CLICKED_SESSION")
type_text "echo TAB_CLICK_ADOPTED_FOCUS"
press_keys Return
if ! wait_for_count_growth "count_keys_to '$CLICKED_SESSION'" "$(( KEYS_CLICKED_BEFORE + 20 ))" 15; then
    fail "phase 10: the adopted-layout tab click switched tabs but left keyboard focus outside the terminal"
fi
shot /output/10-tab-switching-click-adopted.png
echo "PHASE 10 PASS: cross-region tab click stays live on the adopted layout (clicked $CLICKED_SESSION)"

# ── Phase 11: a jittered pointer click still selects a tab ────────
# Real mouse clicks always travel a few px between press and release; that
# travel engages GPUI's native tab drag and cancels the element-level click
# (this swallowed every real-mouse tab click while the suite's zero-jitter
# clicks stayed green). An engaged drag that never reordered must select the
# pressed tab on release.
ATTACH_ANY_BEFORE=$(count_client AttachSessions)
send_keys alt+3
if ! wait_for_count_growth "count_client AttachSessions" "$ATTACH_ANY_BEFORE" 15; then
    fail "phase 11: alt+3 never moved the selection off the first tab"
fi
JITTER_BASE_SESSION=$(latest_attach_session) \
    || fail "phase 11: no attach recorded after alt+3"
ATTACH_ANY_BEFORE=$(count_client AttachSessions)
jitter_click_terminal_at "$BADGED_FIRST_TAB_X" "$TITLEBAR_Y"
if ! wait_for_count_growth "count_client AttachSessions" "$ATTACH_ANY_BEFORE" 15; then
    fail "phase 11: the jittered first-tab click attached nothing (click swallowed by the drag system)"
fi
JITTER_SESSION=$(latest_attach_session) \
    || fail "phase 11: no attach recorded after the jittered click"
if [ "$JITTER_SESSION" = "$JITTER_BASE_SESSION" ]; then
    fail "phase 11: the jittered click re-attached $JITTER_BASE_SESSION instead of selecting the clicked tab"
fi
KEYS_JITTER_BEFORE=$(count_keys_to "$JITTER_SESSION")
type_text "echo TAB_JITTER_CLICK_FOCUS"
press_keys Return
if ! wait_for_count_growth "count_keys_to '$JITTER_SESSION'" "$(( KEYS_JITTER_BEFORE + 20 ))" 15; then
    fail "phase 11: the jittered click selected a tab but left keyboard focus outside the terminal"
fi
shot /output/11-tab-switching-jitter-click.png
echo "PHASE 11 PASS: jittered pointer click still selects (attached $JITTER_SESSION)"

echo ""
echo "PASS: tab switching and post-click typing stay live for keyboard and titlebar"
