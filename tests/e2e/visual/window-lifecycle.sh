#!/bin/bash
[ "${SCRIBE_E2E_SANDBOX:-0}" = "1" ] || { echo "FATAL: this script only runs inside the scribe e2e container (use just e2e-func / e2e-visual)." >&2; exit 99; }
# Scripted E2E: the GPUI client's window lifecycle, asserted on the real wire.
#
# Seven protocol messages make up a window's own lifecycle and none of them can
# be shown by a headless test: `CloseWindow`, `QuitAll`, `ListWindows` and
# `FocusChanged` only exist if the running window emits them, and `WindowClosed`
# / `QuitRequested` / `WindowList` only matter if the running window acts on
# them. So this test drives the real client against the real `scribe-server`
# and reads the frames off the wire.
#
# The wire tap (`scribe-test share-tap`, SCRIBE_SHARE_TAP=1) is interposed on
# the server socket purely as a recorder here — nothing is injected. Every
# server frame this test asserts on is one the real server chose to send in
# answer to something the real client sent, so a passing run is an end-to-end
# round trip, not a replay.
#
# Phases:
#   0. discover the live pane the client bootstrapped and focused;
#   1. the window-list poll leaves the client and the reply is acted on;
#   2. an OS focus change and a new tab each put a `FocusChanged` on the wire;
#   3. exiting one of two terminals keeps the window open, while exiting the
#      last sends `CloseWindow` and exits after `WindowClosed`;
#   4. the WM's close button raises the close dialog, "Quit Scribe" puts
#      `QuitAll` on the wire, and the server's `QuitRequested` exits the client;
#   5. with two windows open in the one client process, "Kill Window" puts
#      `CloseWindow` on the wire for the focused window only, and the server's
#      `WindowClosed` takes that window down while its sibling stays up;
#   6. killing the last remaining window ends the process.
#
# Phase 0 reads the client's own `FocusChanged` frame rather than the separate
# session the entrypoint creates for `scribe-test`. Fresh-window bootstrap gives
# the client a real shell before the script starts; a newly created shell is
# already attached, so its focus report is the exact visible-session oracle.
#
# `remote.enabled = true` is seeded through SCRIBE_EXTRA_CONFIG because the
# window-list poll is gated on it, exactly as the winit client gates it: the
# reply's only rendered consumer is the status bar's owning-machine
# remote-control summary, which is not drawn while remote control is off. The
# server is already running by the time that file is written, so nothing on the
# server side is remote-enabled — this only turns the client's poll on.
#
# Input is driven through XTEST (plain `xdotool key`, no `--window`), matching
# overlay-actions.sh.
#
# Requires: visual container with SCRIBE_SHARE_TAP=1 and SCRIBE_EXTRA_CONFIG
# seeding `[remote] enabled = true`; xdotool, scrot, python3.
set -e

RECORD="${SHARE_WIRE_RECORD:-/output/share-wire.jsonl}"
CLIENT_LOG="${SCRIBE_CLIENT_LOG:-/output/client.log}"
FOCUS_PROBE=/tmp/window-lifecycle-focus-probe.py
FOCUS_PROBE_PREFIX=/tmp/window-lifecycle-focus-probe
FOCUS_PROBE_LOG=/output/window-lifecycle-focus-probe.log
SESSION=""

fail() {
    echo "FAIL: $1" >&2
    echo "--- client log tail ---" >&2
    tail -40 "$CLIENT_LOG" >&2 || true
    if [ -f "$FOCUS_PROBE_LOG" ]; then
        echo "--- focus probe log ---" >&2
        cat "$FOCUS_PROBE_LOG" >&2 || true
    fi
    exit 1
}

# Count recorded frames of `type` in `dir` matching every key=value pair. A
# value that parses as JSON is compared as JSON (so `gained=null` matches a
# JSON null and a bare uuid matches the string), which is how the focus and
# window-id assertions below can be exact.
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

