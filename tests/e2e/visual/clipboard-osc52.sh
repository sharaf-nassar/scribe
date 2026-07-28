#!/bin/bash
# Scripted + visual E2E: the GPUI client's clipboard surfaces, end to end.
#
# Everything under test here is a round trip that no headless test can show.
#
#   * OSC 52 is a two-hop bridge. A PTY-side program emits the escape, the
#     SERVER decides policy, and the only way the request is ever honoured is a
#     client that (a) announced `clipboard_gating: true` in `Hello`, (b) raises
#     the confirmation modal, (c) puts `ClipboardPromptResponse` back on the
#     wire, and (d) answers `ClipboardBridgeReadRequest` with a real
#     `ClipboardBridgeReadReply` carrying the host clipboard's contents. Before
#     this bead the client cleared the gating bit, so the server took its
#     headless-deny path and not one `Clipboard*` frame was ever sent.
#
#   * Copy and paste need a live selection, a real X11 clipboard, and the real
#     key path. The copy chord serialises the pane's selection into the host
#     clipboard; the paste chord reads it back and types it into the PTY.
#
# The wire tap (`scribe-test share-tap`, SCRIBE_SHARE_TAP=1) is interposed on
# the server socket purely as a recorder — nothing is injected. Every frame
# asserted below is one the real client or the real server chose to send.
#
# Phase 0 is the session-adoption dance `tab-window-chords.sh` documents: the
# entrypoint creates $SESSION after the client launched, so the running client
# never hears about it, and only a relaunch after the test daemon releases
# ownership picks it up through `ListSessions`. It matters twice over here:
# the server routes an OSC 52 prompt to the window's CONTROLLER, and after the
# daemon is gone the GPUI client is the window's only participant.
#
# Input is driven through XTEST (plain `xdotool key` / `type` / `mousemove`,
# never `--window`): GPUI reads input through XInput2 and ignores the synthetic
# events `xdotool --window` sends with XSendEvent.
#
# Requires: visual container with SCRIBE_SHARE_TAP=1 and an SCRIBE_EXTRA_CONFIG
# that puts both OSC 52 policy axes in `prompt` mode (`just e2e-visual-clipboard`);
# xdotool, scrot, xclip, python3.
set -e

RECORD="${SHARE_WIRE_RECORD:-/output/share-wire.jsonl}"
CLIENT_LOG="${SCRIBE_CLIENT_LOG:-/output/client.log}"

# Payload a PTY-side program asks to put on the host clipboard over OSC 52.
OSC52_WRITE_PAYLOAD="scribe-osc52-write-payload"
# Text seeded on the host clipboard before the OSC 52 read is triggered.
OSC52_READ_PAYLOAD="scribe-osc52-read-payload"
# Text seeded on the host clipboard for the paste-chord phase.
PASTE_PAYLOAD="scribe-pasted-by-chord"
# Word typed into the pane and then selected with the mouse for the copy phase.
COPY_NEEDLE="scribecopyneedle"

# Minimum changed pixels for a capture comparison to count as "the grid
# repainted". Well below one echoed line of glyphs and far above capture noise.
DIFF_MIN="${DIFF_MIN:-40}"

fail() {
    echo "FAIL: $1" >&2
    echo "--- client log tail ---" >&2
    tail -60 "$CLIENT_LOG" >&2 || true
    echo "--- wire record tail ---" >&2
    tail -20 "$RECORD" >&2 || true
    exit 1
}

# Count recorded frames of `type` in `dir` matching every key=value pair. A
# value that parses as JSON is compared as JSON, so a bare word matches the
# string and `request_id=7` matches the number.
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

# Count `ClipboardBridgeReadReply` frames whose Ok payload contains `$1`. The
# payload is a serde `Result`, so a successful read is `{"Ok": "…"}`.
count_read_replies_containing() {
    python3 - "$RECORD" "$1" <<'PY'
import json, sys

path, needle = sys.argv[1], sys.argv[2]
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
        if message.get("type") != "ClipboardBridgeReadReply":
            continue
        payload = message.get("payload")
        if isinstance(payload, dict) and needle in str(payload.get("Ok", "")):
            total += 1
print(total)
PY
}

