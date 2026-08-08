#!/bin/bash
[ "${SCRIBE_E2E_SANDBOX:-0}" = "1" ] || { echo "FATAL: this script only runs inside the scribe e2e container (use just e2e-func / e2e-visual)." >&2; exit 99; }
# Scripted E2E: a restart puts the window back where it was, with its tabs in
# the order the user left them and the tab they were on still selected.
#
# Both halves used to be lost, and both were lost in the *reporting*, not in the
# restore:
#
#   * `WorkspaceTreeNode::Leaf` carries `session_ids` ("Ordered session IDs for
#     tabs in this workspace"), `pane_trees` parallel to it, and
#     `active_tab_index`. The client filled in only the one session its visible
#     pane was showing and hardcoded `active_tab_index: 0`, so the server — the
#     only thing that persists a live window's layout — was told a one-element
#     order and never learned which tab was active. The strip was then rebuilt
#     from `SessionList`, whose order came out of a `HashMap` of workspaces.
#
#   * The window position handed to `open_window` is a hint. GPUI's X11 backend
#     sets no `USPosition`/`PPosition` size hint, so a window manager is free to
#     place the window itself, and Mutter does — every restored window came back
#     on the active monitor. The client now re-asserts the saved position once
#     the window is mapped.
#
# The wire tap is what makes the first half observable: the report is a frame,
# not a pixel, and reading it is the only way to assert the *whole* tab list
# rather than the one tab that happens to be on screen.
#
# Phases:
#   0. one window with one adopted tab;
#   1. two more tabs — the report names all three, in strip order;
#   2. switching tabs moves `active_tab_index` instead of pinning it at 0;
#   3. the window is moved, and the geometry record follows it;
#   4. after a restart the window is back at that position, with the same tab
#      order and the same active tab.
#
# Requires: visual container with SCRIBE_SHARE_TAP=1; xdotool, scrot, python3.
set -e

CLIENT_LOG="${SCRIBE_CLIENT_LOG:-/output/client.log}"
RECORD="${SHARE_WIRE_RECORD:-/output/share-wire.jsonl}"
STATE_DIR="${XDG_STATE_HOME:?the entrypoint must export XDG_STATE_HOME}/scribe"
GEOMETRY_DIR="$STATE_DIR/windows"

# Somewhere the window manager would not have chosen on its own, and far enough
# from the default placement that a restored window landing there cannot be a
# coincidence.
MOVE_X=320
MOVE_Y=180

fail() {
    echo "FAIL: $1" >&2
    echo "--- client log tail ---" >&2
    tail -60 "$CLIENT_LOG" >&2 || true
    echo "--- last reported tree ---" >&2
    reported_tree >&2 || true
    exit 1
}

count_log() { grep -c "$1" "$CLIENT_LOG" 2>/dev/null || true; }

wait_for_log_growth() {
    local pattern="$1" baseline="$2" timeout_secs="$3" started
    started=$(date +%s)
    while [ "$(count_log "$pattern")" -le "$baseline" ]; do
        if [ $(( "$(date +%s)" - started )) -ge "$timeout_secs" ]; then
            return 1
        fi
        sleep 0.3
    done
    return 0
}

# The most recent `ReportWorkspaceTree` the client put on the wire, printed as
# `<active_tab_index> <session_id> <session_id> …` for the FIRST leaf of the
# tree. One region is all these phases build, so the first leaf is the region.
reported_tree() {
    python3 - "$RECORD" <<'PY'
import json, sys

latest = None
try:
    handle = open(sys.argv[1])
except OSError:
    sys.exit(0)
with handle:
    for line in handle:
        try:
            row = json.loads(line)
        except ValueError:
            continue
        message = row.get("message", {})
        if row.get("dir") == "client" and message.get("type") == "ReportWorkspaceTree":
            latest = message.get("tree")
if latest is None:
    sys.exit(0)


def first_leaf(node):
    if "Leaf" in node:
        return node["Leaf"]
    if "Split" in node:
        return first_leaf(node["Split"]["first"])
    # Untagged shapes: a leaf is the node that carries session_ids.
    if "session_ids" in node:
        return node
    for key in ("first", "second"):
        if key in node:
            found = first_leaf(node[key])
            if found:
                return found
    return None


leaf = first_leaf(latest)
if not leaf:
    sys.exit(0)
print(leaf.get("active_tab_index", 0), *leaf.get("session_ids", []))
PY
}

tree_tabs() { reported_tree | cut -d' ' -f2-; }
tree_active_index() { reported_tree | cut -d' ' -f1; }

# Wait until the newest report names `want` tabs.
wait_for_tab_count() {
    local want="$1" timeout_secs="$2" started seen
    started=$(date +%s)
    while :; do
        seen=$(tree_tabs | wc -w)
        [ "$seen" -eq "$want" ] && return 0
        if [ $(( "$(date +%s)" - started )) -ge "$timeout_secs" ]; then
            echo "  (report names $seen tabs, wanted $want)" >&2
            return 1
        fi
        sleep 0.4
    done
}

list_windows() {
    xdotool search --onlyvisible --class '[Ss]cribe' 2>/dev/null \
        || xdotool search --onlyvisible --name '[Ss]cribe' 2>/dev/null || true
}
find_window() { list_windows | tail -1; }

focus() {
    local wid
    wid=$(find_window)
    [ -n "$wid" ] || fail "no Scribe window found"
    xdotool windowactivate --sync "$wid" 2>/dev/null \
        || xdotool windowfocus --sync "$wid" 2>/dev/null || true
    # Past the X11 focus guard's 300 ms reactivation debounce.
    sleep 0.8
}