# The window id the server handed the *newest* client connection in its
# `Welcome`. A `CloseWindow` must name exactly this id or the server refuses it,
# and the test relaunches the client, so the last one recorded is the live one.
last_welcome_window() {
    python3 - "$RECORD" <<'PY'
import json, sys

found = None
with open(sys.argv[1]) as handle:
    for line in handle:
        try:
            row = json.loads(line)
        except ValueError:
            continue
        message = row.get("message", {})
        if row.get("dir") == "server" and message.get("type") == "Welcome":
            found = message.get("window_id")
if found is None:
    sys.exit(1)
print(found)
PY
}

last_focused_session() {
    python3 - "$RECORD" <<'PY'
import json, sys

found = None
with open(sys.argv[1]) as handle:
    for line in handle:
        try:
            row = json.loads(line)
        except ValueError:
            continue
        message = row.get("message", {})
        gained = message.get("gained")
        if row.get("dir") == "client" and message.get("type") == "FocusChanged" and gained:
            found = gained
if found is None:
    sys.exit(1)
print(found)
PY
}

# A `FocusChanged` that moved focus off `$SESSION` onto some *other* pane. The
# new session's id is minted by the server when the tab is created, so it
# cannot be spelled out in advance — only its shape can.
count_focus_moved_off_session() {
    python3 - "$RECORD" "$SESSION" <<'PY'
import json, sys

path, session = sys.argv[1], sys.argv[2]
total = 0
with open(path) as handle:
    for line in handle:
        try:
            row = json.loads(line)
        except ValueError:
            continue
        message = row.get("message", {})
        if row.get("dir") != "client" or message.get("type") != "FocusChanged":
            continue
        gained = message.get("gained")
        if message.get("lost") == session and gained not in (None, session):
            total += 1
print(total)
PY
}

find_window() {
    local wid
    wid=$(xdotool search --class '[Ss]cribe' 2>/dev/null | tail -1)
    [ -z "$wid" ] && wid=$(xdotool search --name '[Ss]cribe' 2>/dev/null | tail -1)
    printf '%s' "$wid"
}

# Every mapped Scribe window, one X11 id per line and sorted, so two captures
# can be compared as sets. One client process hosts them all, which is exactly
# what the kill-one-window phase has to prove.
scribe_windows() {
    { xdotool search --onlyvisible --class '[Ss]cribe' 2>/dev/null || true
      xdotool search --onlyvisible --name '[Ss]cribe' 2>/dev/null || true
    } | sort -u
}

focus_window() {
    xdotool windowactivate --sync "$1" 2>/dev/null \
        || xdotool windowfocus --sync "$1" 2>/dev/null || true
    # Past the X11 focus guard's 300 ms reactivation debounce, as in `focus`.
    sleep 0.8
}

# Wait until `$2...` succeeds, or give up after `$1` seconds.
wait_until() {
    local timeout_secs="$1" started
    shift
    started=$(date +%s)
    until "$@"; do
        if [ $(( "$(date +%s)" - started )) -ge "$timeout_secs" ]; then
            return 1
        fi
        sleep 0.3
    done
    return 0
}

window_count_is() { [ "$(scribe_windows | wc -l)" -eq "$1" ]; }
window_is_gone() { ! scribe_windows | grep -qx "$1"; }
window_is_active() { [ "$(xdotool getactivewindow 2>/dev/null)" = "$1" ]; }

# Whether the server has relayed a byte sequence from this session. Joining
# adjacent PtyOutput chunks keeps the check stable when one PTY write is split.
server_output_contains() {
    python3 - "$RECORD" "$1" "$2" <<'PY'
import json, sys

path, session, wanted = sys.argv[1], sys.argv[2], bytes.fromhex(sys.argv[3])
output = bytearray()
try:
    handle = open(path)
except OSError:
    sys.exit(1)
with handle:
    for line in handle:
        try:
            row = json.loads(line)
        except ValueError:
            continue
        message = row.get("message", {})
        if (
            row.get("dir") == "server"
            and message.get("type") == "PtyOutput"
            and str(message.get("session_id")) == session
        ):
            output.extend(message.get("data") or [])
sys.exit(wanted not in output)
PY
}

