#!/bin/bash
[ "${SCRIBE_E2E_SANDBOX:-0}" = "1" ] || { echo "FATAL: this script only runs inside the scribe e2e container (use just e2e-func / e2e-visual)." >&2; exit 99; }
# Scripted E2E: quitting Scribe with several windows open and relaunching brings
# every one of them back, and a new window opened afterwards is empty.
#
# This is the WARM restart — the one users actually perform. The server outlives
# the client and keeps every session, so nothing here is a cold restart: no
# snapshot is replayed, and each window comes back by claiming the window id the
# server still holds its sessions under. `Welcome` hands one window per
# connection plus `other_windows` for the rest, so a client that ignores the
# remainder can only ever bring one window back — and the windows it left behind
# then get handed to the next connection that asks for one, which is what made a
# deliberate "new window" open somebody's old window instead.
#
# Phases:
#   0. one client window over the daemon's released session;
#   1. Ctrl+Shift+N gives the server a second window with its own session;
#   2. quit through the close dialog — both windows exit, and their snapshots
#      survive because a quit ends the client, not the sessions;
#   3. relaunch: exactly two windows come back, one adopted and one reopened
#      from `other_windows`, with no cold-restart replay;
#   4. Ctrl+Shift+N on the restored client opens an EMPTY window — it spawns a
#      new PTY rather than adopting a window the server was holding.
#
# Requires: visual container; xdotool, scrot.
set -e

CLIENT_LOG="${SCRIBE_CLIENT_LOG:-/output/client.log}"
SERVER_LOG="${SCRIBE_SERVER_LOG:-/output/server.log}"
STATE_DIR="${XDG_STATE_HOME:?the entrypoint must export XDG_STATE_HOME}/scribe"
RESTORE_DIR="$STATE_DIR/restore"

fail() {
    echo "FAIL: $1" >&2
    echo "--- client log tail ---" >&2
    tail -80 "$CLIENT_LOG" >&2 || true
    echo "--- restore store ---" >&2
    ls -la "$RESTORE_DIR/windows" >&2 2>/dev/null || true
    exit 1
}

count_log() { grep -c "$1" "$CLIENT_LOG" 2>/dev/null || true; }
count_server_log() { grep -c "$1" "$SERVER_LOG" 2>/dev/null || true; }

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

# Every mapped Scribe window, newest last.
list_windows() {
    xdotool search --onlyvisible --class '[Ss]cribe' 2>/dev/null \
        || xdotool search --onlyvisible --name '[Ss]cribe' 2>/dev/null || true
}
count_windows() { list_windows | grep -c . || true; }

# Wait until the mapped window count settles on `want`, so an assertion never
# fires between two windows appearing.
wait_for_windows() {
    local want="$1" timeout_secs="$2" started seen
    started=$(date +%s)
    while :; do
        seen=$(count_windows)
        [ "$seen" -eq "$want" ] && { sleep 1.0; [ "$(count_windows)" -eq "$want" ] && return 0; }
        if [ $(( "$(date +%s)" - started )) -ge "$timeout_secs" ]; then
            echo "  (window count is $seen, wanted $want)" >&2
            return 1
        fi
        sleep 0.5
    done
}

focus() {
    local wid
    wid=$(list_windows | tail -1)
    [ -n "$wid" ] || fail "no Scribe window found"
    xdotool windowactivate --sync "$wid" 2>/dev/null \
        || xdotool windowfocus --sync "$wid" 2>/dev/null || true
    # Past the X11 focus guard's 300 ms reactivation debounce.
    sleep 0.8
}

shot() {
    sleep 0.3
    scrot -o "$1"
    echo "captured $1"
}

