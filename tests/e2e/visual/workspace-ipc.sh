#!/bin/bash
[ "${SCRIBE_E2E_SANDBOX:-0}" = "1" ] || { echo "FATAL: this script only runs inside the scribe e2e container (use just e2e-func / e2e-visual)." >&2; exit 99; }
# Scripted E2E: the workspace IPC of the GPUI client, asserted on the wire
# against a real client and a real server.
#
# Backs the `ClientMessage::CreateWorkspace`, `CloseWorkspace`, `MoveSession`
# and `ReportWorkspaceTree` parity rows, plus the inbound
# `ServerMessage::WorkspaceInfo` row. Bead .58 gave the window a real workspace
# tree, but every region it opened was pure client-side layout: the client never
# asked the server for a workspace, never told it one went away, never said
# which region a session ended up in, and never reported the tree the server
# persists for reconnect. The `WorkspaceInfo` answer fell into the reader's
# drop counter.
#
# A green `#[gpui::test]` cannot tell that state apart from this one — the
# workspace-tree model passes either way — so every phase below drives the real
# window and asserts on frames the wire tap actually recorded.
#
# The wire tap (`scribe-test share-tap`, SCRIBE_SHARE_TAP=1) is interposed on
# the server socket, so every frame the client sends and every frame the server
# answers with is recorded as JSONL, and `scribe-test share-inject` can push a
# synthetic server frame at the client. The record is truncated at the phase
# boundary, so a frame found afterwards can only have been produced by the
# action that phase performed.
#
# Phases:
#   * Ctrl+Alt+\ splits the window into a second workspace region: the client
#     puts `CreateWorkspace` on the wire, the server answers `WorkspaceInfo`,
#     and the client adopts that id onto the region it just opened;
#   * the same split reports the new two-region tree with `ReportWorkspaceTree`;
#   * the session the split seeded is created through the FIRST region's
#     workspace and adopted by a pane in the SECOND, so the client reconciles
#     the two with `MoveSession` naming the workspace the server just minted;
#   * an injected `WorkspaceInfo` carrying a display name repaints the status
#     bar's workspace segment, and injecting the same frame with no name
#     repaints it back — the render follows the payload, not a one-shot latch;
#   * Ctrl+Shift+W closes the second region's only pane, collapsing the region,
#     and the client puts `CloseWorkspace` for that exact id on the wire.
#
# Phase 0 is the same session-adoption dance `pane-workspace-layout.sh`
# documents: the entrypoint creates $SESSION after the client launched, so the
# running client never hears about it, and only a relaunch after the test daemon
# releases ownership picks it up through `ListSessions`.
#
# Input is driven through XTEST (plain `xdotool key`, no `--window`). GPUI reads
# the keyboard through XInput2 and ignores the synthetic events
# `xdotool --window` sends with XSendEvent.
#
# Requires: visual container (docker/entrypoint-visual.sh) with
# SCRIBE_SHARE_TAP=1, xdotool, scrot, imagemagick, python3.
set -e

RECORD="${SHARE_WIRE_RECORD:-/output/share-wire.jsonl}"
ORACLE=/tests/visual/workspace-tree-oracle.py
oracle() { python3 "$ORACLE" "$RECORD" "$@"; }
CONTROL="${SHARE_TAP_CONTROL:-$XDG_RUNTIME_DIR/scribe/share-tap.sock}"
CLIENT_LOG="${SCRIBE_CLIENT_LOG:-/output/client.log}"
SERVER_LOG="${SCRIBE_SERVER_LOG:-/output/server.log}"

# The display name injected through the tap. Long enough that rendering it
# shifts every segment to its right, so the repaint is unmistakable.
WS_NAME="WSINFO-RENDER-OK"

# Fallback accent injected alongside the name, used only if the recorded
# `WorkspaceInfo` carried none. The real run echoes the server's own accent back
# so the only thing that changes between the two captures is the name itself.
WS_ACCENT="#a78bfa"

# Height of the band at the very bottom of the painted window that the status
# bar occupies (window_chrome::STATUS_BAR_HEIGHT is 24 px), plus a little slack
# for whatever border the window manager draws under it.
STATUS_BAND_H=34

# Fraction of the window width the assertions look at, measured from the left
# edge. The workspace name is the first variable-width segment of the status
# bar's left group, so it lands well inside this slice; the system sparklines
# and their percentages sit outside it and repaint on their own 2 s cadence,
# which would otherwise swamp the comparison.
LEFT_FRACTION_NUM=25
LEFT_FRACTION_DEN=100

# Differing pixels rendering a 16-character workspace name must add to the
# status bar's left group. One glyph is dozens of lit pixels and the name also
# shifts the CWD segment right, so the real delta is far above this.
NAME_DIFF_MIN="${NAME_DIFF_MIN:-120}"