focus() {
    local wid
    wid=$(find_window)
    [ -n "$wid" ] || fail "no Scribe window found"
    xdotool windowactivate --sync "$wid" 2>/dev/null \
        || xdotool windowfocus --sync "$wid" 2>/dev/null || true
    # Past the X11 focus guard's 300 ms reactivation debounce, so a keystroke
    # sent right after a re-activation is not swallowed by the guard.
    sleep 0.8
}

shot() {
    sleep 0.3
    scrot -o "$1"
    echo "captured $1"
}

WIN_X=0
WIN_Y=0
WIN_W=0
WIN_H=0

# Cache the client window's on-screen geometry so a full-screen capture can be
# cropped down to just its body.
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

# Crop a full-screen capture to the window minus its bottom band, because the
# status bar's sparklines resample every 2 s and would move pixels on their own.
crop_body() {
    convert "$1" -crop "${WIN_W}x$(( WIN_H - 60 ))+${WIN_X}+${WIN_Y}" +repage "$2"
}

send_keys() {
    xdotool key --clearmodifiers "$@"
    sleep 0.5
}

type_text() {
    xdotool type --clearmodifiers --delay 40 "$1"
    sleep 0.3
}

# Wait for every scribe-client process to be gone.
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

# Write the bounded raw-PTY stand-in used for Claude Code 2.1.228's focus-mode
# suspend/restore sequence. State files synchronize the X11 driver without
# reading the PTY while focus reporting is suspended.
write_focus_probe() {
    cat >"$FOCUS_PROBE" <<'PY'
import os
import select
import sys
import termios
import time
import tty
from pathlib import Path

PREFIX = Path("/tmp/window-lifecycle-focus-probe")
TRACE = Path("/output/window-lifecycle-focus-probe.log")
INITIAL = b"\x1b[?1049h\x1b[?25l\x1b[?1004h"
SUSPEND = (
    b"\x1b[<u\x1b[>4m\x1b[?1049h\x1b[?1004l\x1b[?2004l"
    b"\x1b[?2031l\x1b[0m\x1b[?25h\x1b[2J\x1b[H"
)
RESTORE = (
    b"\x1b[?1049l\x1b[?25l\x1b[<u\x1b[>4m\x1b[?1004h"
    b"\x1b[?2004h\x1b[?2031h"
)
RESET = b"\x1b[?1004l\x1b[?2004l\x1b[?2031l\x1b[?25h\x1b[0m"


def marker(name):
    return Path(f"{PREFIX}.{name}")


def record(label, data=b""):
    with TRACE.open("a") as handle:
        handle.write(f"{label} {data.hex()}\n")


def read_through(needle, label, buffered=b"", timeout=15):
    deadline = time.monotonic() + timeout
    while needle not in buffered:
        remaining = deadline - time.monotonic()
        if remaining <= 0 or not select.select([sys.stdin], [], [], remaining)[0]:
            raise TimeoutError(f"timed out waiting for {label}")
        chunk = os.read(sys.stdin.fileno(), 1024)
        if not chunk:
            raise EOFError(f"PTY closed while waiting for {label}")
        buffered += chunk
    end = buffered.index(needle) + len(needle)
    record(label, buffered[:end])
    marker(label).touch()
    return buffered[end:]


for name in ("initial-I", "blur-O", "suspended", "restore", "second-I", "done", "failed"):
    marker(name).unlink(missing_ok=True)
TRACE.write_text("")

fd = sys.stdin.fileno()
saved = termios.tcgetattr(fd)
ok = False
try:
    tty.setraw(fd)
    os.write(sys.stdout.fileno(), INITIAL)
    pending = read_through(b"\x1b[I", "initial-I")
    pending = read_through(b"\x1b[O", "blur-O", pending)
    os.write(sys.stdout.fileno(), SUSPEND)
    record("suspend-output", SUSPEND)
    marker("suspended").touch()

    deadline = time.monotonic() + 15
    while not marker("restore").exists():
        if time.monotonic() >= deadline:
            raise TimeoutError("timed out waiting for restore signal")
        time.sleep(0.01)

    os.write(sys.stdout.fileno(), RESTORE)
    record("restore-output", RESTORE)
    read_through(b"\x1b[I", "second-I", pending)
    ok = True
except Exception as error:
    record("error", str(error).encode())
    marker("failed").touch()
    raise
finally:
    os.write(sys.stdout.fileno(), RESET)
    termios.tcsetattr(fd, termios.TCSANOW, saved)

if ok:
    marker("done").touch()
    print("\r\nFOCUS_PROBE_DONE", flush=True)
PY
}

