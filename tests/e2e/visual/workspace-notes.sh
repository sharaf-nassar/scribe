#!/bin/bash
# Scripted E2E: the workspace-notes surface of the GPUI client, asserted on the
# wire and on screen against a real client and a real server.
#
# Backs the `ClientMessage::WorkspaceNotesGet` / `WorkspaceNotesMutate` parity
# rows and the inbound `ServerMessage::WorkspaceNotesSnapshot` /
# `WorkspaceNotesChanged` rows. Before this test the surface was a demo: the
# modal opened against a freshly fabricated `WorkspaceId::new()` the server had
# never heard of, and both server answers fell into the reader's drop counter,
# so nothing the server said could ever reach the screen.
#
# A green `#[gpui::test]` over the notes model cannot tell that state apart from
# this one — the modal's state machine passes either way — so every phase below
# drives the real window and asserts on frames the wire tap actually recorded
# plus the pixels the client actually painted.
#
# The decisive de-demo assertion is Phase 1: the `WorkspaceNotesGet` the modal
# puts on the wire must name the very workspace the server filed the harness
# session under, not an id minted client-side.
#
# Phases:
#   * Ctrl+Shift+M opens the modal: `WorkspaceNotesGet` carries the server's own
#     workspace id, the server answers `WorkspaceNotesSnapshot`, and the empty
#     modal paints;
#   * typing a note and pressing Enter puts `WorkspaceNotesMutate`
#     (CreateActiveNote) on the wire for that same workspace, the server
#     broadcasts `WorkspaceNotesChanged` carrying the text, and the OPEN modal
#     repaints with the new row;
#   * closing and reopening the modal re-requests the notes and renders the
#     server's `WorkspaceNotesSnapshot` — the note survives the round trip
#     through the server, so what is on screen is server state, not local state;
#   * a `WorkspaceNotesChanged` injected through the tap (nothing the client
#     did causes it) updates the modal that is already open.
#
# Phase 0 is the same session-adoption dance `workspace-ipc.sh` documents: the
# entrypoint creates $SESSION after the client launched, so the running client
# never hears about it, and only a relaunch after the test daemon releases
# ownership picks it up through `ListSessions`. That relaunch is also what gives
# the window a server-minted workspace to open notes for.
#
# Input is driven through XTEST (plain `xdotool key`, no `--window`). GPUI reads
# the keyboard through XInput2 and ignores the synthetic events
# `xdotool --window` sends with XSendEvent.
#
# Requires: visual container (docker/entrypoint-visual.sh) with
# SCRIBE_SHARE_TAP=1, xdotool, scrot, imagemagick, python3.
set -e

RECORD="${SHARE_WIRE_RECORD:-/output/share-wire.jsonl}"
CONTROL="${SHARE_TAP_CONTROL:-$XDG_RUNTIME_DIR/scribe/share-tap.sock}"
CLIENT_LOG="${SCRIBE_CLIENT_LOG:-/output/client.log}"
SERVER_LOG="${SCRIBE_SERVER_LOG:-/output/server.log}"

# The note typed into the modal. Upper case and long enough that the row it
# becomes is unmistakable both on the wire and in a pixel diff.
NOTE_TEXT="NOTESYNCONE"

# The note pushed at the client through the tap, never typed anywhere.
INJECTED_TEXT="INJECTEDNOTETWO"

# Differing pixels the modal itself must add to the window. The panel is a
# multi-row overlay across the middle of the window, which is far more than this.
MODAL_DIFF_MIN="${MODAL_DIFF_MIN:-3000}"

# Differing pixels a single note row must add. One row is a marker, a summary,
# and three buttons replacing the "No active notes" placeholder.
ROW_DIFF_MIN="${ROW_DIFF_MIN:-200}"

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
# be cropped down to the window.
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

send_keys() {
    xdotool key --clearmodifiers "$@"
    sleep 0.5
}

