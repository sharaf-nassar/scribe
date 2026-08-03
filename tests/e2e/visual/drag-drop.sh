#!/bin/bash
[ "${SCRIBE_E2E_SANDBOX:-0}" = "1" ] || { echo "FATAL: this script only runs inside the scribe e2e container (use just e2e-func / e2e-visual)." >&2; exit 99; }
# Scripted E2E: dropping a file on the GPUI client types its shell-quoted path
# into the focused pane.
#
# `drag_drop.rs` shipped the ported quoting complete and unit-tested, and the
# binary contained no `on_drop` and no `ExternalPaths` handler at all — so
# `dropped_path_insertion` had no caller and a dropped file did nothing.
#
# The drop is real. `xdnd-drop.py` is an actual XDND drag source on the same X
# server: it owns `XdndSelection`, walks the client's X11 backend through
# `XdndEnter` / `XdndPosition` / `XdndDrop`, and answers the client's own
# `text/uri-list` selection conversion. Nothing about the client's drop path is
# stubbed, and the oracle is the PTY: `scribe-test wait-output` reads what the
# real shell in the real server-owned pane received.
#
# The path deliberately contains a space and a single quote, because the whole
# point of the ported quoting is that such a path survives as ONE argument.
#
# Requires: visual container with SCRIBE_SHARED_PANE=1 (so `scribe-test` reads
# the very pane the client types into), python3-xlib, xdotool.
set -e

CLIENT_LOG="${SCRIBE_CLIENT_LOG:-/output/client.log}"
DROP_DIR="/tmp/scribe drop dir"
DROP_FILE="$DROP_DIR/it's a file.txt"

WID=""

fail() {
    echo "$1"
    echo "--- client log tail ---"
    tail -40 "$CLIENT_LOG" || true
    echo "--- pane ---"
    scribe-test snapshot "$SESSION" /output/drag-drop-fail.json >/dev/null 2>&1 || true
    exit 1
}

find_window() {
    local wid
    wid=$(xdotool search --class '[Ss]cribe' 2>/dev/null | tail -1)
    [ -z "$wid" ] && wid=$(xdotool search --name '[Ss]cribe' 2>/dev/null | tail -1)
    printf '%s' "$wid"
}

focus() {
    WID=$(find_window)
    [ -z "$WID" ] && fail "FAIL: no Scribe window found"
    xdotool windowactivate --sync "$WID" 2>/dev/null \
        || xdotool windowfocus --sync "$WID" 2>/dev/null || true
    sleep 0.5
}

count_log() {
    grep -acF "$1" "$CLIENT_LOG" 2>/dev/null || true
}

INSERTED="inserting a dropped file path into the focused pane"

# ── Phase 0: the client is attached and painting the shared pane ──
mkdir -p "$DROP_DIR"
: >"$DROP_FILE"
sleep 1.5
focus
if ! grep -q "attaching to session" "$CLIENT_LOG"; then
    fail "PHASE 0 FAIL: the client never attached to the shared pane"
fi
scrot -o /output/00-drop-attached.png
echo "PHASE 0 PASS: client window $WID is attached to session $SESSION"

# ── Phase 1: a dropped path is typed into the pane, shell-quoted ──
# The pane is parked at a plain `cat` so whatever the client types is echoed
# back verbatim, which makes the PTY itself the oracle for the exact bytes.
scribe-test send "$SESSION" "cat\n"
sleep 0.7

# Move the pointer inside the window first: the client's X11 backend takes the
# drop position from `query_pointer`, and a drop landing outside the window's
# own bounds would never reach the root element's drop handler.
eval "$(xdotool getwindowgeometry --shell "$WID")"
xdotool mousemove $((X + WIDTH / 2)) $((Y + HEIGHT / 2))
sleep 0.3

BEFORE=$(count_log "$INSERTED")
python3 /tests/visual/xdnd-drop.py --window "$WID" --path "$DROP_FILE" \
    | tee /output/xdnd-drop.log
sleep 1.5

if [ "$(count_log "$INSERTED")" -le "${BEFORE:-0}" ]; then
    fail "PHASE 1 FAIL: the running client never handled the dropped path"
fi

# The POSIX quoting the module produces for a path holding a single quote:
#   /tmp/scribe drop dir/it's a file.txt
# becomes  '/tmp/scribe drop dir/it'"'"'s a file.txt'  plus a trailing space.
# `cat` echoes the client's bytes straight back, so the session's PTY output
# is the byte-level oracle here — no pixels, no re-derivation.
QUOTED="'$DROP_DIR/it'\"'\"'s a file.txt'"
if ! scribe-test wait-output "$SESSION" "$QUOTED" --timeout 8000; then
    fail "PHASE 1 FAIL: the POSIX-quoted path never reached the pane's PTY"
fi
scribe-test snapshot "$SESSION" /output/drop-pane.json
scrot -o /output/01-drop-inserted.png
echo "PHASE 1 PASS: the pane received exactly $QUOTED"

# ── Phase 2: the insertion ends in a separator space ──────────────
# The trailing space is what makes the next thing the user types a separate
# argument rather than a suffix on the filename. Typing a marker straight
# after the drop is what proves it is there: without the space the marker
# would appear glued to the closing quote.
scribe-test send "$SESSION" "SEPARATOR-MARKER\n"
if ! scribe-test wait-output "$SESSION" "$QUOTED SEPARATOR-MARKER" --timeout 8000; then
    fail "PHASE 2 FAIL: the quoted path was not followed by a separator space"
fi
echo "PHASE 2 PASS: the insertion ends with the argument separator"

scribe-test send "$SESSION" "\x04"
sleep 0.3

echo ""
echo "PASS: visual drag-drop test"
echo "  Inspect screenshots in test-output/:"
echo "    00-drop-attached.png  — client attached to the shared pane"
echo "    01-drop-inserted.png  — the dropped path on the pane"