# ── Phase 0: identify the client's live pane ──────────────────────
sleep 1.0
SESSION=$(last_focused_session) || fail "PHASE 0: the client never focused a live session"
focus
shot /output/00-attached.png
echo "PHASE 0 PASS: the client attached to session $SESSION"

# ── Phase 1: the window-list poll round trips ─────────────────────
# `ListWindows` is throttled to one send every 2 s and is answered by exactly
# one `WindowList`, so a growing pair proves the poll is live on both ends.
LIST_BEFORE=$(count_client ListWindows)
REPLY_BEFORE=$(count_server WindowList)
LOG_BEFORE=$(grep -c "server window list" "$CLIENT_LOG" || true)
wait_for_frames client "$LIST_BEFORE" 20 ListWindows \
    || fail "PHASE 1: the client never sent ListWindows"
wait_for_frames server "$REPLY_BEFORE" 20 WindowList \
    || fail "PHASE 1: the server never answered with WindowList"
LOG_AFTER=$(grep -c "server window list" "$CLIENT_LOG" || true)
[ "$LOG_AFTER" -gt "$LOG_BEFORE" ] \
    || fail "PHASE 1: the client never acted on the WindowList reply"
if grep -E "server message not wired into the GPUI client.*variant=WindowList" "$CLIENT_LOG"; then
    fail "PHASE 1: WindowList still fell through to the unhandled counter"
fi
echo "PHASE 1 PASS: ListWindows left the client and its WindowList reply was handled"

# ── Phase 2a: an OS focus change is reported ──────────────────────
# Iconifying the window makes openbox take X input focus away from it, which is
# the FocusOut the client's activation observer reports as a focus loss; mapping
# and re-activating it reports the matching gain.
BLUR_BEFORE=$(count_client FocusChanged "gained=null" "lost=$SESSION")
WID=$(find_window)
xdotool windowminimize "$WID"
wait_for_frames client "$BLUR_BEFORE" 15 FocusChanged "gained=null" "lost=$SESSION" \
    || fail "PHASE 2a: losing OS focus put no FocusChanged on the wire"
GAIN_BEFORE=$(count_client FocusChanged "gained=$SESSION" "lost=null")
xdotool windowmap "$WID" 2>/dev/null || true
focus
wait_for_frames client "$GAIN_BEFORE" 15 FocusChanged "gained=$SESSION" "lost=null" \
    || fail "PHASE 2a: regaining OS focus put no FocusChanged on the wire"
shot /output/01-refocused.png
echo "PHASE 2a PASS: both edges of an OS focus change reached the server"

# ── Phase 2b: switching panes is reported ─────────────────────────
# Ctrl+Shift+T creates a real session; the client focuses and attaches the new
# tab, which moves the focused pane and therefore reports a gain *and* a loss.
MOVED_BEFORE=$(count_focus_moved_off_session)
send_keys ctrl+shift+t
started=$(date +%s)
while [ "$(count_focus_moved_off_session)" -le "$MOVED_BEFORE" ]; do
    if [ $(( "$(date +%s)" - started )) -ge 20 ]; then
        fail "PHASE 2b: a new tab moved focus without reporting FocusChanged"
    fi
    sleep 0.3