# Differing pixels the workspace split itself must add to the window. A split
# reflows the grid and redraws the focus ring, which is far more than this.
SPLIT_DIFF_MIN="${SPLIT_DIFF_MIN:-2000}"

WIN_X=0
WIN_Y=0
WIN_W=0
WIN_H=0

find_window() {
    local wid
    wid=$(xdotool search --class '[Ss]cribe' 2>/dev/null | tail -1)
    [ -z "$wid" ] && wid=$(xdotool search --name '[Ss]cribe' 2>/dev/null | tail -1)
    printf '%s' "$wid"
}

# Focus the client and cache its on-screen geometry so a full-screen capture can
# be cropped down to the window, and the window down to one of its bands.
focus() {
    local wid
    wid=$(find_window)
    if [ -z "$wid" ]; then
        echo "FAIL: no Scribe window found"
        exit 1
    fi
    xdotool windowactivate --sync "$wid" 2>/dev/null \
        || xdotool windowfocus --sync "$wid" 2>/dev/null || true
    sleep 0.3
    eval "$(xdotool getwindowgeometry --shell "$wid")"
    WIN_X="$X"
    WIN_Y="$Y"
    WIN_W="$WIDTH"
    WIN_H="$HEIGHT"
}

shot() {
    sleep 0.4
    scrot -o "$1"
    echo "captured $1"
}

# Capture only the focused window's left status-bar band. A full-desktop trim
# can select another restored Scribe window when their frames overlap.
status_bar_shot() {
    local wid w
    wid=$(find_window)
    [ -n "$wid" ] || fail "no Scribe window found for status-bar capture"
    w=$(( WIN_W * LEFT_FRACTION_NUM / LEFT_FRACTION_DEN ))
    import -window "$wid" +repage miff:- \
        | convert - -gravity SouthWest -crop "${w}x${STATUS_BAND_H}+0+0" +repage "$1"
    echo "captured $1"
}

# Lit pixels inside the client window. Rendered text is near-white on a
# near-black background, so a luminance threshold separates ink from the pane.
window_ink() {
    local value
    value=$(convert "$1" -crop "${WIN_W}x${WIN_H}+${WIN_X}+${WIN_Y}" +repage \
        -colorspace Gray -threshold 35% -format "%[fx:mean*w*h]" info:)
    printf '%s' "${value%.*}"
}

# Count differing pixels between two captures, cropped to the client window.
window_diff() {
    local value
    value=$(compare -metric AE \
        \( "$1" -crop "${WIN_W}x${WIN_H}+${WIN_X}+${WIN_Y}" +repage \) \
        \( "$2" -crop "${WIN_W}x${WIN_H}+${WIN_X}+${WIN_Y}" +repage \) \
        null: 2>&1 || true)
    printf '%s' "${value%%.*}"
}

# Count differing pixels between two focused-window status-bar crops.
status_left_diff() {
    local value
    value=$(compare -metric AE "$1" "$2" null: 2>&1 || true)
    printf '%s' "${value%%.*}"
}

send_keys() {
    xdotool key --clearmodifiers "$@"
    sleep 0.5
}

inject() {
    scribe-test share-inject --control "$CONTROL" "$1"
    sleep 0.8
}

count_in() {
    grep -acF "$2" "$1" 2>/dev/null || true
}

count_log() {
    count_in "$CLIENT_LOG" "$1"
}

wait_for_log_growth() {
    local pattern="$1" baseline="$2" timeout_secs="${3:-15}" now started
    started=$(date +%s)
    while true; do
        now=$(count_log "$pattern")
        if [ "$now" -gt "$baseline" ]; then
            return 0
        fi
        if [ $(( "$(date +%s)" - started )) -ge "$timeout_secs" ]; then
            return 1
        fi
        sleep 0.3
    done
}

fail() {
    echo "$1"
    echo "--- recorded client frame types ---"
    frame_types client || true
    echo "--- recorded server frame types ---"
    frame_types server || true
    echo "--- client log tail ---"
    tail -60 "$CLIENT_LOG" || true
    echo "--- server log tail ---"
    tail -20 "$SERVER_LOG" || true
    exit 1
}

frame_types() {
    oracle frame-types "$1"
}

# Start a fresh window on the wire record so the frames a phase asserts on can
# only have come from that phase.
reset_record() {
    : >"$RECORD"
    sleep 0.2
}

