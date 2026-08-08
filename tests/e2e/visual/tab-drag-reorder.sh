#!/bin/bash
[ "${SCRIBE_E2E_SANDBOX:-0}" = "1" ] || { echo "FATAL: this script only runs inside the scribe e2e container (use just e2e-func / e2e-visual)." >&2; exit 99; }
# Scripted E2E: dragging a tab inside its own region reorders the strip and
# makes that new order durable.
#
# Tab drag-reorder had unit coverage on both halves and none end to end: the
# titlebar suite proves a synthetic drag emits `TitlebarEvent::ReorderTab`, and
# `TabSessions::reorder` proves the model moves an entry. Nothing exercised the
# handler that joins them, which is where the index translation and the wire
# report live — so a break between the two halves was invisible.
#
# The oracle is the wire, not a screenshot. A `ReportWorkspaceTree` frame
# carries each region's `session_ids` in the user's tab order, which is exactly
# what a drag changes and exactly what a restart later replays; a screenshot
# only shows tab titles, which are identical shells here.
#
# Requires: visual container with SCRIBE_SHARE_TAP=1 and SCRIBE_SHARED_PANE=1;
# xdotool, python3.
set -e

# @lat: [[test#Test Harness#Visual E2E Tests#Tab drag reorders in every bar]]
CLIENT_LOG="${SCRIBE_CLIENT_LOG:-/output/client.log}"
RECORD="${SHARE_WIRE_RECORD:-/output/share-wire.jsonl}"

# Titlebar geometry from crates/scribe-client/src/titlebar.rs: a 34 px band of
# fixed 176 px tabs. Single-workspace rig, so no badge offsets the strip.
TITLEBAR_HEIGHT=34
TAB_WIDTH=176
TITLEBAR_Y=$(( TITLEBAR_HEIGHT / 2 ))

TERM_X=0
TERM_Y=0
WID=""

fail() {
    echo "$1" >&2
    [ -f "$CLIENT_LOG" ] && tail -40 "$CLIENT_LOG" >&2
    exit 1
}

find_window() {
    xdotool search --onlyvisible --class '[Ss]cribe' 2>/dev/null \
        || xdotool search --onlyvisible --name '[Ss]cribe' 2>/dev/null || true
}

# Re-reads the client-area origin every time, the way tab-switching.sh does:
# `xdotool getwindowgeometry` reports the frame, so tab coordinates derived from
# it land on the window manager's titlebar instead of Scribe's own band.
focus_terminal() {
    xdotool windowactivate --sync "$WID" 2>/dev/null \
        || xdotool windowfocus --sync "$WID" 2>/dev/null || true
    sleep 0.8
    local info
    info=$(xwininfo -id "$WID")
    TERM_X=$(printf '%s\n' "$info" | awk '/Absolute upper-left X/ { print $4 }')
    TERM_Y=$(printf '%s\n' "$info" | awk '/Absolute upper-left Y/ { print $4 }')
}

tab_center_x() { echo $(( $1 * TAB_WIDTH + TAB_WIDTH / 2 )); }

# The newest `ReportWorkspaceTree` the client put on the wire, as one line per
# region leaf (left to right) of that leaf's session ids in tab order. `$1`
# selects a leaf, defaulting to the first.
tree_tabs() {
    python3 - "$RECORD" "${1:-1}" <<'PY'
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


def leaves(node, out):
    if node is None:
        return
    if "Leaf" in node:
        out.append(node["Leaf"])
        return
    if "session_ids" in node:
        out.append(node)
        return
    inner = node.get("Split", node)
    for key in ("first", "second"):
        if key in inner:
            leaves(inner[key], out)


found = []
leaves(latest, found)
wanted = int(sys.argv[2])
if len(found) < wanted:
    sys.exit(0)
print(*found[wanted - 1].get("session_ids", []))
PY
}

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

# A real pointer drag: press on the source tab, cross GPUI's ~2 px drag
# threshold, walk to the target slot in steps so the titlebar sees intermediate
# positions the way a human drag produces them, then release.
drag_tab() {
    local from_x="$1" to_x="$2" step
    focus_terminal
    xdotool mousemove "$(( TERM_X + from_x ))" "$(( TERM_Y + TITLEBAR_Y ))"
    sleep 0.3
    xdotool mousedown 1
    sleep 0.2
    xdotool mousemove_relative -- 3 0
    sleep 0.2
    for step in $(seq 1 8); do
        xdotool mousemove \
            "$(( TERM_X + from_x + (to_x - from_x) * step / 8 ))" \
            "$(( TERM_Y + TITLEBAR_Y ))"
        sleep 0.1
    done
    sleep 0.3
    xdotool mouseup 1
    sleep 1.0
}

# ── Phase 0: three tabs in one region ────────────────────────────
for _ in $(seq 1 40); do
    WID=$(find_window | head -1)
    [ -n "$WID" ] && break
    sleep 0.5
done
[ -n "$WID" ] || fail "PHASE 0 FAIL: no Scribe window appeared"

focus_terminal
xdotool key --clearmodifiers ctrl+shift+t
sleep 1.5
xdotool key --clearmodifiers ctrl+shift+t
sleep 1.5

if ! wait_for_tab_count 3 30; then
    fail "PHASE 0 FAIL: the client never reported a three-tab region"
fi
BEFORE=$(tree_tabs)
echo "PHASE 0 PASS: three tabs reported in order [$BEFORE]"

# ── Phase 1: drag the first tab onto the third slot ──────────────
FIRST=$(echo "$BEFORE" | cut -d' ' -f1)
SECOND=$(echo "$BEFORE" | cut -d' ' -f2)
THIRD=$(echo "$BEFORE" | cut -d' ' -f3)
EXPECTED="$SECOND $THIRD $FIRST"