# Count `KeyInput` frames whose bytes contain `$1`. A paste reaches the server
# as ordinary key input, so this is where "the paste really left the client"
# is observable as a frame rather than as a screenshot.
count_key_input_containing() {
    python3 - "$RECORD" "$1" <<'PY'
import json, sys

path, needle = sys.argv[1], sys.argv[2].encode()
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
        data = message.get("data") or []
        try:
            raw = bytes(data)
        except (TypeError, ValueError):
            continue
        if needle in raw:
            total += 1
print(total)
PY
}

# Wait until a counting function's result exceeds `baseline`.
wait_for_count() {
    local baseline="$1" timeout_secs="$2"
    shift 2
    local started
    started=$(date +%s)
    while true; do
        if [ "$("$@")" -gt "$baseline" ]; then
            return 0
        fi
        if [ $(( "$(date +%s)" - started )) -ge "$timeout_secs" ]; then
            return 1
        fi
        sleep 0.3
    done
}

count_log() { grep -acF "$1" "$CLIENT_LOG" 2>/dev/null || true; }

wait_for_log_growth() {
    local pattern="$1" baseline="$2" timeout_secs="${3:-15}" started
    started=$(date +%s)
    while true; do
        if [ "$(count_log "$pattern")" -gt "$baseline" ]; then
            return 0
        fi
        if [ $(( "$(date +%s)" - started )) -ge "$timeout_secs" ]; then
            return 1
        fi
        sleep 0.3
    done
}

list_windows() {
    xdotool search --class '[Ss]cribe' 2>/dev/null \
        || xdotool search --name '[Ss]cribe' 2>/dev/null || true
}

WIN_X=0
WIN_Y=0
WIN_W=0
WIN_H=0