done
shot /output/02-second-tab.png
echo "PHASE 2b PASS: moving the focused pane reported gained and lost together"

# ── Phase 3: the final terminal exit closes its window ───────────
WIN=$(last_welcome_window) || fail "PHASE 3: the client never got a Welcome"
CLOSE_BEFORE=$(count_client CloseWindow "window_id=$WIN")
EXIT_BEFORE=$(count_server SessionExited)
type_text "exit"
send_keys Return
wait_for_frames server "$EXIT_BEFORE" 15 SessionExited \
    || fail "PHASE 3: the second terminal never exited"
sleep 1.0
pgrep -f 'scribe-client' >/dev/null 2>&1 \
    || fail "PHASE 3: exiting one of two terminals closed the window"
[ "$(count_client CloseWindow "window_id=$WIN")" -eq "$CLOSE_BEFORE" ] \
    || fail "PHASE 3: exiting one of two terminals sent CloseWindow"
focus
shot /output/03-one-terminal-left.png

EXIT_BEFORE=$(count_server SessionExited)
CLOSED_BEFORE=$(count_server WindowClosed "window_id=$WIN")
type_text "exit"
send_keys Return
wait_for_frames server "$EXIT_BEFORE" 15 SessionExited \
    || fail "PHASE 3: the final terminal never exited"
wait_for_frames client "$CLOSE_BEFORE" 15 CloseWindow "window_id=$WIN" \
    || fail "PHASE 3: final terminal exit put no CloseWindow on the wire"
wait_for_frames server "$CLOSED_BEFORE" 15 WindowClosed "window_id=$WIN" \
    || fail "PHASE 3: server never acknowledged the empty window's close"
wait_for_client_exit 20 || fail "PHASE 3: final terminal exit left the client running"
echo "PHASE 3 PASS: one exit kept the window open; the final exit closed it after the ack"

# ── Phase 4: the close dialog's Quit Scribe quits every window ────
# Alt+F4 is openbox's Close action, which sends WM_DELETE_WINDOW — the same
# message the decoration's close button sends, and the one GPUI turns into the
# client's `on_window_should_close` hook. (`xdotool windowclose` is *not* used:
# it calls XDestroyWindow and bypasses the WM protocol entirely.) The client
# must veto that close and raise its own dialog instead, because the server owns
# this window's sessions.
launch_client
focus
measure_window
shot /output/03a-before-close.png
crop_body /output/03a-before-close.png /output/03a-body.png
send_keys alt+F4
sleep 1.0
shot /output/03-close-dialog.png
pgrep -f 'scribe-client' >/dev/null 2>&1 \
    || fail "PHASE 4: the client closed on WM_DELETE_WINDOW instead of asking"
# The modal dims and covers the grid, so a changed body crop is the dialog and
# nothing else; the crop excludes the status bar, whose sparklines resample
# every 2 s and would change pixels on their own.
crop_body /output/03-close-dialog.png /output/03-body.png
DIALOG_DIFF=$(compare -metric AE /output/03a-body.png /output/03-body.png null: 2>&1 || true)
[ "${DIALOG_DIFF%% *}" != "0" ] \
    || fail "PHASE 4: WM_DELETE_WINDOW painted no close dialog"
QUIT_BEFORE=$(count_client QuitAll)
ACK_BEFORE=$(count_server QuitRequested)
# Cancel holds default focus, so one Tab lands on the accent Quit Scribe button.
send_keys Tab
shot /output/04-quit-focused.png
send_keys Return
wait_for_frames client "$QUIT_BEFORE" 15 QuitAll \
    || fail "PHASE 4: Quit Scribe put no QuitAll on the wire"
wait_for_frames server "$ACK_BEFORE" 15 QuitRequested \
    || fail "PHASE 4: the server never broadcast QuitRequested"
