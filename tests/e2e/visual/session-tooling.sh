#!/bin/bash
# Scripted E2E: the `Subscribe` and `RequestSnapshot` session tooling of the
# GPUI client, asserted on the wire against a real client and a real server.
#
# Backs the `ClientMessage::Subscribe` and `ClientMessage::RequestSnapshot`
# parity rows. Both frames used to exist only in the frozen protocol enum — no
# construction site anywhere in `scribe-client` — so the client could
# neither register an attached pane for the server's CWD-fallback check nor ask
# for an authoritative screen when its own pane may have drifted.
#
# The wire tap (`scribe-test share-tap`, SCRIBE_SHARE_TAP=1) is interposed on
# the server socket, so every frame the client sends and every frame the server
# answers with is recorded as JSONL. That is what makes "the message is on the
# wire at the right lifecycle point" assertable instead of inferred: the record
# is truncated at each phase boundary, so a frame found afterwards can only have
# been produced by the action that phase performed.
#
# Phase 0 mirrors overlay-actions.sh: the entrypoint creates $SESSION through
# `scribe-test` *after* launching the client, and the server sends
# `SessionCreated` only to the connection that asked, so the running client
# never learns the session exists. Killing the client and stopping the test
# daemon releases the session's ownership; a relaunched client then picks it up
# through the normal `ListSessions` -> `AttachSessions` path, which is exactly
# the lifecycle point `Subscribe` has to ride along with.
#
# Requires: visual container with SCRIBE_SHARE_TAP=1, xdotool, scrot,
# imagemagick, python3.
set -e

RECORD="${SHARE_WIRE_RECORD:-/output/share-wire.jsonl}"
CLIENT_LOG="${SCRIBE_CLIENT_LOG:-/output/client.log}"
SERVER_LOG="${SCRIBE_SERVER_LOG:-/output/server.log}"
CONFIG_DIR="$XDG_CONFIG_HOME/scribe"
CONFIG_FILE="$CONFIG_DIR/config.toml"
mkdir -p "$CONFIG_DIR"

# The text typed into the attached pane. It has to survive a round trip through
# the server's `Term` and come back inside the per-cell snapshot, so it is
# asserted both in the recorded `ScreenSnapshot` payload and as ink on screen.
MARKER="SNAPSHOT_PROBE_OK"

WIN_X=0
WIN_Y=0
GRID_X=0
GRID_Y=0
GRID_W=0
GRID_H=0

find_window() {
    local wid
    wid=$(xdotool search --class '[Ss]cribe' 2>/dev/null | tail -1)
    [ -z "$wid" ] && wid=$(xdotool search --name '[Ss]cribe' 2>/dev/null | tail -1)
    printf '%s' "$wid"
}

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
    GRID_X="$X"
    GRID_Y="$Y"
    GRID_W="$WIDTH"
    GRID_H="$HEIGHT"
}

shot() {
    sleep 0.3
    scrot -o "$1"
    echo "captured $1"
}

# Lit pixels inside the client window of a full-screen capture. Rendered text is
# near-white on a near-black background, so a luminance threshold separates the
# pane's ink from its background.
grid_ink() {
    local value
    value=$(convert "$1" -crop "${GRID_W}x${GRID_H}+${GRID_X}+${GRID_Y}" +repage \
        -colorspace Gray -threshold 35% -format "%[fx:mean*w*h]" info:)
    printf '%s' "${value%.*}"
}

send_keys() {
    xdotool key --clearmodifiers "$@"
    sleep 0.4
}

type_text() {
    xdotool type --clearmodifiers --delay 40 "$1"
    sleep 0.6
}

count_log() {
    grep -acF "$1" "$CLIENT_LOG" 2>/dev/null || true
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
    echo "--- client log tail ---"
    tail -40 "$CLIENT_LOG" || true
    exit 1
}

# Start a fresh window on the wire record so the frames a phase asserts on can
# only have come from that phase.
reset_record() {
    : >"$RECORD"
    sleep 0.2
}

