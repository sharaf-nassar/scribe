#!/bin/bash
[ "${SCRIBE_E2E_SANDBOX:-0}" = "1" ] || { echo "FATAL: this script only runs inside the scribe e2e container (use just e2e-func / e2e-visual)." >&2; exit 99; }
# Scripted + visual E2E: the GPUI client's find overlay, end to end on the wire.
#
# Find is the one client surface whose whole value is a server round trip. This
# client is display-only: it holds the visible viewport and nothing else, so the
# scrollback the user is searching lives on the server and the only way to find
# anything in it is `ClientMessage::SearchRequest` -> `ServerMessage::
# SearchResults`. Neither end of that exchange can be shown by a headless test,
# so this drives the real window against the real `scribe-server` and reads the
# frames off the wire.
#
# The wire tap (`scribe-test share-tap`, SCRIBE_SHARE_TAP=1) is interposed on
# the server socket purely as a recorder — nothing is injected. Every server
# frame asserted below is one the real server chose to send in answer to
# something the real client sent.
#
# The shared-pane rig (SCRIBE_SHARED_PANE=1) hands the client a live pane the
# harness can also write to, so the text being searched is put on screen through
# the real PTY rather than faked into the client.
#
# Phases:
#   0. seed the pane with a known needle and capture the quiet grid;
#   1. the find chord opens the overlay (it used to be a counted, dropped
#      KeyAction::OpenFind);
#   2. typing a query puts SearchRequest on the wire, the server answers
#      SearchResults, the client consumes it (no unhandled-variant warning) and
#      the grid paints highlights where the needle is;
#   3. Enter moves the current match, which repaints a different cell run;
#   4. Escape closes the overlay, every highlight is dropped, and the chord
#      re-opens it — which it could not do if the overlay had swallowed it.
#
# Input is driven through XTEST (plain `xdotool key` / `type`, no `--window`):
# GPUI reads keyboard through XInput2 and ignores the synthetic events that
# `xdotool --window` sends with XSendEvent.
#
# Requires: visual container with SCRIBE_SHARED_PANE=1 and SCRIBE_SHARE_TAP=1
# (`just e2e-visual-find`); xdotool, scrot, python3, ImageMagick.
set -e

RECORD="${SHARE_WIRE_RECORD:-/output/share-wire.jsonl}"
CLIENT_LOG="${SCRIBE_CLIENT_LOG:-/output/client.log}"
SESSION="${SESSION:?the shared-pane rig must export a created SESSION}"

# The string typed into the find field. It appears several times on screen (the
# echoed command line plus its output), so the current match and the passive
# matches are both visible in one frame.
NEEDLE="needle"

# Minimum changed pixels for a crop comparison to count as "the grid repainted".
# Well below one highlighted glyph run and far above compression noise.
DIFF_MIN="${DIFF_MIN:-40}"

# Maximum changed pixels for two crops to count as identical. Non-zero only so a
# single stray antialiased pixel cannot fail the "highlights were dropped" check.
# The terminal cursor is hidden before the baseline, so its independent blink
# never consumes this budget.
DIFF_MAX="${DIFF_MAX:-40}"

fail() {
    echo "FAIL: $1" >&2
    echo "--- client log tail ---" >&2
    tail -40 "$CLIENT_LOG" >&2 || true
    exit 1
}

# Count recorded frames of `type` in `dir` matching every key=value pair. A
# value that parses as JSON is compared as JSON, so a bare word matches the
# string and `limit=256` matches the number.
count_frames() {
    python3 - "$RECORD" "$@" <<'PY'
import json, sys

path, direction, wanted = sys.argv[1], sys.argv[2], sys.argv[3]


def norm(value):
    try:
        return json.loads(value)
    except ValueError:
        return value


pairs = [(k, norm(v)) for k, v in (p.split("=", 1) for p in sys.argv[4:])]
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
        message = row.get("message", {})
        if message.get("type") != wanted:
            continue
        if all(message.get(key) == value for key, value in pairs):
            total += 1
print(total)
PY
}

count_client() { count_frames client "$@"; }
count_server() { count_frames server "$@"; }

# Wait until the recorded frame count for a matcher exceeds `baseline`.
wait_for_frames() {
    local direction="$1" baseline="$2" timeout_secs="$3"
    shift 3
    local started now
    started=$(date +%s)
    while true; do
        now=$(count_frames "$direction" "$@")
        if [ "$now" -gt "$baseline" ]; then
            return 0
        fi
        if [ $(( "$(date +%s)" - started )) -ge "$timeout_secs" ]; then
            return 1
        fi
        sleep 0.3
    done
}