wait_for_client_exit 20 || fail "PHASE 4: the client ignored QuitRequested and stayed up"
echo "PHASE 4 PASS: WM close raised the dialog, Quit Scribe sent QuitAll, the ack exited the app"

# ── Phase 5: refocus repair, then Kill Window kills only this window ──
# Sessions survived the quit-all, but this phase needs no pane: `CloseWindow`
# names the window the fresh `Welcome` assigns and destroys whatever it owns.
#
# The second window is the whole point. One client process hosts every window
# the user opens, so a "Kill Window" that ends the PROCESS takes its siblings
# down with it — and merely closes them, since nobody asked the server to
# destroy their sessions. That is invisible with one window open, which is how
# it shipped, so the assertion here is on the sibling: still mapped, still
# served by a live process, and never named in a `CloseWindow`.
launch_client
WIN=$(last_welcome_window) || fail "PHASE 5: the relaunched client never got a Welcome"
echo "relaunched client window id: $WIN"
focus
PROBE_SESSION=$(last_focused_session) \
    || fail "PHASE 5a: the relaunched client never focused a live session"
write_focus_probe
type_text "python3 $FOCUS_PROBE"
send_keys Return
wait_until 15 test -f "$FOCUS_PROBE_PREFIX.initial-I" \
    || fail "PHASE 5a: enabling focus reporting produced no initial ESC[I"

BEFORE_WINDOWS=$(scribe_windows)
send_keys ctrl+shift+n
wait_until 15 window_count_is 2 \
    || fail "PHASE 5: ctrl+shift+n never opened a second window"
VICTIM_WID=$(comm -13 <(printf '%s\n' "$BEFORE_WINDOWS") <(scribe_windows))
[ -n "$VICTIM_WID" ] || fail "PHASE 5: could not identify the new window"
SIBLING_WID=$BEFORE_WINDOWS
VICTIM=$(last_welcome_window) || fail "PHASE 5: the new window never got a Welcome"
[ "$VICTIM" != "$WIN" ] || fail "PHASE 5: the new window adopted the existing window's id"
echo "second window id: $VICTIM (X11 $VICTIM_WID), sibling X11 $SIBLING_WID"

# @lat: [[test#Test Harness#Visual E2E Tests#Window lifecycle over the wire#Claude focus-mode restore repairs activation]]
# Claude Code 2.1.228 disables focus reporting while blurred, stops reading,
# then restores it immediately after asking X11 to reactivate the original
# window. GPUI can miss that activation callback during the fullscreen repaint;
# the 100 ms EWMH poll must repair the lifecycle and put both the gained frame
# and the second focus-in on their real paths.
wait_until 15 test -f "$FOCUS_PROBE_PREFIX.blur-O" \
    || fail "PHASE 5a: blurring the original window produced no ESC[O"
wait_until 15 test -f "$FOCUS_PROBE_PREFIX.suspended" \
    || fail "PHASE 5a: the fake Claude process never suspended focus reporting"
SUSPEND_HEX=1b5b3c751b5b3e346d1b5b3f31303439681b5b3f313030346c1b5b3f323030346c1b5b3f323033316c1b5b306d1b5b3f3235681b5b324a1b5b48
wait_until 15 server_output_contains "$PROBE_SESSION" "$SUSPEND_HEX" \
    || fail "PHASE 5a: the Claude suspend sequence never crossed the server wire"
GAIN_BEFORE=$(count_client FocusChanged "gained=$PROBE_SESSION" "lost=null")
xdotool windowactivate "$SIBLING_WID" 2>/dev/null \
    || xdotool windowfocus "$SIBLING_WID" 2>/dev/null || true
: >"$FOCUS_PROBE_PREFIX.restore"
wait_until 15 window_is_active "$SIBLING_WID" \
    || fail "PHASE 5a: EWMH never reactivated the original Scribe XID"
wait_for_frames client "$GAIN_BEFORE" 15 FocusChanged \
    "gained=$PROBE_SESSION" "lost=null" \
    || fail "PHASE 5a: EWMH reactivation put no gained FocusChanged on the wire"