focus() {
    local wid
    wid=$(list_windows | tail -1)
    [ -n "$wid" ] || fail "no Scribe window found"
    xdotool windowactivate --sync "$wid" 2>/dev/null \
        || xdotool windowfocus --sync "$wid" 2>/dev/null || true
    # Past the X11 focus guard's 300 ms reactivation debounce.
    sleep 0.8
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

# Changed-pixel count between two captures, cropped to the terminal grid.
#
# The bottom bands are excluded for the same reason window-lifecycle.sh
# excludes them: the status bar clock and sparklines move pixels on their own,
# so any diff that included them would pass without the grid changing at all.
pixel_diff() {
    local crop="${WIN_W}x$(( WIN_H - 120 ))+${WIN_X}+${WIN_Y}"
    local out
    out=$(compare -metric AE \
        \( "$1" -crop "$crop" +repage \) \
        \( "$2" -crop "$crop" +repage \) \
        null: 2>&1 || true)
    printf '%s' "${out%%.*}"
}

send_keys() {
    xdotool key --clearmodifiers "$@"
    sleep 0.5
}

type_line() {
    xdotool type --clearmodifiers --delay 30 "$1"
    xdotool key --clearmodifiers Return
    sleep 0.8
}

# Put text on the X11 clipboard. xclip forks a daemon to serve the selection;
# its fds are redirected so that daemon cannot inherit the entrypoint's `tee`
# pipe and keep the container alive.
set_clipboard() {
    printf '%s' "$1" | xclip -selection clipboard >/dev/null 2>&1
    sleep 0.4
}

read_clipboard() {
    xclip -o -selection clipboard 2>/dev/null || true
}

# Answer the OSC 52 modal with "Allow once". The four buttons render
# Deny once / Always deny / Allow once / Always allow and focus starts on the
# safe Deny once, so two Rights land on Allow once.
allow_once() {
    send_keys Right
    send_keys Right
    send_keys Return
}

# ── Phase 0: hand the client sole ownership of a live pane ────────
sleep 1.0
kill "${SCRIBE_CLIENT_PID:-0}" 2>/dev/null || true
for _ in $(seq 1 40); do
    pgrep -f 'scribe-client' >/dev/null 2>&1 || break
    sleep 0.25
done
scribe-test daemon stop >/dev/null 2>&1 || true
sleep 1.0
scribe-client >>"$CLIENT_LOG" 2>&1 &
xdotool search --sync --name "Scribe" >/dev/null 2>&1 || true
sleep 2
focus
# The tracing subscriber writes ANSI-styled field names, so the escapes are
# stripped before the negotiated capability is read out of the Welcome line.
strip_ansi() { sed 's/\x1b\[[0-9;]*m//g' "$CLIENT_LOG"; }
strip_ansi | grep -q "clipboard_gating=true" \
    || fail "PHASE 0: the client never negotiated OSC 52 gating in Welcome"
shot /output/00-attached.png
echo "PHASE 0 PASS: client owns session $SESSION and negotiated clipboard gating"

# ── Phase 1: an OSC 52 write prompts, is allowed, and lands ───────
# `printf` inside the real pane emits the escape from the PTY side, which is
# the only way the server's clipboard policy engine is ever reached.
set_clipboard "clipboard-before-osc52-write"
RAISED_BEFORE=$(count_log "raising the OSC 52 confirmation prompt")
type_line "printf '\\033]52;c;'\$(printf '%s' '$OSC52_WRITE_PAYLOAD' | base64 -w0)'\\a'"
wait_for_log_growth "raising the OSC 52 confirmation prompt" "$RAISED_BEFORE" 20 \
    || fail "PHASE 1: the server's ClipboardPromptRequest never raised a modal"
if grep -E "server message not wired.*ClipboardPromptRequest" "$CLIENT_LOG"; then
    fail "PHASE 1: ClipboardPromptRequest still falls through to the drop counter"
fi
shot /output/01-write-prompt.png
RESPONSES_BEFORE=$(count_client ClipboardPromptResponse decision=allow_once)
focus
allow_once
wait_for_count "$RESPONSES_BEFORE" 20 count_client ClipboardPromptResponse decision=allow_once \
    || fail "PHASE 1: no ClipboardPromptResponse left the client"
# The server only forwards the write after the response resolves the prompt.
wait_for_count 0 20 count_server ClipboardBridgeWrite \
    || fail "PHASE 1: the server never forwarded ClipboardBridgeWrite"
for _ in $(seq 1 40); do
    [ "$(read_clipboard)" = "$OSC52_WRITE_PAYLOAD" ] && break
    sleep 0.3
done
[ "$(read_clipboard)" = "$OSC52_WRITE_PAYLOAD" ] \
    || fail "PHASE 1: the host clipboard still holds '$(read_clipboard)'"
shot /output/02-write-applied.png
echo "PHASE 1 PASS: OSC 52 write prompted, was answered on the wire, and reached X11"

# ── Phase 2: an OSC 52 read answers with the host clipboard ───────
set_clipboard "$OSC52_READ_PAYLOAD"
RAISED_BEFORE=$(count_log "raising the OSC 52 confirmation prompt")
type_line "printf '\\033]52;c;?\\a'"
wait_for_log_growth "raising the OSC 52 confirmation prompt" "$RAISED_BEFORE" 20 \
    || fail "PHASE 2: the OSC 52 read raised no modal"
shot /output/03-read-prompt.png
focus
allow_once
wait_for_count 0 20 count_server ClipboardBridgeReadRequest \
    || fail "PHASE 2: the server never asked the client to read the host clipboard"
wait_for_count 0 20 count_read_replies_containing "$OSC52_READ_PAYLOAD" \
    || fail "PHASE 2: no ClipboardBridgeReadReply carried the clipboard contents"
if grep -E "server message not wired.*ClipboardBridgeReadRequest" "$CLIENT_LOG"; then
    fail "PHASE 2: ClipboardBridgeReadRequest still falls through to the drop counter"
fi
shot /output/04-read-answered.png
echo "PHASE 2 PASS: the host clipboard crossed the wire as ClipboardBridgeReadReply"

# ── Phase 3: a mouse selection copies with the copy chord ─────────
# The needle is echoed on its own line, so a drag across that line selects a
# known string and the copy chord must produce exactly it.
set_clipboard "clipboard-before-copy"
# The server writes the OSC 52 read reply back into the PTY, so the shell has
# an escape sequence sitting on its input line. Ctrl+C drops it, and `clear`
# puts the next command at the very top of the grid.
send_keys ctrl+c
# A band of identical needle lines rather than one: the grid rows the drag
# crosses then do not depend on the font metrics the container resolves, so
# the gesture is about selection behaviour and not about pixel arithmetic.
type_line "clear; yes $COPY_NEEDLE | head -20"
sleep 1.0
focus
shot /output/05-before-selection.png
# The echoed output line sits just above the shell's next prompt. Drag along a
# generous span of it starting at the left margin; trailing blanks are trimmed
# out of the extracted text, so overshooting the word is harmless.
# The needle band starts at the top of the cleared grid, just under the tab
# strip, so the drag runs down the first rows of the viewport.
DRAG_TOP=$(( WIN_Y + 80 ))
DRAG_BOTTOM=$(( WIN_Y + 220 ))
xdotool mousemove $(( WIN_X + 12 )) "$DRAG_TOP"
xdotool mousedown 1
sleep 0.3
xdotool mousemove $(( WIN_X + 300 )) $(( (DRAG_TOP + DRAG_BOTTOM) / 2 ))
sleep 0.2
xdotool mousemove $(( WIN_X + 600 )) "$DRAG_BOTTOM"
sleep 0.3
xdotool mouseup 1
sleep 0.6
shot /output/06-selection-highlight.png
if [ "$(read_clipboard)" = "$COPY_NEEDLE" ]; then
    fail "PHASE 3: the clipboard held the needle before the copy chord ran"
fi
COPIES_BEFORE=$(count_log "copied to the host clipboard")
send_keys ctrl+shift+c
wait_for_log_growth "copied to the host clipboard" "$COPIES_BEFORE" 15 \
    || fail "PHASE 3: the copy chord never reached the clipboard (still swallowed?)"
if grep -E "action not wired into the GPUI shell.*CopySelection" "$CLIENT_LOG"; then
    fail "PHASE 3: LayoutAction::CopySelection is still being dropped"
fi
COPIED=$(read_clipboard)
case "$COPIED" in
    *"$COPY_NEEDLE"*) ;;
    *) fail "PHASE 3: the clipboard holds '$COPIED', not the selected needle" ;;