type_text() {
    xdotool type --clearmodifiers --delay 40 "$1"
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

frame_types() {
    python3 - "$RECORD" "$1" <<'PY'
import json, sys
path, direction = sys.argv[1:3]
try:
    fh = open(path)
except OSError:
    sys.exit(0)
with fh:
    for line in fh:
        try:
            row = json.loads(line)
        except ValueError:
            continue
        if row.get("dir") == direction:
            print(row.get("message", {}).get("type"))
PY
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

# Start a fresh window on the wire record so the frames a phase asserts on can
# only have come from that phase.
reset_record() {
    : >"$RECORD"
    sleep 0.2
}

# Wait until a recorded frame in `$1` of type `$2` mentions `$3` somewhere in
# its payload, printing the matching frame.
wait_for_frame() {
    local direction="$1" wanted="$2" needle="$3" timeout_secs="${4:-20}" started
    started=$(date +%s)
    while true; do
        if python3 - "$RECORD" "$direction" "$wanted" "$needle" <<'PY'
import json, sys
path, direction, wanted, needle = sys.argv[1:5]
try:
    fh = open(path)
except OSError:
    sys.exit(1)
with fh:
    for line in fh:
        try:
            row = json.loads(line)
        except ValueError:
            continue
        if row.get("dir") != direction:
            continue
        msg = row.get("message", {})
        if msg.get("type") != wanted:
            continue
        if not needle or needle in json.dumps(msg):
            print(json.dumps(msg)[:400])
            sys.exit(0)
sys.exit(1)
PY
        then
            return 0
        fi
        if [ $(( "$(date +%s)" - started )) -ge "$timeout_secs" ]; then
            return 1
        fi
        sleep 0.3
    done
}

# The workspace the SERVER filed $SESSION under, read off its own `SessionList`
# frame. This is the ground truth Phase 1 compares the modal's request against:
# an id the client minted for itself can never equal it.
server_workspace_for_session() {
    python3 - "$RECORD" "$SESSION" <<'PY'
import json, sys
path, session = sys.argv[1:3]
with open(path) as fh:
    for line in fh:
        try:
            row = json.loads(line)
        except ValueError:
            continue
        if row.get("dir") != "server":
            continue
        msg = row.get("message", {})
        if msg.get("type") != "SessionList":
            continue
        for info in msg.get("sessions") or []:
            if str(info.get("session_id")) == session:
                print(info.get("workspace_id"))
                sys.exit(0)
sys.exit(1)
PY
}

# The single workspace id a recorded client `WorkspaceNotesGet` asked about.
requested_notes_workspace() {
    python3 - "$RECORD" <<'PY'
import json, sys
path = sys.argv[1]
with open(path) as fh:
    for line in fh:
        try:
            row = json.loads(line)
        except ValueError:
            continue
        if row.get("dir") != "client":
            continue
        msg = row.get("message", {})
        if msg.get("type") != "WorkspaceNotesGet":
            continue
        ids = msg.get("workspace_ids") or []
        if len(ids) != 1:
            print(f"expected exactly one workspace id, got {ids}", file=sys.stderr)
            sys.exit(1)
        print(ids[0])
        sys.exit(0)
sys.exit(1)
PY
}

# Assert a recorded client `WorkspaceNotesMutate` is a `CreateActiveNote` for
# `$1` carrying `$2`.
assert_create_note() {
    python3 - "$RECORD" "$1" "$2" <<'PY'
import json, sys
path, workspace, text = sys.argv[1:4]
found = []
with open(path) as fh:
    for line in fh:
        try:
            row = json.loads(line)
        except ValueError:
            continue
        if row.get("dir") != "client":
            continue
        msg = row.get("message", {})
        if msg.get("type") != "WorkspaceNotesMutate":
            continue
        mutation = msg.get("mutation") or {}
        create = mutation.get("CreateActiveNote")
        if not create:
            found.append(sorted(mutation))
            continue
        if str(create.get("workspace_id")) != workspace:
            print(f"CreateActiveNote named workspace {create.get('workspace_id')}, "
                  f"expected {workspace}", file=sys.stderr)
            sys.exit(1)
        if text not in create.get("text", ""):
            print(f"CreateActiveNote carried {create.get('text')!r}, "
                  f"expected it to contain {text!r}", file=sys.stderr)
            sys.exit(1)
        print(f"CreateActiveNote for {workspace}: {create.get('text')!r}")
        sys.exit(0)
print(f"no CreateActiveNote mutation recorded (saw {found})", file=sys.stderr)
sys.exit(1)
PY
}

# Print the `WorkspaceNotesChanged` collection the server broadcast, as JSON, so
# the injection phase can replay it with one extra note appended.
recorded_changed_collection() {
    python3 - "$RECORD" <<'PY'
import json, sys
path = sys.argv[1]
latest = None
with open(path) as fh:
    for line in fh:
        try:
            row = json.loads(line)
        except ValueError:
            continue
        if row.get("dir") != "server":
            continue
        msg = row.get("message", {})
        if msg.get("type") == "WorkspaceNotesChanged":
            latest = msg.get("collection")
if latest is None:
    sys.exit(1)
print(json.dumps(latest))
PY
}

# Build a `WorkspaceNotesChanged` frame from the recorded collection with one
# extra active note appended, so the injected frame is the server's own state
# plus exactly one visible row.
build_injected_changed() {
    python3 - "$1" "$2" <<'PY'
import json, sys
collection, text = json.loads(sys.argv[1]), sys.argv[2]
notes = list(collection.get("active_notes") or [])
notes.append({
    "note_id": "injected-note",
    "workspace_id": collection["workspace_id"],
    "text": text,
    "status": "active",
    "created_at_ms": 0,
    "updated_at_ms": 0,
    "archived_at_ms": None,
    "archive_reason": None,
})
collection["active_notes"] = notes
print(json.dumps({"type": "WorkspaceNotesChanged", "collection": collection}))
PY
}

# ── Phase 0: hand the relaunched client a live pane to act in ─────
sleep 1.0
kill "$SCRIBE_CLIENT_PID" 2>/dev/null || true
for _ in $(seq 1 40); do
    pgrep -f 'scribe-client-gpui' >/dev/null 2>&1 || break
    sleep 0.25
done
if pgrep -f 'scribe-client-gpui' >/dev/null 2>&1; then
    fail "PHASE 0 FAIL: the original client did not exit"
fi
scribe-test daemon stop >/dev/null 2>&1 || true
sleep 1.0
reset_record
scribe-client-gpui >>"$CLIENT_LOG" 2>&1 &
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
SERVER_WS=$(server_workspace_for_session) \
    || fail "PHASE 0 FAIL: no server SessionList entry for session $SESSION"
echo "PHASE 0 PASS: client attached to session $SESSION in workspace $SERVER_WS (ink $BASE_INK)"

# ── Phase 1: the modal opens on the SERVER's workspace ────────────
OPENS_BEFORE=$(count_log "opened the workspace notes modal")
focus
shot /output/01-before-notes-modal.png
send_keys ctrl+shift+m
if ! wait_for_log_growth "opened the workspace notes modal" "$OPENS_BEFORE" 15; then
    fail "PHASE 1 FAIL: ctrl+shift+m never opened the workspace notes modal"
fi
if ! wait_for_frame client WorkspaceNotesGet "" 20; then
    fail "PHASE 1 FAIL: opening the modal put no WorkspaceNotesGet frame on the wire"
fi
ASKED_WS=$(requested_notes_workspace) \
    || fail "PHASE 1 FAIL: the recorded WorkspaceNotesGet names no single workspace"
if [ "$ASKED_WS" != "$SERVER_WS" ]; then
    fail "PHASE 1 FAIL: the modal asked about workspace $ASKED_WS, but the server filed session $SESSION under $SERVER_WS — the id is still fabricated client-side"
fi
if ! wait_for_frame server WorkspaceNotesSnapshot "" 20; then
    fail "PHASE 1 FAIL: the server never answered WorkspaceNotesGet with a snapshot"
fi
focus
shot /output/02-notes-modal-open.png
MODAL_DIFF=$(window_diff /output/01-before-notes-modal.png /output/02-notes-modal-open.png)
if [ "${MODAL_DIFF:-0}" -lt "$MODAL_DIFF_MIN" ]; then
    fail "PHASE 1 FAIL: the notes modal changed $MODAL_DIFF px (min $MODAL_DIFF_MIN)"
fi
EMPTY_INK=$(window_ink /output/02-notes-modal-open.png)
echo "PHASE 1 PASS: WorkspaceNotesGet named the server's own workspace $SERVER_WS and the modal painted (+$MODAL_DIFF px, ink $EMPTY_INK)"

# ── Phase 2: a typed note round-trips through the server ──────────
CHANGES_BEFORE=$(count_log "workspace notes changed")
reset_record
type_text "$NOTE_TEXT"
send_keys Return
if ! wait_for_frame client WorkspaceNotesMutate "$NOTE_TEXT" 20; then
    fail "PHASE 2 FAIL: saving the note put no WorkspaceNotesMutate frame on the wire"
fi
assert_create_note "$SERVER_WS" "$NOTE_TEXT" \
    || fail "PHASE 2 FAIL: the recorded mutation is not a CreateActiveNote for $SERVER_WS"
if ! wait_for_frame server WorkspaceNotesChanged "$NOTE_TEXT" 20; then
    fail "PHASE 2 FAIL: the server never broadcast WorkspaceNotesChanged for the new note"
fi
if ! wait_for_log_growth "workspace notes changed" "$CHANGES_BEFORE" 15; then
    fail "PHASE 2 FAIL: the WorkspaceNotesChanged broadcast never reached the reader"
fi
focus
shot /output/03-note-saved.png
SAVED_DIFF=$(window_diff /output/02-notes-modal-open.png /output/03-note-saved.png)
if [ "${SAVED_DIFF:-0}" -lt "$ROW_DIFF_MIN" ]; then
    fail "PHASE 2 FAIL: the broadcast note changed $SAVED_DIFF px (min $ROW_DIFF_MIN)"
fi
# Keep the server's own collection before the next phase truncates the record;
# Phase 4 replays it with one extra note so the injected frame is real server
# state plus exactly one visible row.
COLLECTION=$(recorded_changed_collection) \
    || fail "PHASE 2 FAIL: the broadcast carried no collection to replay"
echo "PHASE 2 PASS: the typed note crossed the wire and its broadcast repainted the open modal (+$SAVED_DIFF px)"

# ── Phase 3: reopening renders the server's snapshot ──────────────
# Closing drops every local list, so what comes back on reopen can only be the
# server's answer to the fresh `WorkspaceNotesGet`.
send_keys Escape
sleep 0.5
focus
shot /output/04-notes-modal-closed.png
CLOSE_DIFF=$(window_diff /output/03-note-saved.png /output/04-notes-modal-closed.png)
if [ "${CLOSE_DIFF:-0}" -lt "$MODAL_DIFF_MIN" ]; then
    fail "PHASE 3 FAIL: Escape did not tear the modal down ($CLOSE_DIFF px)"
fi
reset_record
OPENS_BEFORE=$(count_log "opened the workspace notes modal")
send_keys ctrl+shift+m
if ! wait_for_log_growth "opened the workspace notes modal" "$OPENS_BEFORE" 15; then
    fail "PHASE 3 FAIL: the modal did not reopen"
fi
if ! wait_for_frame server WorkspaceNotesSnapshot "$NOTE_TEXT" 20; then
    fail "PHASE 3 FAIL: the reopened modal's snapshot does not carry the saved note"
fi
focus
shot /output/05-notes-modal-reopened.png
SNAPSHOT_INK=$(window_ink /output/05-notes-modal-reopened.png)
SNAPSHOT_DIFF=$(window_diff /output/02-notes-modal-open.png /output/05-notes-modal-reopened.png)
if [ "${SNAPSHOT_DIFF:-0}" -lt "$ROW_DIFF_MIN" ]; then
    fail "PHASE 3 FAIL: the snapshot's note row changed $SNAPSHOT_DIFF px against the empty modal (min $ROW_DIFF_MIN)"
fi
if [ "$SNAPSHOT_INK" -le "$EMPTY_INK" ]; then
    fail "PHASE 3 FAIL: the reopened modal has no more ink than the empty one ($SNAPSHOT_INK vs $EMPTY_INK)"
fi
echo "PHASE 3 PASS: WorkspaceNotesSnapshot rendered the persisted note (+$SNAPSHOT_DIFF px, ink $EMPTY_INK -> $SNAPSHOT_INK)"

# ── Phase 4: a pushed WorkspaceNotesChanged updates the open modal ─
# Injected through the tap, so nothing the client did causes it: this is an
# assertion about the INBOUND row alone.
FRAME=$(build_injected_changed "$COLLECTION" "$INJECTED_TEXT") \
    || fail "PHASE 4 FAIL: could not build the injected WorkspaceNotesChanged frame"
CHANGES_BEFORE=$(count_log "workspace notes changed")
focus
shot /output/06-before-injected-note.png
inject "$FRAME"
if ! wait_for_log_growth "workspace notes changed" "$CHANGES_BEFORE" 15; then
    fail "PHASE 4 FAIL: the injected WorkspaceNotesChanged never reached the reader"
fi
focus
shot /output/07-injected-note.png
INJECT_DIFF=$(window_diff /output/06-before-injected-note.png /output/07-injected-note.png)
if [ "${INJECT_DIFF:-0}" -lt "$ROW_DIFF_MIN" ]; then
    fail "PHASE 4 FAIL: the injected note changed $INJECT_DIFF px in the open modal (min $ROW_DIFF_MIN)"
fi
echo "PHASE 4 PASS: an injected WorkspaceNotesChanged repainted the already-open modal (+$INJECT_DIFF px)"

echo ""
echo "PASS: visual workspace-notes test"
echo "  Inspect screenshots in test-output/:"
echo "    00-attached.png               — the adopted pane before any modal"
echo "    01-before-notes-modal.png     — the window with no notes modal"
echo "    02-notes-modal-open.png       — the modal on the server's workspace, empty"
echo "    03-note-saved.png             — the broadcast note rendered in the open modal"
echo "    04-notes-modal-closed.png     — the modal torn down"
echo "    05-notes-modal-reopened.png   — the note re-rendered from the server snapshot"
echo "    06-before-injected-note.png   — the modal before the injected change"
echo "    07-injected-note.png          — the injected WorkspaceNotesChanged on screen"