# The shared oracle owns bounded frame waits and FIRST-frame field lookup.
# First rather than last on purpose: the record is truncated at each phase
# boundary and `CreateWorkspace` is the first frame the split sends, so the
# first `WorkspaceInfo` in the window is its answer. The server also fans a
# `WorkspaceInfo` out for the *existing* workspace when the split's session is
# created, and taking the last frame would pick that one up instead.
# Assert the last recorded client `ReportWorkspaceTree` carries a split with the
# given number of workspace leaves.
assert_reported_leaves() {
    oracle reported-leaves "$1"
}

# ── Phase 0: hand the relaunched client a live pane to act in ─────
sleep 1.0
kill "$SCRIBE_CLIENT_PID" 2>/dev/null || true
for _ in $(seq 1 40); do
    pgrep -f 'scribe-client' >/dev/null 2>&1 || break
    sleep 0.25
done
if pgrep -f 'scribe-client' >/dev/null 2>&1; then
    fail "PHASE 0 FAIL: the original client did not exit"
fi
scribe-test daemon stop >/dev/null 2>&1 || true
sleep 1.0
reset_record
scribe-client >>"$CLIENT_LOG" 2>&1 &
xdotool search --sync --name "Scribe" >/dev/null 2>&1 || true
sleep 2

BASE_INK=0
for _ in $(seq 1 40); do
    focus
    shot /output/00-attached.png >/dev/null
    BASE_INK=$(window_ink /output/00-attached.png)
    [ "$BASE_INK" -ge 20 ] && break
    sleep 0.5
done
if [ "$BASE_INK" -lt 20 ]; then
    fail "PHASE 0 FAIL: the client rendered no pane content (ink $BASE_INK)"
fi
echo "PHASE 0 PASS: client attached to session $SESSION (window ink $BASE_INK)"

# ── Phase 1: the workspace split asks the server for a workspace ──
WS_SPLITS_BEFORE=$(count_log "split the window into a new workspace region")
ADOPTS_BEFORE=$(count_log "a workspace region adopted a server workspace")
focus
shot /output/01-before-workspace-split.png
reset_record
send_keys ctrl+alt+backslash
if ! wait_for_log_growth "split the window into a new workspace region" "$WS_SPLITS_BEFORE" 15; then
    fail "PHASE 1 FAIL: ctrl+alt+backslash never reached workspace_split_vertical"
fi
if ! oracle wait-frame client CreateWorkspace "" 20; then
    fail "PHASE 1 FAIL: the split put no CreateWorkspace frame on the wire"
fi
if ! oracle wait-frame server WorkspaceInfo "" 20; then
    fail "PHASE 1 FAIL: the server never answered CreateWorkspace with WorkspaceInfo"
fi
if ! wait_for_log_growth "a workspace region adopted a server workspace" "$ADOPTS_BEFORE" 20; then
    fail "PHASE 1 FAIL: the new region never adopted the server's workspace id"
fi
NEW_WS=$(oracle first-frame-field server WorkspaceInfo workspace_id) \
    || fail "PHASE 1 FAIL: the recorded WorkspaceInfo carries no workspace_id"
sleep 1.5
focus
shot /output/02-after-workspace-split.png
WS_DIFF=$(window_diff /output/01-before-workspace-split.png /output/02-after-workspace-split.png)
if [ "${WS_DIFF:-0}" -lt "$SPLIT_DIFF_MIN" ]; then
    fail "PHASE 1 FAIL: the workspace split changed $WS_DIFF px (min $SPLIT_DIFF_MIN)"
fi
echo "PHASE 1 PASS: CreateWorkspace crossed the wire and the region adopted $NEW_WS (+$WS_DIFF px)"

# ── Phase 2: the split reports the two-region tree ────────────────
if ! oracle wait-frame client ReportWorkspaceTree "$NEW_WS" 20; then
    fail "PHASE 2 FAIL: no client ReportWorkspaceTree frame naming $NEW_WS"
fi
assert_reported_leaves 2 || fail "PHASE 2 FAIL: the reported tree is not a two-region split"
echo "PHASE 2 PASS: the split reported a two-region workspace tree to the server"

# ── Phase 3: the seeded session is filed under the new region ─────
# The pane the split opened asks for its session through the FIRST region's
# workspace, because the second one did not exist yet. `MoveSession` is what
# tells the server the session ended up somewhere else.
if ! oracle wait-frame client MoveSession "$NEW_WS" 25; then
    fail "PHASE 3 FAIL: no client MoveSession frame targeting $NEW_WS"
fi
MOVED=$(oracle first-frame-field client MoveSession session_id) \
    || fail "PHASE 3 FAIL: the recorded MoveSession carries no session_id"
if [ "$MOVED" = "$SESSION" ]; then
    fail "PHASE 3 FAIL: MoveSession named the first region's own session $SESSION"
