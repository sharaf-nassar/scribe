#!/bin/bash
# e2e-timeout: 180
[ "${SCRIBE_E2E_SANDBOX:-0}" = "1" ] || {
    echo "FATAL: this script only runs inside the scribe e2e container (use just e2e-func / e2e-visual)." >&2
    exit 99
}
# Scripted E2E: the Tabs-and-windows chords reach their actions.
#
# Two parity rows were unreachable for the whole rebuild because the shell
# hard-coded overlay chords on top of the Linux defaults: Ctrl+Shift+Q
# (`close_tab`) opened the close-dialog demo, and Ctrl+Shift+N (`new_window`)
# opened the since-removed notes modal. Both keystrokes were claimed by
# `handle_overlay_key` and never reached `handle_binding`, so the actions could
# only be run from the command palette or after a rebind.
#
# A headless test can prove the *precedence rule* (it does — see the
# keybindings suite), but only the running client can prove that the chord a
# user actually presses now lands on the action. So each phase drives the real
# window through XTEST and asserts an effect the action alone can produce:
#
#   * Ctrl+Shift+Q makes the client log "closing the active tab" — a line only
#     `close_active_tab` writes and only `LayoutAction::CloseTab` reaches — and
#     the server answer it, logging "session closed by client";
#   * Ctrl+Shift+N makes the client log "opened a new terminal window", adds a
#     second mapped X11 window, and makes the server register a second window
#     through a fresh `Hello`; a dialog or a swallowed chord does none of it;
#   * Ctrl+Shift+D still opens the close dialog on its relocated chord, so
#     moving the overlay off `close_tab`'s default did not strand the surface;
#   * Ctrl+Alt+Z opens exactly one Pi tab, and the `pi` stub it runs records a
#     plain tab's startup — the rc file every other tab reads, the integration
#     marker, the `PATH` that rc exported, the focused pane's CWD, and no
#     leftover restore-delta file — with no argv of its own. Ctrl+C then ends
#     the stub and the server finalizes that exact session, which is what
#     `exec pi` buys: quitting Pi closes its tab rather than dropping the user
#     at a stray prompt.
#
# Phase 0 is the same session-adoption dance `overlay-actions.sh` documents:
# the entrypoint creates $SESSION after the client launched, so the running
# client never hears about it, and only a relaunch after the test daemon
# releases ownership picks it up through `ListSessions`.
#
# Input is driven through XTEST (plain `xdotool key`, no `--window`). GPUI
# reads the keyboard through XInput2 and ignores the synthetic events
# `xdotool --window` sends with XSendEvent, so window-targeted input would
# leave the client untouched while the script still "passed".
#
# Requires: visual container (see docker/entrypoint-visual.sh), which exports
# SESSION, SCRIBE_CLIENT_PID and SCRIBE_CLIENT_LOG.
set -e

CLIENT_LOG="${SCRIBE_CLIENT_LOG:-/output/client.log}"
SERVER_LOG="${SCRIBE_SERVER_LOG:-/output/server.log}"

# Differing pixels a modal must add to the frame. The dialog dims the whole
# backdrop and draws a box over the grid, which is far more than this; a
# swallowed chord leaves consecutive frames byte-identical (animations are off).
DIALOG_DIFF_MIN="${DIALOG_DIFF_MIN:-20000}"

GRID_X=0
GRID_Y=0
GRID_W=0
GRID_H=0

# The Pi stand-in on /tests/bin writes exactly this file (tests/e2e/bin/pi).
PI_RECORD=/tmp/pi-invocation.txt
# The directory the Pi tab must start in. Deliberately not $HOME, so the
# server's own home fallback cannot be mistaken for the focused pane's CWD.
PI_CWD=/tmp/scribe-pi-tab-cwd
PI_CWD_PROBE=/tmp/scribe-pi-cwd-settled
PI_PROBE=/tmp/scribe-pi-probe.sh
# Exported by the rc file below, so a Pi tab that skipped normal shell startup
# records no marker at all.
PI_RC_MARKER=PI_STARTUP_ORDER=bashrc