# Largest `matches=` count the client has logged for a consumed SearchResults.
# The tracing subscriber writes ANSI-styled field names, so the escapes are
# stripped before the number is read out.
largest_reported_match_count() {
    grep "search results received" "$CLIENT_LOG" 2>/dev/null \
        | sed 's/\x1b\[[0-9;]*m//g' \
        | grep -oE 'matches=[0-9]+' | grep -oE '[0-9]+' | sort -n | tail -1
}

count_log() { grep -c "$1" "$CLIENT_LOG" 2>/dev/null || true; }

find_window() {
    local wid
    wid=$(xdotool search --class '[Ss]cribe' 2>/dev/null | tail -1)
    [ -z "$wid" ] && wid=$(xdotool search --name '[Ss]cribe' 2>/dev/null | tail -1)
    printf '%s' "$wid"
}

focus() {
    local wid
    wid=$(find_window)
    [ -n "$wid" ] || fail "no Scribe window found"
    xdotool windowactivate --sync "$wid" 2>/dev/null \
        || xdotool windowfocus --sync "$wid" 2>/dev/null || true
    # Past the X11 focus guard's 300 ms reactivation debounce.
    sleep 0.8
}

shot() {
    sleep 0.4
    scrot -o "$1"
    echo "captured $1"
}

send_keys() {
    xdotool key --clearmodifiers "$@"
    sleep 0.5
}

type_text() {
    xdotool type --clearmodifiers --delay 80 "$1"
    sleep 1.0
}

WIN_X=0
WIN_Y=0
WIN_W=0
WIN_H=0

measure_window() {
    local wid
    wid=$(find_window)
    [ -n "$wid" ] || fail "no Scribe window found"
    eval "$(xdotool getwindowgeometry --shell "$wid")"
    WIN_X="$X"
    WIN_Y="$Y"
    WIN_W="$WIDTH"
    WIN_H="$HEIGHT"
}

# Crop a full-screen capture down to the *left* part of the terminal grid.
#
# The overlay box itself is anchored top-right and is 360 px wide plus its
# margin, so excluding the right 420 px leaves a region the box never covers:
# any pixel that changes inside this crop is grid paint, which is exactly the
# match highlighting under test. The crop keeps every grid row from the very
# first one — the first match is on it — and drops only the bottom bands, for
# the same reason window-lifecycle.sh drops the status bar: its clock and
# sparklines move pixels on their own.
crop_grid() {
    convert "$1" \
        -crop "$(( WIN_W - 420 ))x$(( WIN_H - 100 ))+${WIN_X}+${WIN_Y}" \
        +repage "$2"
}

# Changed-pixel count between two crops.
pixel_diff() {
    local out
    out=$(compare -metric AE "$1" "$2" null: 2>&1 || true)
    printf '%s' "${out%% *}"
}

# ── Phase 0: put a known needle on the real PTY ───────────────────
sleep 1.0
focus
measure_window
# Cursor blink is not controlled by SCRIBE_DISABLE_ANIMATIONS. Hide only that
# terminal animation before taking the quiet baseline; the full grid remains in
# every crop, so stale search highlights still count against DIFF_MAX.
scribe-test send "$SESSION" "printf '${NEEDLE} ${NEEDLE} ${NEEDLE}\\n\\033[?25l'\n"
scribe-test wait-output "$SESSION" "$NEEDLE" >/dev/null \
    || fail "PHASE 0: the seeded needle never reached the session"
sleep 1.5
shot /output/00-seeded.png
crop_grid /output/00-seeded.png /output/00-grid.png
echo "PHASE 0 PASS: session $SESSION shows the needle the find will look for"

# ── Phase 1: the find chord opens the overlay ─────────────────────
OPENED_BEFORE=$(count_log "opened the find overlay")
focus
send_keys ctrl+shift+f
sleep 0.5
OPENED_AFTER=$(count_log "opened the find overlay")
[ "$OPENED_AFTER" -gt "$OPENED_BEFORE" ] \
    || fail "PHASE 1: the find chord did not open the overlay"
if grep -E "action not wired into the GPUI shell.*OpenFind" "$CLIENT_LOG"; then
    fail "PHASE 1: KeyAction::OpenFind is still being dropped"