fi
echo "PHASE 3 PASS: session $MOVED was moved into workspace $NEW_WS on the wire"

# ── Phase 4: an inbound WorkspaceInfo repaints the status bar ─────
# The status bar renders the focused pane's workspace name. Injecting the frame
# through the tap is what makes this an assertion about the INBOUND row rather
# than about the outbound one: nothing the client did causes it.
TARGET_WS=$(oracle first-frame-field client MoveSession target_workspace) || TARGET_WS="$NEW_WS"
# Echo the server's own accent back so the name is the only thing that moves
# between the captures; a different accent would also retint the focus ring.
WS_ACCENT=$(oracle first-frame-field server WorkspaceInfo accent_color) || WS_ACCENT="#a78bfa"
INFOS_BEFORE=$(count_log "workspace info received")
focus
status_bar_shot /output/03-before-workspace-name.png
inject "{\"type\":\"WorkspaceInfo\",\"workspace_id\":\"$TARGET_WS\",\"name\":\"$WS_NAME\",\"accent_color\":\"$WS_ACCENT\",\"split_direction\":null,\"project_root\":null}"
if ! wait_for_log_growth "workspace info received" "$INFOS_BEFORE" 15; then
    fail "PHASE 4 FAIL: the injected WorkspaceInfo never reached the reader"
fi
focus
status_bar_shot /output/04-workspace-named.png
NAME_DIFF=$(status_left_diff /output/03-before-workspace-name.png /output/04-workspace-named.png)
if [ "${NAME_DIFF:-0}" -lt "$NAME_DIFF_MIN" ]; then
    fail "PHASE 4 FAIL: the named workspace changed $NAME_DIFF px in the status bar (min $NAME_DIFF_MIN)"
fi
# The same frame with no name must take the segment back off screen, which is
# what separates "the payload drives the render" from "a repaint happened".
INFOS_BEFORE=$(count_log "workspace info received")
inject "{\"type\":\"WorkspaceInfo\",\"workspace_id\":\"$TARGET_WS\",\"name\":null,\"accent_color\":\"$WS_ACCENT\",\"split_direction\":null,\"project_root\":null}"
if ! wait_for_log_growth "workspace info received" "$INFOS_BEFORE" 15; then
    fail "PHASE 4 FAIL: the second injected WorkspaceInfo never reached the reader"
fi
focus
status_bar_shot /output/05-workspace-name-cleared.png
CLEAR_DIFF=$(status_left_diff /output/04-workspace-named.png /output/05-workspace-name-cleared.png)
BACK_DIFF=$(status_left_diff /output/03-before-workspace-name.png /output/05-workspace-name-cleared.png)
if [ "${CLEAR_DIFF:-0}" -lt "$NAME_DIFF_MIN" ]; then
    fail "PHASE 4 FAIL: clearing the name changed $CLEAR_DIFF px in the status bar"
fi
if [ "${BACK_DIFF:-0}" -ge "$NAME_DIFF_MIN" ]; then
    fail "PHASE 4 FAIL: clearing the name did not restore the original bar ($BACK_DIFF px off)"
fi
echo "PHASE 4 PASS: WorkspaceInfo drives the status bar (+$NAME_DIFF px named, -$CLEAR_DIFF px cleared, $BACK_DIFF px residual)"

# ── Phase 5: collapsing the region closes it on the server ────────
CLOSES_BEFORE=$(count_log "closed a workspace region on the server")
focus
send_keys ctrl+shift+w
if ! wait_for_log_growth "closed a workspace region on the server" "$CLOSES_BEFORE" 20; then
    fail "PHASE 5 FAIL: ctrl+shift+w never collapsed the second region"
fi
if ! oracle wait-frame client CloseWorkspace "$NEW_WS" 20; then
    fail "PHASE 5 FAIL: no client CloseWorkspace frame naming $NEW_WS"
fi
assert_reported_leaves 1 \
    || fail "PHASE 5 FAIL: the collapse was never reported as a one-region tree"
focus
shot /output/06-after-region-close.png
echo "PHASE 5 PASS: CloseWorkspace for $NEW_WS crossed the wire and the tree was re-reported"

echo ""
echo "PASS: visual workspace-ipc test"
echo "  Inspect screenshots in test-output/:"
echo "    00-attached.png                 — the adopted pane before any split"
echo "    01-before-workspace-split.png   — the window before the workspace split"
echo "    02-after-workspace-split.png    — two workspace regions"
echo "    03-before-workspace-name.png    — the status bar with no workspace name"
echo "    04-workspace-named.png          — the injected WorkspaceInfo's name on the bar"
echo "    05-workspace-name-cleared.png   — the same frame with no name"
echo "    06-after-region-close.png       — back to one region after the collapse"