# Every mapped Scribe window, newest last.
list_windows() {
    xdotool search --class '[Ss]cribe' 2>/dev/null || xdotool search --name '[Ss]cribe' 2>/dev/null || true
}

count_windows() {
    list_windows | grep -c . || true
}

find_window() {
    list_windows | tail -1
}

# Focus the client and cache its on-screen geometry so a full-screen capture
# can be cropped down to the window under test.
focus() {
    local wid
    wid=$(find_window)
    if [ -z "$wid" ]; then
        echo "FAIL: no Scribe window found"
        exit 1
    fi
    xdotool windowactivate --sync "$wid" 2>/dev/null ||
        xdotool windowfocus --sync "$wid" 2>/dev/null || true
    sleep 0.3
    eval "$(xdotool getwindowgeometry --shell "$wid")"
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

# Count lit pixels in the client window of a full-screen capture. Rendered text
# is near-white on a near-black background, so a plain luminance threshold
# separates ink from the pane cleanly.
grid_ink() {
    local value
    value=$(convert "$1" -crop "${GRID_W}x${GRID_H}+${GRID_X}+${GRID_Y}" +repage \
        -colorspace Gray -threshold 35% -format "%[fx:mean*w*h]" info:)
    printf '%s' "${value%.*}"
}

# Count differing pixels between two captures, cropped to the client window.
window_diff() {
    local value
    value=$(compare -metric AE \
        \( "$1" -crop "${GRID_W}x${GRID_H}+${GRID_X}+${GRID_Y}" +repage \) \
        \( "$2" -crop "${GRID_W}x${GRID_H}+${GRID_X}+${GRID_Y}" +repage \) \
        null: 2>&1 || true)
    printf '%s' "${value%%.*}"
}

send_keys() {
    xdotool key --clearmodifiers "$@"
    sleep 0.4
}

# Count matching lines in a log (0 when the log does not exist yet).
count_in() {
    grep -acF "$2" "$1" 2>/dev/null || true
}

count_log() {
    count_in "$CLIENT_LOG" "$1"
}

count_server_log() {
    count_in "$SERVER_LOG" "$1"
}

# Wait until a log holds more copies of a pattern than `baseline`.
wait_for_log_growth_in() {
    local file="$1" pattern="$2" baseline="$3" timeout_secs="${4:-15}" started now
    started=$(date +%s)
    while true; do
        now=$(count_in "$file" "$pattern")
        if [ "$now" -gt "$baseline" ]; then
            return 0
        fi
        if [ $(("$(date +%s)" - started)) -ge "$timeout_secs" ]; then
            return 1
        fi
        sleep 0.3
    done
}

wait_for_log_growth() {
    wait_for_log_growth_in "$CLIENT_LOG" "$1" "$2" "${3:-15}"
}

wait_for_server_log_growth() {
    wait_for_log_growth_in "$SERVER_LOG" "$1" "$2" "${3:-15}"
}

fail() {
    echo "$1"
    echo "--- client log tail ---"
    tail -40 "$CLIENT_LOG" || true
    echo "--- server log tail ---"
    tail -20 "$SERVER_LOG" || true
    exit 1
}

# ── Phase 0: hand the client a live pane to act in ────────────────
sleep 1.0
# CLOSED, not killed. A killed client leaves its own login shell running in a
# window the server keeps, and the relaunch below reopens every window the
# server still holds sessions for — so a kill here would leave two windows and
# break the count this phase establishes. "Kill Window" (ctrl+shift+d, then Tab
# twice off the safe Cancel default) destroys it on the server instead.
focus
send_keys ctrl+shift+d
send_keys Tab
send_keys Tab
send_keys Return
for _ in $(seq 1 80); do
    pgrep -f 'scribe-client' >/dev/null 2>&1 || break
    sleep 0.25
done
if pgrep -f 'scribe-client' >/dev/null 2>&1; then
    fail "PHASE 0 FAIL: the original client did not close"
fi
scribe-test daemon stop >/dev/null 2>&1 || true
sleep 1.0
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
WINDOWS_BEFORE=$(count_windows)
if [ "$WINDOWS_BEFORE" -ne 1 ]; then
    fail "PHASE 0 FAIL: expected exactly one Scribe window, found $WINDOWS_BEFORE"
fi
echo "PHASE 0 PASS: client attached to session $SESSION (grid ink $BASE_INK, 1 window)"

# ── Phase 1: Ctrl+Shift+Q reaches close_tab ───────────────────────
# The close dialog used to own this chord. "closing the active tab" is written
# only by `close_active_tab`, which only `LayoutAction::CloseTab` reaches, so
# the line appearing means the chord got past the overlay layer.
# The window has to survive the close. Phase 0 leaves exactly ONE tab, and
# `on_session_exited` sends CloseWindow the moment the strip empties ("existed
# && tabs_empty"), so the server acks, the client exits, and phase 2's focus()
# finds no window at all. Open a second tab first: closing the active one then
# leaves the strip non-empty and the window alive, which is what phases 2-4
# need to keep driving. This is the test's own setup gap, not a product bug.
TABS_BEFORE=$(count_log "opened a new tab")
focus
send_keys ctrl+shift+t
if ! wait_for_log_growth "opened a new tab" "$TABS_BEFORE" 15; then
    fail "PHASE 1 FAIL: ctrl+shift+t did not open the second tab the close needs"
fi

CLOSES_BEFORE=$(count_log "closing the active tab")
SERVER_CLOSES_BEFORE=$(count_server_log "session closed by client")
focus
send_keys ctrl+shift+q
if ! wait_for_log_growth "closing the active tab" "$CLOSES_BEFORE" 15; then
    fail "PHASE 1 FAIL: ctrl+shift+q never reached close_tab (still shadowed?)"
fi
# The client's own line proves the chord dispatched; the server's proves the
# CloseSession it sent actually crossed the wire and killed the session.
if ! wait_for_server_log_growth "session closed by client" "$SERVER_CLOSES_BEFORE" 15; then
    fail "PHASE 1 FAIL: the server never saw a CloseSession for the active tab"
fi
shot /output/01-close-tab.png
echo "PHASE 1 PASS: ctrl+shift+q closed the tab end to end (client chord, server close)"

# ── Phase 2: Ctrl+Shift+N opens a second window ───────────────────
# A since-removed modal used to own this chord, and `NewWindow` had no
# handler at all. Both the log line and a second mapped X11 window are asserted:
# the log proves the action ran, the window count proves it really opened one.
WINDOWS_LOG_BEFORE=$(count_log "opened a new terminal window")
HELLOS_BEFORE=$(count_server_log "client identified via Hello")
focus
send_keys ctrl+shift+n
if ! wait_for_log_growth "opened a new terminal window" "$WINDOWS_LOG_BEFORE" 15; then
    fail "PHASE 2 FAIL: ctrl+shift+n never reached new_window (still shadowed?)"
fi
# The second window is a separate client to the server: its own connection, its
# own Hello, and therefore its own window id and sessions.
if ! wait_for_server_log_growth "client identified via Hello" "$HELLOS_BEFORE" 15; then
    fail "PHASE 2 FAIL: the new window opened no connection of its own"
fi
WINDOWS_AFTER=0
for _ in $(seq 1 40); do
    WINDOWS_AFTER=$(count_windows)
    [ "$WINDOWS_AFTER" -gt "$WINDOWS_BEFORE" ] && break
    sleep 0.5
done
if [ "$WINDOWS_AFTER" -le "$WINDOWS_BEFORE" ]; then
    fail "PHASE 2 FAIL: new_window mapped no second window (still $WINDOWS_AFTER)"
fi
shot /output/02-new-window.png
echo "PHASE 2 PASS: ctrl+shift+n opened a second window ($WINDOWS_BEFORE -> $WINDOWS_AFTER)"

# ── Phase 3: the close dialog survives on its new chord ───────────
# Relocating the overlay off close_tab's default must not strand it, so the
# dialog is opened on ctrl+shift+d and asserted as a real repaint of the
# window it opens over.
focus
shot /output/03-before-dialog.png
send_keys ctrl+shift+d
shot /output/04-close-dialog.png
DIFF=$(window_diff /output/03-before-dialog.png /output/04-close-dialog.png)
if [ "${DIFF:-0}" -lt "$DIALOG_DIFF_MIN" ]; then
    fail "PHASE 3 FAIL: ctrl+shift+d changed $DIFF px (min $DIALOG_DIFF_MIN); dialog did not open"
fi
send_keys Escape
echo "PHASE 3 PASS: the close dialog opens on its relocated chord (+$DIFF px)"

# @lat: [[test#Visual E2E Tests#Tab and window chords reach their actions#Ctrl+Alt+Z opens one Pi tab that starts like a plain tab]]
# ── Phase 4: Ctrl+Alt+Z opens exactly one Pi tab ──────────────────
# Pi is launch-only: no provider, no resume, no AI chrome. What it must inherit
# is a plain tab's startup, so the rc file below is the oracle — it is the only
# place `PI_STARTUP_ORDER` and the stub's own `PATH` entry come from, and a Pi
# tab that resolved its shell differently records neither.
rm -f "$PI_RECORD" "$PI_CWD_PROBE"
mkdir -p "$PI_CWD"
# shellcheck disable=SC2016 # $PATH is written literally; the rc file expands it.
printf 'export %s\nexport PATH="/tests/bin:$PATH"\n' "$PI_RC_MARKER" >>"$HOME/.bashrc"

# Give the focused pane a CWD of its own, and report it the way a shell would.
# Sourced rather than typed so the OSC 7 escape survives xdotool verbatim, and
# emitted by hand because this image ships no shell-integration scripts: with
# nothing reporting a CWD the client falls back to the server's $HOME guard,
# which is the very thing this phase has to tell apart from the pane's own
# directory. The trailing marker file is the gate — it cannot be written until
# the `cd` and the OSC 7 before it have run.
cat >"$PI_PROBE" <<'SH'
cd "$1" || exit 1
printf '\033]7;file://%s%s\033\\' "${HOSTNAME}" "$PWD"
printf 'ready\n' > "$2"
SH
focus
xdotool type --clearmodifiers --delay 20 ". $PI_PROBE $PI_CWD $PI_CWD_PROBE"
send_keys Return
for _ in $(seq 1 60); do
    [ -s "$PI_CWD_PROBE" ] && break
    sleep 0.25
done
if [ ! -s "$PI_CWD_PROBE" ]; then
    fail "PHASE 4 FAIL: the focused pane never reached $PI_CWD"
fi
# The OSC 7 is already in the PTY stream by now; this covers the server's
# forward and the client's metadata write.
sleep 1

PI_TABS_BEFORE=$(count_log "opened a new tab")
focus
send_keys ctrl+alt+z
for _ in $(seq 1 60); do
    [ -f "$PI_RECORD" ] && break
    sleep 0.25
done
if [ ! -f "$PI_RECORD" ]; then
    fail "PHASE 4 FAIL: ctrl+alt+z never reached new_pi_tab (no pi invocation)"
fi
# One chord, one tab. A second would mean the chord reached two handlers.
sleep 2
PI_TABS_AFTER=$(count_log "opened a new tab")
if [ "$PI_TABS_AFTER" -ne $((PI_TABS_BEFORE + 1)) ]; then
    fail "PHASE 4 FAIL: ctrl+alt+z opened $((PI_TABS_AFTER - PI_TABS_BEFORE)) tabs, expected 1"
fi

# Pi is launch-only, so the server appends no resume arguments: everything
# before the --ENV-- marker must be empty.
if [ -n "$(awk '/^--ENV--$/ { exit } { print }' "$PI_RECORD")" ]; then
    echo "argv:"
    awk '/^--ENV--$/ { exit } { print "  " $0 }' "$PI_RECORD"
    fail "PHASE 4 FAIL: the pi launch carried argv of its own"
fi
# SCRIBE_SHELL_INTEGRATION is deliberately absent from this list: it only exists
# when the server finds the integration scripts, which this image does not ship.
for expected in "PWD=$PI_CWD" "$PI_RC_MARKER" "TERM_PROGRAM=Scribe"; do
    if ! grep -Fqx "$expected" "$PI_RECORD"; then
        fail "PHASE 4 FAIL: the pi environment is missing '$expected'"
    fi
done
if ! grep -q '^PATH=.*/tests/bin' "$PI_RECORD"; then
    fail "PHASE 4 FAIL: the pi environment did not inherit the PATH its rc exported"
fi
# zsh and fish consume the staged delta in the same `-c` command that execs, so
# a leaked variable here means the temp file was left behind too.
if grep -q '^SCRIBE_RESTORE_ENV_DELTA_FILE=' "$PI_RECORD"; then
    fail "PHASE 4 FAIL: the pi launch leaked SCRIBE_RESTORE_ENV_DELTA_FILE"
fi
shot /output/05-pi-tab.png
echo "PHASE 4 PASS: ctrl+alt+z opened one Pi tab in $PI_CWD with a plain tab's startup"

# @lat: [[test#Visual E2E Tests#Tab and window chords reach their actions#Quitting Pi ends its tab]]
# ── Phase 5: quitting Pi ends its tab ─────────────────────────────
# The shell execs Pi over itself, so Pi is the PTY's direct child and its exit
# is the session's. Assert on that exact session id rather than on a count, so
# an unrelated pane dying cannot pass this phase.
PI_SESSION_UUID=$(sed -n 's/^SCRIBE_SESSION_ID=//p' "$PI_RECORD" | head -1)
if [ -z "$PI_SESSION_UUID" ]; then
    fail "PHASE 5 FAIL: the pi environment carried no SCRIBE_SESSION_ID"
fi
# The server logs ids through SessionId's Display, which is the prefixed short
# form rather than the full UUID the environment carries.
PI_SESSION_LOG_ID="session-$(printf '%s' "$PI_SESSION_UUID" | cut -c1-8)"
if [ "$(count_server_log "$PI_SESSION_LOG_ID")" -eq 0 ]; then
    fail "PHASE 5 FAIL: the server never logged the pi session $PI_SESSION_LOG_ID"
fi
focus
send_keys ctrl+c
EXIT_FOUND=0
for _ in $(seq 1 60); do
    if grep -aF "session exit finalized" "$SERVER_LOG" 2>/dev/null |
        grep -aqF "$PI_SESSION_LOG_ID"; then
        EXIT_FOUND=1
        break
    fi
    sleep 0.25
done
if [ "$EXIT_FOUND" -ne 1 ]; then
    fail "PHASE 5 FAIL: quitting pi did not end session $PI_SESSION_LOG_ID"
fi
shot /output/06-pi-tab-closed.png
echo "PHASE 5 PASS: quitting pi ended session $PI_SESSION_LOG_ID and its tab"

echo ""
echo "PASS: visual tab-window-chords test"
echo "  Inspect screenshots in test-output/:"
echo "    00-attached.png       — the adopted pane before any chord"
echo "    01-close-tab.png      — after ctrl+shift+q closed the tab"
echo "    02-new-window.png     — the second window ctrl+shift+n opened"
echo "    03-before-dialog.png  — the window before the dialog chord"
echo "    04-close-dialog.png   — the close dialog on ctrl+shift+d"
echo "    05-pi-tab.png         — the Pi tab ctrl+alt+z opened"
echo "    06-pi-tab-closed.png  — the window after quitting Pi"