# Wait until a recorded frame in `direction` of `type` mentions `needle`
# somewhere in its payload, printing the matching frame. Ordering is checked
# separately by `assert_order`.
wait_for_frame() {
    local direction="$1" wanted="$2" needle="$3" timeout_secs="${4:-15}" started
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

# Assert the first recorded client frame of type $1 comes before the first of
# type $2. This is the "at the right lifecycle point" half of the bead: a
# `Subscribe` the server would reject because it preceded its `AttachSessions`
# is a bug, not parity.
assert_order() {
    python3 - "$RECORD" "$1" "$2" <<'PY'
import json, sys
path, first, second = sys.argv[1:4]
seen = {}
with open(path) as fh:
    for index, line in enumerate(fh):
        try:
            row = json.loads(line)
        except ValueError:
            continue
        if row.get("dir") != "client":
            continue
        kind = row.get("message", {}).get("type")
        if kind in (first, second) and kind not in seen:
            seen[kind] = index
if first not in seen:
    print(f"no client {first} frame recorded", file=sys.stderr)
    sys.exit(1)
if second not in seen:
    print(f"no client {second} frame recorded", file=sys.stderr)
    sys.exit(1)
if seen[first] >= seen[second]:
    print(f"{first} (line {seen[first]}) did not precede {second} (line {seen[second]})",
          file=sys.stderr)
    sys.exit(1)
print(f"{first} at line {seen[first]} precedes {second} at line {seen[second]}")
PY
}

# Print the text of every recorded ScreenSnapshot for $2, visible grid first
# then scrollback, so a marker typed into the pane can be located in the reply
# the server actually put on the wire.
snapshot_text() {
    python3 - "$RECORD" "$1" <<'PY'
import json, sys
path, session = sys.argv[1:3]
def chars(cells):
    return "".join(cell.get("c", " ") for cell in cells)
with open(path) as fh:
    for line in fh:
        try:
            row = json.loads(line)
        except ValueError:
            continue
        msg = row.get("message", {})
        if row.get("dir") != "server" or msg.get("type") != "ScreenSnapshot":
            continue
        if session and msg.get("session_id") != session:
            continue
        snap = msg.get("snapshot", {})
        print(chars(snap.get("cells", [])))
        print(chars(snap.get("scrollback", [])))
PY
}

# The `cols`x`rows` of the last recorded ScreenSnapshot for $1, so the client's
# own "repainted" log line can be tied to the frame on the wire rather than to
# some other snapshot.
snapshot_dims() {
    python3 - "$RECORD" "$1" <<'PY'
import json, sys
path, session = sys.argv[1:3]
dims = None
with open(path) as fh:
    for line in fh:
        try:
            row = json.loads(line)
        except ValueError:
            continue
        msg = row.get("message", {})
        if row.get("dir") != "server" or msg.get("type") != "ScreenSnapshot":
            continue
        if session and msg.get("session_id") != session:
            continue
        snap = msg.get("snapshot", {})
        dims = (snap.get("cols"), snap.get("rows"))
if dims is None:
    sys.exit(1)
print(f"cols={dims[0]} rows={dims[1]}")
PY
}

# ── Phase 0: hand the relaunched client the harness session ───────
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
    BASE_INK=$(grid_ink /output/00-attached.png)
    [ "$BASE_INK" -ge 20 ] && break
    sleep 0.5
done
if [ "$BASE_INK" -lt 20 ]; then
    fail "PHASE 0 FAIL: the client rendered no pane content (ink $BASE_INK)"
fi
echo "PHASE 0 PASS: client attached to session $SESSION (grid ink $BASE_INK)"

# ── Phase 1: the attach carries a Subscribe for the same pane ─────
# The relaunched client's whole startup is inside this record window, so the
# frames below are the ones the `ListSessions` -> `attach_session` path emitted.
if ! wait_for_frame client Subscribe "$SESSION" 15; then
    echo "--- recorded client frame types ---"
    python3 -c 'import json,sys
for line in open(sys.argv[1]):
    try: row=json.loads(line)
    except ValueError: continue
    if row.get("dir")=="client": print(row["message"].get("type"))' "$RECORD" || true
    fail "PHASE 1 FAIL: no client Subscribe frame naming session $SESSION"
fi
assert_order AttachSessions Subscribe || \
    fail "PHASE 1 FAIL: Subscribe was not sent behind its AttachSessions"
if grep -q "Subscribe denied for unattached session" "$SERVER_LOG" 2>/dev/null; then
    fail "PHASE 1 FAIL: the server rejected the Subscribe as unattached"
fi
echo "PHASE 1 PASS: attach put Subscribe on the wire behind AttachSessions, and the server took it"

# ── Phase 2: put a known marker into the attached pane ────────────
focus
type_text "$MARKER"
shot /output/01-marker-typed.png
MARKER_INK=$(grid_ink /output/01-marker-typed.png)
if [ "$MARKER_INK" -le "$BASE_INK" ]; then
    fail "PHASE 2 FAIL: typing the marker changed no pixels ($BASE_INK -> $MARKER_INK)"
fi
echo "PHASE 2 PASS: '$MARKER' is on screen and in the server's Term (ink $MARKER_INK)"

# ── Phase 3: a cell-metric change asks for a fresh screen ─────────
# Editing the font size hot-reloads the running client, which republishes its
# cell metrics as a `Resize` and — because a display-only client cannot
# re-derive the post-SIGWINCH grid itself — follows it with `RequestSnapshot`.
reset_record
RELOADS_BEFORE=$(count_log "config hot-reloaded")
SNAPSHOTS_BEFORE=$(count_log "repainted pane from server screen snapshot")
cat > "$CONFIG_FILE" <<'EOF'
[appearance]
font_size = 21.0
EOF
if ! wait_for_log_growth "config hot-reloaded" "${RELOADS_BEFORE:-0}" 15; then
    fail "PHASE 3 FAIL: the client never hot-reloaded the font edit"
fi
echo "PHASE 3 PASS: the font-size edit reloaded the running client"

# ── Phase 4: RequestSnapshot is on the wire and answered ──────────
if ! wait_for_frame client RequestSnapshot "$SESSION" 15; then
    fail "PHASE 4 FAIL: no client RequestSnapshot frame naming session $SESSION"
fi
assert_order Resize RequestSnapshot || \
    fail "PHASE 4 FAIL: RequestSnapshot did not follow the Resize it resyncs"
if ! wait_for_frame server ScreenSnapshot "$SESSION" 15; then
    fail "PHASE 4 FAIL: the server sent no ScreenSnapshot for session $SESSION"
fi
if grep -q "RequestSnapshot denied for unattached session" "$SERVER_LOG" 2>/dev/null; then
    fail "PHASE 4 FAIL: the server rejected the RequestSnapshot as unattached"
fi
if ! snapshot_text "$SESSION" | grep -qF "$MARKER"; then
    echo "--- snapshot text ---"
    snapshot_text "$SESSION" | head -5
    fail "PHASE 4 FAIL: the ScreenSnapshot reply does not carry the pane's content"
fi
echo "PHASE 4 PASS: RequestSnapshot rode the Resize and came back carrying '$MARKER'"

# ── Phase 5: the snapshot repainted the running window ────────────
# The reader applies a snapshot as RIS + the snapshot's own ANSI, so everything
# the pane shows after this point was painted by the snapshot: a blank window
# would mean the reset landed without its content, and unchanged pixels would
# mean the reply never reached the pane at all.
if ! wait_for_log_growth "repainted pane from server screen snapshot" \
        "${SNAPSHOTS_BEFORE:-0}" 15; then
    fail "PHASE 5 FAIL: the client never applied the ScreenSnapshot to the pane"
fi
# Tie the applied snapshot to the recorded frame: the log line carries the
# dimensions of the grid the reader actually fed into the pane.
DIMS=$(snapshot_dims "$SESSION") || fail "PHASE 5 FAIL: no ScreenSnapshot dimensions on the wire"
if ! sed 's/\x1b\[[0-9;]*m//g' "$CLIENT_LOG" \
        | grep -F "repainted pane from server screen snapshot" \
        | grep -qF "$DIMS"; then
    fail "PHASE 5 FAIL: the applied snapshot ($DIMS) is not the one recorded on the wire"
fi
sleep 0.8
focus
shot /output/02-snapshot-repaint.png
AFTER_INK=$(grid_ink /output/02-snapshot-repaint.png)
if [ "$AFTER_INK" -lt 20 ]; then
    fail "PHASE 5 FAIL: the pane is blank after the snapshot reset (ink $AFTER_INK)"
fi
if cmp -s /output/01-marker-typed.png /output/02-snapshot-repaint.png; then
    fail "PHASE 5 FAIL: the window is pixel-identical across the snapshot repaint"
fi
echo "PHASE 5 PASS: the snapshot repainted the window (ink $MARKER_INK -> $AFTER_INK)"

echo ""
echo "PASS: visual session-tooling test"
echo "  Inspect screenshots in test-output/:"
echo "    00-attached.png           — relaunched client attached to the harness session"
echo "    01-marker-typed.png       — the marker typed into the attached pane"
echo "    02-snapshot-repaint.png   — pane repainted from the server's ScreenSnapshot"
echo "  Wire record: test-output/share-wire.jsonl"