wait_until 15 test -f "$FOCUS_PROBE_PREFIX.second-I" \
    || fail "PHASE 5a: restored focus reporting received no second ESC[I"
wait_until 15 test -f "$FOCUS_PROBE_PREFIX.done" \
    || fail "PHASE 5a: the fake Claude process did not exit cleanly"
echo "PHASE 5a PASS: EWMH repaired activation and restored focus reporting received ESC[I"

focus_window "$VICTIM_WID"
shot /output/05a-two-windows.png

CLOSE_BEFORE=$(count_client CloseWindow "window_id=$VICTIM")
CLOSED_BEFORE=$(count_server WindowClosed "window_id=$VICTIM")
send_keys ctrl+shift+d
shot /output/05-close-dialog-again.png
# Cancel -> Quit Scribe -> Kill Window.
send_keys Tab
send_keys Tab
shot /output/06-kill-window-focused.png
send_keys Return
wait_for_frames client "$CLOSE_BEFORE" 15 CloseWindow "window_id=$VICTIM" \
    || fail "PHASE 5: Kill Window put no CloseWindow for $VICTIM on the wire"
wait_for_frames server "$CLOSED_BEFORE" 15 WindowClosed "window_id=$VICTIM" \
    || fail "PHASE 5: the server never acknowledged with WindowClosed"
wait_until 20 window_is_gone "$VICTIM_WID" \
    || fail "PHASE 5: the killed window's frame never went away"
sleep 1.0
pgrep -f 'scribe-client' >/dev/null 2>&1 \
    || fail "PHASE 5: killing one window ended the process hosting the other one"
[ "$(scribe_windows)" = "$SIBLING_WID" ] \
    || fail "PHASE 5: killing one window closed its sibling instead of leaving it up"
[ "$(count_client CloseWindow "window_id=$WIN")" -eq 0 ] \
    || fail "PHASE 5: the sibling window was closed too"
shot /output/05b-sibling-survived.png
echo "PHASE 5 PASS: Kill Window destroyed only its own window; the sibling stayed up"

# ── Phase 6: killing the last window ends the process ─────────────
focus_window "$SIBLING_WID"
CLOSE_BEFORE=$(count_client CloseWindow "window_id=$WIN")
CLOSED_BEFORE=$(count_server WindowClosed "window_id=$WIN")
send_keys ctrl+shift+d
send_keys Tab
send_keys Tab
send_keys Return
wait_for_frames client "$CLOSE_BEFORE" 15 CloseWindow "window_id=$WIN" \
    || fail "PHASE 6: Kill Window put no CloseWindow for $WIN on the wire"
wait_for_frames server "$CLOSED_BEFORE" 15 WindowClosed "window_id=$WIN" \
    || fail "PHASE 6: the server never acknowledged the last window's close"
wait_for_client_exit 20 || fail "PHASE 6: the last window's close left the client running"
echo "PHASE 6 PASS: killing the last window ended the process"

echo ""
echo "PASS: visual window-lifecycle test"
echo "  Inspect screenshots in test-output/:"
echo "    00-attached.png            — the adopted pane before any action"
echo "    01-refocused.png           — the window after a blur/focus round trip"
echo "    02-second-tab.png          — the second tab that moved pane focus"
echo "    03-one-terminal-left.png   — one terminal exit kept the window open"
echo "    03a-before-close.png       — the window just before the close request"
echo "    03-close-dialog.png        — WM close raised the in-app dialog"
echo "    04-quit-focused.png        — Tab moved focus onto Quit Scribe"
echo "    05a-two-windows.png        — a second window open in the same process"
echo "    05-close-dialog-again.png  — the close chord raised it on a fresh window"
echo "    06-kill-window-focused.png — Tab twice landed on Kill Window"
echo "    05b-sibling-survived.png   — the sibling window after the kill"
echo "  Wire record: test-output/share-wire.jsonl"