fi
shot /output/01-overlay-open.png
echo "PHASE 1 PASS: ctrl+shift+f opened the find overlay"

# ── Phase 2: the query round trips and the grid highlights ────────
REQ_BEFORE=$(count_client SearchRequest "query=$NEEDLE" "session_id=$SESSION")
RES_BEFORE=$(count_server SearchResults "query=$NEEDLE" "session_id=$SESSION")
type_text "$NEEDLE"
wait_for_frames client "$REQ_BEFORE" 20 SearchRequest "query=$NEEDLE" "session_id=$SESSION" \
    || fail "PHASE 2: typing the query put no SearchRequest on the wire"
wait_for_frames server "$RES_BEFORE" 20 SearchResults "query=$NEEDLE" "session_id=$SESSION" \
    || fail "PHASE 2: the server never answered with SearchResults"
[ "$(count_client SearchRequest "query=$NEEDLE" "limit=256")" -gt 0 ] \
    || fail "PHASE 2: the SearchRequest did not carry the 256-match limit"
if grep -E "server message not wired into the GPUI client.*variant=SearchResults" "$CLIENT_LOG"; then
    fail "PHASE 2: SearchResults still fell through to the unhandled counter"
fi
MATCHES=$(largest_reported_match_count)
[ -n "$MATCHES" ] && [ "$MATCHES" -gt 0 ] \
    || fail "PHASE 2: the client consumed no SearchResults with matches"
echo "client consumed SearchResults with $MATCHES matches"
shot /output/02-matches-highlighted.png
crop_grid /output/02-matches-highlighted.png /output/02-grid.png
HIGHLIGHT_DIFF=$(pixel_diff /output/00-grid.png /output/02-grid.png)
[ "${HIGHLIGHT_DIFF:-0}" -gt "$DIFF_MIN" ] \
    || fail "PHASE 2: the matched cells were never repainted (diff $HIGHLIGHT_DIFF)"
echo "PHASE 2 PASS: SearchRequest left the client, SearchResults came back, $HIGHLIGHT_DIFF px repainted"

# ── Phase 3: Enter moves the current match ────────────────────────
# The current match is painted with the opaque accent and a contrast
# foreground while the others are only tinted, so advancing it must move that
# solid run to a different cell range.
send_keys Return
shot /output/03-next-match.png
crop_grid /output/03-next-match.png /output/03-grid.png
CYCLE_DIFF=$(pixel_diff /output/02-grid.png /output/03-grid.png)
[ "${CYCLE_DIFF:-0}" -gt "$DIFF_MIN" ] \
    || fail "PHASE 3: Enter did not move the highlighted match (diff $CYCLE_DIFF)"
echo "PHASE 3 PASS: Enter advanced the current match, $CYCLE_DIFF px repainted"

# ── Phase 4: Escape closes and releases the keyboard ──────────────
send_keys Escape
shot /output/04-overlay-closed.png
crop_grid /output/04-overlay-closed.png /output/04-grid.png
CLEARED_DIFF=$(pixel_diff /output/00-grid.png /output/04-grid.png)
[ "${CLEARED_DIFF:-0}" -le "$DIFF_MAX" ] \
    || fail "PHASE 4: closing the overlay left highlights behind (diff $CLEARED_DIFF)"
# A still-open overlay would swallow its own chord, so a second open is the
# proof that Escape really released the keyboard.
REOPEN_BEFORE=$(count_log "opened the find overlay")
send_keys ctrl+shift+f
sleep 0.5
[ "$(count_log "opened the find overlay")" -gt "$REOPEN_BEFORE" ] \
    || fail "PHASE 4: the overlay did not reopen, so Escape never closed it"
shot /output/05-reopened.png
echo "PHASE 4 PASS: Escape cleared every highlight and released the keyboard"

echo ""
echo "PASS: visual find-overlay test"
echo "  Inspect screenshots in test-output/:"
echo "    00-seeded.png              — the pane before any find"
echo "    01-overlay-open.png        — the find chord opened the overlay"
echo "    02-matches-highlighted.png — the query's matches painted on the grid"
echo "    03-next-match.png          — Enter moved the current match"
echo "    04-overlay-closed.png      — Escape dropped every highlight"
echo "    05-reopened.png            — the chord reopened the overlay"
echo "  Wire record: test-output/share-wire.jsonl"