window_position() {
    local wid
    wid=$(find_window)
    [ -n "$wid" ] || return 1
    eval "$(xdotool getwindowgeometry --shell "$wid")"
    printf '%s %s' "$X" "$Y"
}

shot() {
    sleep 0.3
    scrot -o "$1"
    echo "captured $1"
}

send_keys() {
    xdotool key --clearmodifiers "$@"
    sleep 0.6
}

wait_for_client_exit() {
    local timeout_secs="$1" started
    started=$(date +%s)
    while pgrep -f 'scribe-client' >/dev/null 2>&1; do
        if [ $(( "$(date +%s)" - started )) -ge "$timeout_secs" ]; then
            return 1
        fi
        sleep 0.3
    done
    return 0
}

launch_client() {
    scribe-client >>"$CLIENT_LOG" 2>&1 &
    xdotool search --sync --name "Scribe" >/dev/null 2>&1 || true
    sleep 2
}

# ── Phase 0: one window with one adopted tab ──────────────────────
# Closed, not killed: a killed client leaves its login shell in a window the
# server keeps, and the relaunch below would reopen that window too.
sleep 1.0
focus
send_keys ctrl+shift+d
send_keys Tab
send_keys Tab
send_keys Return
wait_for_client_exit 20 || fail "PHASE 0: the entrypoint's client did not close"
scribe-test daemon stop >/dev/null 2>&1 || true
sleep 1.0
launch_client
focus
wait_for_tab_count 1 25 || fail "PHASE 0: the client never reported its adopted tab"
echo "PHASE 0 PASS: one window, one tab"

# ── Phase 1: the report names every tab, not just the visible one ─
for _ in 1 2; do
    TABS_BEFORE=$(count_log "requested initial shell session")
    send_keys ctrl+shift+t
    sleep 1.5
done
wait_for_tab_count 3 25 \
    || fail "PHASE 1: the reported tree does not name all three tabs"
ORDER_BEFORE=$(tree_tabs)
echo "PHASE 1 PASS: the report names all three tabs ($ORDER_BEFORE)"

# ── Phase 2: the active tab is reported, not pinned at 0 ──────────
# A new tab is appended and focused, so the strip is on the last one now.
LAST_INDEX=$(tree_active_index)
[ "$LAST_INDEX" = "2" ] \
    || fail "PHASE 2: after opening two tabs the active index is $LAST_INDEX, expected 2"
send_keys alt+1
sleep 1.5
FIRST_INDEX=$(tree_active_index)
[ "$FIRST_INDEX" = "0" ] \
    || fail "PHASE 2: after selecting tab 1 the active index is $FIRST_INDEX, expected 0"
# Back to the middle tab, which is the one the restart must restore.
send_keys alt+2
sleep 1.5
ACTIVE_BEFORE=$(tree_active_index)
[ "$ACTIVE_BEFORE" = "1" ] \
    || fail "PHASE 2: after selecting tab 2 the active index is $ACTIVE_BEFORE, expected 1"
shot /output/00-three-tabs.png
echo "PHASE 2 PASS: the active tab index follows the selection (2 -> 0 -> 1)"

# ── Phase 3: move the window; the geometry record follows ─────────
WID=$(find_window)
xdotool windowmove "$WID" "$MOVE_X" "$MOVE_Y"
sleep 3.0
POS_BEFORE=$(window_position)
echo "PHASE 3 PASS: window moved to $POS_BEFORE"

# ── Phase 4: a restart restores position, order, and active tab ───
pkill -TERM -f 'scribe-client' || true
wait_for_client_exit 20 || fail "PHASE 4: the client did not exit"
: >"$RECORD"
launch_client
focus
wait_for_tab_count 3 30 || fail "PHASE 4: the restarted client did not restore three tabs"

ORDER_AFTER=$(tree_tabs)
[ "$ORDER_AFTER" = "$ORDER_BEFORE" ] \
    || fail "PHASE 4: tab order came back as [$ORDER_AFTER], expected [$ORDER_BEFORE]"
ACTIVE_AFTER=$(tree_active_index)
[ "$ACTIVE_AFTER" = "$ACTIVE_BEFORE" ] \
    || fail "PHASE 4: active tab came back as $ACTIVE_AFTER, expected $ACTIVE_BEFORE"

POS_AFTER=$(window_position)
[ "$POS_AFTER" = "$POS_BEFORE" ] \
    || fail "PHASE 4: window came back at [$POS_AFTER], expected [$POS_BEFORE]"
shot /output/01-restored.png
echo "PHASE 4 PASS: position $POS_AFTER, order [$ORDER_AFTER], active tab $ACTIVE_AFTER"

# ── Phase 5: a second restart does not walk the window ────────────
# The saved record is the content origin while a reparenting window manager
# positions its frame, so an uncorrected restore moves the window down and right
# by one decoration EVERY time. One restart hides that; two do not.
pkill -TERM -f 'scribe-client' || true
wait_for_client_exit 20 || fail "PHASE 5: the client did not exit"
launch_client
focus
wait_for_tab_count 3 30 || fail "PHASE 5: the second restart did not restore three tabs"
POS_TWICE=$(window_position)
[ "$POS_TWICE" = "$POS_BEFORE" ] \
    || fail "PHASE 5: window drifted to [$POS_TWICE] on the second restart, expected [$POS_BEFORE]"
echo "PHASE 5 PASS: a second restart landed at $POS_TWICE again"

echo ""
echo "PASS: visual layout-restore test"
echo "  Inspect screenshots in test-output/:"
echo "    00-three-tabs.png — three tabs with the middle one active"
echo "    01-restored.png   — the same window after a restart"