drag_tab "$(tab_center_x 0)" "$(tab_center_x 2)"

AFTER=""
for _ in $(seq 1 25); do
    AFTER=$(tree_tabs)
    [ "$AFTER" = "$EXPECTED" ] && break
    sleep 0.4
done

if [ "$AFTER" = "$BEFORE" ]; then
    fail "PHASE 1 FAIL: the drag did not reorder the strip (still [$BEFORE])"
fi
if [ "$AFTER" != "$EXPECTED" ]; then
    fail "PHASE 1 FAIL: expected [$EXPECTED] after the drag, reported [$AFTER]"
fi
echo "PHASE 1 PASS: drag moved the first tab to the last slot [$AFTER]"

# ── Phase 2: the new order is what the report carries ────────────
# The drag has to reach the wire, not just the titlebar: the reported tree is
# the only place tab order is durable, so a drag that never reports is a drag
# that dies at the next reconnect.
REPORTS=$(grep -c '"ReportWorkspaceTree"' "$RECORD" 2>/dev/null || echo 0)
[ "$REPORTS" -gt 0 ] || fail "PHASE 2 FAIL: the client never reported a workspace tree"
echo "PHASE 2 PASS: reorder reached the wire ($REPORTS tree reports)"

# ── Phase 3: a tab in a LOWER region's own bar ───────────────────
# The window titlebar only carries the top region's tabs; every region below it
# renders its own bar, and those bars shipped without any drag wiring at all —
# so the tabs a split-window user works with were the only ones in the window
# that could not be reordered. This is that case, not a variation of it.
# Layout chords are dispatched from the pane's focus handle, and the drag above
# left GPUI focus on a titlebar tab, so the grid has to take focus back first.
# `$1` is the fraction of client height to click at, in eighths: 4 is the
# window's middle (the split boundary once stacked), 6 is inside the lower
# region. A new tab opens in whichever region owns focus, so this is what
# decides where the tabs below land.
click_pane() {
    local height
    focus_terminal
    height=$(xwininfo -id "$WID" | awk '/Height:/ { print $2 }')
    xdotool mousemove "$(( TERM_X + 200 ))" "$(( TERM_Y + height * $1 / 8 ))"
    sleep 0.3
    xdotool click 1
    sleep 0.6
}

# A workspace split has to be the HORIZONTAL one: `is_lower_region` is
# `rect.y > 0.5`, so only stacked regions get their own tab bar. A vertical
# split leaves both regions on the top row and their tabs in the titlebar.
click_pane 4
xdotool key --clearmodifiers ctrl+alt+minus
sleep 3.0
if ! grep -q "split the window into a new workspace region" "$CLIENT_LOG"; then
    fail "PHASE 3 FAIL: ctrl+alt+minus never reached workspace_split_horizontal"
fi

# Focus the lower region itself, so both new tabs are filed under it.
click_pane 6
xdotool key --clearmodifiers ctrl+shift+t
sleep 2.0
click_pane 6
xdotool key --clearmodifiers ctrl+shift+t
sleep 2.0

LOWER_BEFORE=""
for _ in $(seq 1 30); do
    LOWER_BEFORE=$(tree_tabs 2)
    [ "$(echo "$LOWER_BEFORE" | wc -w)" -eq 3 ] && break
    sleep 0.5
done
if [ "$(echo "$LOWER_BEFORE" | wc -w)" -ne 3 ]; then
    fail "PHASE 3 FAIL: the lower region never reported three tabs (got [$LOWER_BEFORE])"
fi
TOP_BEFORE=$(tree_tabs 1)

# The lower region's bar sits at the top of its own rect. A horizontal split
# halves the window, so that band starts at half the client height; aim at its
# middle, which is REGION_TAB_BAR_HEIGHT (34 px, the titlebar band) tall.
CLIENT_H=$(xwininfo -id "$WID" | awk '/Height:/ { print $2 }')
LOWER_BAR_Y=$(( CLIENT_H / 2 + TITLEBAR_HEIGHT / 2 ))

L_FIRST=$(echo "$LOWER_BEFORE" | cut -d' ' -f1)
L_SECOND=$(echo "$LOWER_BEFORE" | cut -d' ' -f2)
L_THIRD=$(echo "$LOWER_BEFORE" | cut -d' ' -f3)
LOWER_EXPECTED="$L_SECOND $L_THIRD $L_FIRST"

TITLEBAR_Y=$LOWER_BAR_Y
drag_tab "$(tab_center_x 0)" "$(tab_center_x 2)"

LOWER_AFTER=""
for _ in $(seq 1 25); do
    LOWER_AFTER=$(tree_tabs 2)
    [ "$LOWER_AFTER" = "$LOWER_EXPECTED" ] && break
    sleep 0.4
done

if [ "$LOWER_AFTER" = "$LOWER_BEFORE" ]; then
    fail "PHASE 3 FAIL: a drag in the lower region's bar did nothing (still [$LOWER_BEFORE])"
fi
if [ "$LOWER_AFTER" != "$LOWER_EXPECTED" ]; then
    fail "PHASE 3 FAIL: expected [$LOWER_EXPECTED] in the lower region, got [$LOWER_AFTER]"
fi

# A drag is scoped to its own region: the strip is window-global, so a swap
# applied at the wrong offset would silently reshuffle the region above.
TOP_AFTER=$(tree_tabs 1)
if [ "$TOP_AFTER" != "$TOP_BEFORE" ]; then
    fail "PHASE 3 FAIL: the top region moved too ([$TOP_BEFORE] -> [$TOP_AFTER])"
fi
echo "PHASE 3 PASS: lower-region drag reordered only its own bar [$LOWER_AFTER]"

echo "PASS: tab drag reorders within a region, in the titlebar and in region bars"