send_keys() {
    xdotool key --clearmodifiers "$@"
    sleep 0.5
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

snapshot_count() {
    find "$RESTORE_DIR/windows" -maxdepth 1 -name '*.toml' 2>/dev/null | wc -l
}

# ── Phase 0: one client window over the daemon's session ──────────
# Same preamble as cold-restart.sh: the entrypoint's daemon owns $SESSION in a
# window of its own, so releasing it and relaunching leaves exactly one window
# with sessions for the client to adopt over the ordinary `ListSessions` path.
sleep 1.0
# CLOSED, not killed: a killed client leaves its own login shell running in a
# window the server keeps, and the relaunch would rightly reopen that window too
# — which is the very behaviour under test and would make the counts below mean
# nothing. "Kill Window" (ctrl+shift+d, then Tab twice off the safe Cancel
# default) destroys it on the server, so exactly one window is left to adopt.
focus
send_keys ctrl+shift+d
send_keys Tab
send_keys Tab
send_keys Return
wait_for_client_exit 20 || fail "PHASE 0: the entrypoint's client did not close"
scribe-test daemon stop >/dev/null 2>&1 || true
sleep 1.0
launch_client
wait_for_windows 1 20 || fail "PHASE 0: expected exactly one Scribe window"
focus
shot /output/00-one-window.png
echo "PHASE 0 PASS: one client window"

# ── Phase 1: a second window with its own session ─────────────────
NEW_WINDOWS_BEFORE=$(count_log "opened a new terminal window")
PTYS_BEFORE=$(count_server_log "created new PTY session")
send_keys ctrl+shift+n
wait_for_log_growth "opened a new terminal window" "$NEW_WINDOWS_BEFORE" 20 \
    || fail "PHASE 1: ctrl+shift+n never opened a window"
wait_for_windows 2 25 || fail "PHASE 1: the second window never mapped"
# The second window has to own a session, or the server would not count it as a
# window worth restoring and phase 3 would pass for the wrong reason.
for _ in $(seq 1 40); do
    [ "$(count_server_log 'created new PTY session')" -gt "$PTYS_BEFORE" ] && break
    sleep 0.5
done
[ "$(count_server_log 'created new PTY session')" -gt "$PTYS_BEFORE" ] \
    || fail "PHASE 1: the second window never got a session of its own"
# Both snapshots must reach disk before the quit, on the 500 ms debounce.
sleep 3.0
SNAPSHOTS=$(snapshot_count)
[ "$SNAPSHOTS" -eq 2 ] || fail "PHASE 1: expected two window snapshots, found $SNAPSHOTS"
shot /output/01-two-windows.png
echo "PHASE 1 PASS: two windows, each with its own session and snapshot"

# ── Phase 2: quit through the close dialog ────────────────────────
# Ctrl+Shift+D raises the dialog, Tab moves focus off the safe Cancel default
# onto "Quit Scribe", Enter sends `QuitAll`. The server keeps every session —
# that is what the button promises — so this is a client exit, not a shutdown.
focus
send_keys ctrl+shift+d
shot /output/02-close-dialog.png
send_keys Tab
send_keys Return
wait_for_client_exit 25 || fail "PHASE 2: the client never exited after Quit Scribe"
# A quit is not a "these panes should not come back": the sessions are still
# running, and the snapshots are how the next launch knows which windows to
# reclaim them into.
SNAPSHOTS=$(snapshot_count)
[ "$SNAPSHOTS" -eq 2 ] \
    || fail "PHASE 2: the quit left $SNAPSHOTS snapshots, expected both to survive"
echo "PHASE 2 PASS: quit exited both windows and kept both snapshots"

# ── Phase 3: relaunch brings BOTH windows back ────────────────────
REOPENED_BEFORE=$(count_log "reopened a window the server kept sessions for")
REPLAYS_BEFORE=$(count_log "replaying a cold-restart snapshot")
launch_client
wait_for_windows 2 30 || fail "PHASE 3: the relaunch did not bring both windows back"
wait_for_log_growth "reopened a window the server kept sessions for" "$REOPENED_BEFORE" 20 \
    || fail "PHASE 3: the second window was never reopened from other_windows"
REOPENED=$(( $(count_log "reopened a window the server kept sessions for") - REOPENED_BEFORE ))
[ "$REOPENED" -eq 1 ] \
    || fail "PHASE 3: reopened $REOPENED windows, expected exactly 1 beside the adopted one"
# The server kept every session, so nothing may be replayed on top of them —
# a replay here would double every pane.
[ "$(count_log 'replaying a cold-restart snapshot')" -eq "$REPLAYS_BEFORE" ] \
    || fail "PHASE 3: a cold-restart replay ran against a server that kept its sessions"
[ "$(count_log 'skipping the cold-restart replay')" -ge 1 ] \
    || fail "PHASE 3: the restored window never recognised the server as warm"
shot /output/03-restored.png
echo "PHASE 3 PASS: both windows came back, one adopted and one reopened, no replay"

# ── Phase 4: a new window is EMPTY, not somebody's old one ────────
# Every window the server holds is now connected, so there is none left to hand
# out — but the assertion that matters is the one that fails when a new window
# sends the restart claim: it would adopt an existing window and spawn no PTY.
NEW_WINDOWS_BEFORE=$(count_log "opened a new terminal window")
PTYS_BEFORE=$(count_server_log "created new PTY session")
focus
send_keys ctrl+shift+n
wait_for_log_growth "opened a new terminal window" "$NEW_WINDOWS_BEFORE" 20 \
    || fail "PHASE 4: ctrl+shift+n never opened a window"
wait_for_windows 3 25 || fail "PHASE 4: the third window never mapped"
for _ in $(seq 1 40); do
    [ "$(count_server_log 'created new PTY session')" -gt "$PTYS_BEFORE" ] && break
    sleep 0.5
done
[ "$(count_server_log 'created new PTY session')" -gt "$PTYS_BEFORE" ] \
    || fail "PHASE 4: the new window adopted an existing window instead of starting empty"
shot /output/04-new-window.png
echo "PHASE 4 PASS: the new window started empty with a PTY of its own"

# ── Phase 5: a cold restore relaunches Pi fresh, never resumed ────────────
# Pi is the one AI provider with no resume: `new_pi_tab` is its only launch
# action, and a cold restart has to bring the tab back as a brand new tracked
# Pi session rather than reaching for a conversation id. The `pi` stub records
# every invocation's argv, so "fresh" is asserted as an empty argv block on a
# record written after the restart — a resume would have to put something in it.
PI_RECORD=/tmp/pi-invocation.txt

wait_for_pi_record() {
    local timeout_secs="$1" started
    started=$(date +%s)
    while [ ! -f "$PI_RECORD" ]; do
        if [ $(( "$(date +%s)" - started )) -ge "$timeout_secs" ]; then
            return 1
        fi
        sleep 0.3
    done
    sleep 0.3
    return 0
}

assert_fresh_pi_launch() {
    [ "$(head -1 "$PI_RECORD")" = "--ENV--" ] \
        || fail "$1: pi was launched with argv of its own: $(head -3 "$PI_RECORD" | tr '\n' ' ')"
    grep -q '^SCRIBE_SESSION_ID=.' "$PI_RECORD" \
        || fail "$1: the Pi tab is not a tracked Scribe session"
}

rm -f "$PI_RECORD"
focus
send_keys ctrl+alt+z
wait_for_pi_record 20 || fail "PHASE 5: ctrl+alt+z never launched the pi stub"
assert_fresh_pi_launch "PHASE 5"
PI_SESSION_BEFORE=$(sed -n 's/^SCRIBE_SESSION_ID=//p' "$PI_RECORD" | head -1)
# The snapshot debounce has to flush the Pi launch before the client dies.
sleep 3.0
shot /output/05-pi-tab.png

# SIGKILL, not the close dialog: an orderly quit clears the snapshots, and a
# cold restore is exactly the case where nothing got to run. The server is then
# genuinely restarted, so every PTY dies with it and the empty `SessionList` is
# what admits the replay.
REPLAYS_BEFORE=$(count_log "replaying a cold-restart snapshot")
pkill -KILL -f 'scribe-client' || true
wait_for_client_exit 20 || fail "PHASE 5: the client survived SIGKILL"
rm -f "$PI_RECORD"
scribe-test server stop
started=$(date +%s)
while pgrep -x scribe-server >/dev/null 2>&1; do
    if [ $(( "$(date +%s)" - started )) -ge 20 ]; then
        fail "PHASE 5: the server process outlived the stop"
    fi
    sleep 0.25
done
scribe-test server start
pgrep -x scribe-server >/dev/null 2>&1 || fail "PHASE 5: no replacement server came up"
launch_client
wait_for_log_growth "replaying a cold-restart snapshot" "$REPLAYS_BEFORE" 30 \
    || fail "PHASE 5: the relaunch never replayed a cold-restart snapshot"
wait_for_pi_record 30 || fail "PHASE 5: the cold restore never relaunched pi"
assert_fresh_pi_launch "PHASE 5"
PI_SESSION_AFTER=$(sed -n 's/^SCRIBE_SESSION_ID=//p' "$PI_RECORD" | head -1)
[ -n "$PI_SESSION_AFTER" ] && [ "$PI_SESSION_AFTER" != "$PI_SESSION_BEFORE" ] \
    || fail "PHASE 5: the restored Pi tab reused the pre-restart session instead of starting fresh"
shot /output/05-pi-restored.png
echo "PHASE 5 PASS: the cold restore relaunched Pi as a fresh tracked session with no resume argv"

echo ""
echo "PASS: visual multi-window-restore test"
echo "  Inspect screenshots in test-output/:"
echo "    00-one-window.png    — the single adopted window"
echo "    01-two-windows.png   — after Ctrl+Shift+N"
echo "    02-close-dialog.png  — the close dialog before Quit Scribe"
echo "    03-restored.png      — both windows back after the relaunch"
echo "    04-new-window.png    — a third, empty window"
echo "    05-pi-tab.png        — the Pi tab opened by ctrl+alt+z"
echo "    05-pi-restored.png   — the same tab after a cold restore"