esac
shot /output/07-after-copy.png
echo "PHASE 3 PASS: the dragged selection reached the host clipboard as '$COPIED'"

# ── Phase 4: the paste chord types the clipboard into the pane ────
set_clipboard "$PASTE_PAYLOAD"
PASTES_BEFORE=$(count_key_input_containing "$PASTE_PAYLOAD")
focus
send_keys ctrl+shift+v
wait_for_count "$PASTES_BEFORE" 20 count_key_input_containing "$PASTE_PAYLOAD" \
    || fail "PHASE 4: the pasted bytes never reached the wire as KeyInput"
if grep -E "action not wired into the GPUI shell.*PasteClipboard" "$CLIENT_LOG"; then
    fail "PHASE 4: LayoutAction::PasteClipboard is still being dropped"
fi
# The shell echoes what was pasted, so the same bytes come back as PTY output
# and repaint the grid. The daemon was stopped in phase 0 to make this client
# the window controller, so the echo is asserted as pixels rather than through
# `scribe-test wait-output`.
shot /output/08-after-paste.png
ECHO_DIFF=$(pixel_diff /output/07-after-copy.png /output/08-after-paste.png)
[ "${ECHO_DIFF:-0}" -gt "$DIFF_MIN" ] \
    || fail "PHASE 4: the pasted text never repainted the pane (diff $ECHO_DIFF)"
echo "PHASE 4 PASS: the paste chord put the clipboard on the wire and into the pane"

echo ""
echo "PASS: visual clipboard / OSC 52 test"
echo "  Inspect screenshots in test-output/:"
echo "    01-write-prompt.png       — the OSC 52 write confirmation modal"
echo "    02-write-applied.png      — the pane after the allowed write"
echo "    03-read-prompt.png        — the OSC 52 read confirmation modal"
echo "    04-read-answered.png      — the pane after the answered read"
echo "    06-selection-highlight.png— the dragged selection painted on the grid"
echo "    07-after-copy.png         — the pane after the copy chord"
echo "    08-after-paste.png        — the pasted text echoed by the shell"
echo "  Wire record: test-output/share-wire.jsonl"
