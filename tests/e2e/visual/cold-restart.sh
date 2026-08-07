#!/bin/bash
[ "${SCRIBE_E2E_SANDBOX:-0}" = "1" ] || { echo "FATAL: this script only runs inside the scribe e2e container (use just e2e-func / e2e-visual)." >&2; exit 99; }
# Scripted E2E: the GPUI client's cold-restart restore, driven against the real
# client process.
#
# This is the oracle for spec 016's "Cold-restart restore (`RestoreStore`,
# `--restore-child` fan-out) restores windows, workspaces, tabs, and pane trees"
# and for "window geometry persistence". Neither can be shown by a headless
# test and neither can be shown by a *daemon* stand-in: the daemon is a plain
# protocol client with no window, no layout and no restore store, so a test that
# stops and starts it proves only that the SERVER keeps sessions alive (which is
# what tests/e2e/func/cold-restart.sh still covers). Everything asserted below
# is produced by `scribe-client` itself.
#
# A cold restart is the crash case, so the test reproduces it literally:
#   * SIGKILL the client, so nothing on the exit path can tidy up after it —
#     an orderly quit deliberately *deletes* the snapshot;
#   * restart the disposable test server, so every PTY really is gone and the
#     relaunched client meets an EMPTY `SessionList`, which is the one condition
#     under which a snapshot may be replayed.
#
# Phases:
#   0. hand the client a live pane, then split it so the snapshot has a real
#      pane tree to rebuild;
#   1. the restore store and the window geometry record appear on disk;
#   2. a resize is persisted into the geometry record;
#   3. crash the client and cold-restart the server;
#   4. the relaunched client claims the snapshot and replays it: one
#      `CreateSession` per saved pane, each carrying its `env_envelope_id`;
#   5. the relaunched window came back at the persisted geometry, and both
#      restored panes are on screen.
#
# The wire tap is deliberately NOT used: it renames the server socket out from
# under `scribe-test server stop/start`, and this test has to restart the real
# server. Every assertion below therefore reads the two process logs — the
# client's for what it claimed, replayed and requested, the server's for the
# PTYs it actually spawned in answer.
#
# Requires: visual container (no share tap); xdotool, scrot, python3.
set -e

CLIENT_LOG="${SCRIBE_CLIENT_LOG:-/output/client.log}"
SERVER_LOG="${SCRIBE_SERVER_LOG:-/output/server.log}"
SESSION="${SESSION:?the entrypoint must export a created SESSION}"
STATE_DIR="${XDG_STATE_HOME:?the entrypoint must export XDG_STATE_HOME}/scribe"
RESTORE_DIR="$STATE_DIR/restore"
GEOMETRY_DIR="$STATE_DIR/windows"

# Wider and shorter than the 1920x1080 screen's default window, and far enough
# from it that a restored window cannot match by accident.
RESIZE_W=1280
RESIZE_H=720

fail() {
    echo "FAIL: $1" >&2
    echo "--- client log tail ---" >&2
    tail -60 "$CLIENT_LOG" >&2 || true
    echo "--- restore store ---" >&2
    ls -la "$RESTORE_DIR" "$RESTORE_DIR/windows" "$GEOMETRY_DIR" >&2 2>/dev/null || true
    exit 1
}

count_log() { grep -c "$1" "$CLIENT_LOG" 2>/dev/null || true; }

# The client log is written with tracing's ANSI styling on, which puts escape
# sequences between a field's name and its value — so any assertion that reads a
# numeric field has to strip them first.
plain_log() { sed 's/\x1b\[[0-9;]*m//g' "$CLIENT_LOG"; }

# The value of `field=` on the last log line matching `pattern`.
last_log_field() {
    plain_log | grep "$1" | tail -1 | sed -n "s/.*$2=\([0-9]*\).*/\1/p"
}

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

count_server_log() { grep -c "$1" "$SERVER_LOG" 2>/dev/null || true; }

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

# Geometry is one file per window, and the entrypoint's first client left one
# behind, so the record under test has to be addressed by the live window's id —
# which is exactly the id the restore snapshot is filed under.
LIVE_WINDOW=""

geometry_file() {
    printf '%s/%s.toml' "$GEOMETRY_DIR" "$LIVE_WINDOW"
}

geometry_field() {
    local file
    file=$(geometry_file)
    [ -f "$file" ] || return 1
    sed -n "s/^$1 = \\(.*\\)$/\\1/p" "$file" | head -1
}

# ── Phase 0: a live pane, then a real pane tree to save ───────────
# Same preamble as window-lifecycle.sh: the entrypoint creates $SESSION through
# `scribe-test` after launching the client, and the server hides another
# window's sessions, so the running client never learns it exists. Releasing the
# daemon's ownership and relaunching gets the client a pane over the ordinary
# `ListSessions` path.
sleep 1.0
# The entrypoint's client is CLOSED, not killed. A killed client leaves its own
# login shell running in a window the server keeps, and the relaunch below now
# reopens every such window — two windows and two snapshots, so every assertion
# here would be ambiguous about which one it means. "Kill Window" (ctrl+shift+d,
# then Tab twice off the safe Cancel default onto the second button) destroys it
# on the server, leaving only the daemon's released session to adopt.
focus
send_keys ctrl+shift+d
send_keys Tab
send_keys Tab
send_keys Return
wait_for_client_exit 20 || fail "PHASE 0: the original client did not close"
scribe-test daemon stop >/dev/null 2>&1 || true
sleep 1.0
launch_client
focus
SPLITS_BEFORE=$(count_log "split the focused pane")
send_keys ctrl+shift+backslash
wait_for_log_growth "split the focused pane" "$SPLITS_BEFORE" 15 \
    || fail "PHASE 0: ctrl+shift+backslash never split the pane"
# The split's session has to actually land in the new pane, or the snapshot
# would prune it and the replay would come back with one pane instead of two.
wait_for_log_growth "pane adopted a session" 1 20 \
    || fail "PHASE 0: the split pane never adopted a session"
shot /output/00-two-panes.png
echo "PHASE 0 PASS: the client owns a split window with two live panes"

# ── Phase 1: the snapshot and the geometry record reach disk ──────
# Both are debounced, so give the tick time to flush before looking.
sleep 2.5
[ -f "$RESTORE_DIR/index.toml" ] || fail "PHASE 1: no restore index was written"
SNAPSHOTS=$(find "$RESTORE_DIR/windows" -maxdepth 1 -name '*.toml' 2>/dev/null | wc -l)
[ "$SNAPSHOTS" -eq 1 ] || fail "PHASE 1: expected exactly one window snapshot, found $SNAPSHOTS"
SNAPSHOT_FILE=$(find "$RESTORE_DIR/windows" -maxdepth 1 -name '*.toml' | head -1)
LAUNCHES=$(grep -c '^\[\[launches\]\]' "$SNAPSHOT_FILE" || true)
[ "$LAUNCHES" -eq 2 ] \
    || fail "PHASE 1: the snapshot recorded $LAUNCHES launches, expected 2 (one per pane)"
grep -q '^\[\[workspaces\]\]' "$SNAPSHOT_FILE" \
    || fail "PHASE 1: the snapshot recorded no workspace"
LIVE_WINDOW=$(basename "$SNAPSHOT_FILE" .toml)
[ -f "$(geometry_file)" ] \
    || fail "PHASE 1: no geometry record was written for window $LIVE_WINDOW"
echo "PHASE 1 PASS: a two-launch snapshot and a geometry record are on disk for $LIVE_WINDOW"

# ── Phase 2: a resize is persisted ────────────────────────────────
WID=$(find_window)
xdotool windowsize "$WID" "$RESIZE_W" "$RESIZE_H"
sleep 3.0
SAVED_W=$(geometry_field width) || fail "PHASE 2: the geometry record vanished"
SAVED_H=$(geometry_field height)
# The WM's frame accounting can shift the reported size by a pixel or two, so
# the assertion is a tolerance rather than equality — the point is that the
# record followed the resize instead of keeping the startup size.
[ "$SAVED_W" -ge $(( RESIZE_W - 8 )) ] && [ "$SAVED_W" -le $(( RESIZE_W + 8 )) ] \
    || fail "PHASE 2: geometry width is $SAVED_W, expected about $RESIZE_W"
[ "$SAVED_H" -ge $(( RESIZE_H - 8 )) ] && [ "$SAVED_H" -le $(( RESIZE_H + 8 )) ] \
    || fail "PHASE 2: geometry height is $SAVED_H, expected about $RESIZE_H"
shot /output/01-resized.png
echo "PHASE 2 PASS: the geometry record followed the resize (${SAVED_W}x${SAVED_H})"

# ── Phase 3: crash the client, cold-restart the server ────────────
# SIGKILL, not the close dialog: an orderly quit clears the snapshot on purpose,
# and the whole point of a restore store is the case where nothing got to run.
# The server is then genuinely restarted, so both PTYs die with it — that empty
# `SessionList` is the only condition under which a snapshot may be replayed.
PTYS_BEFORE=$(count_server_log "created new PTY session")
pkill -KILL -f 'scribe-client' || true
wait_for_client_exit 15 || fail "PHASE 3: the client survived SIGKILL"
[ -f "$RESTORE_DIR/index.toml" ] || fail "PHASE 3: the crash took the restore index with it"
scribe-test server stop
# `scribe-test server start` returns as soon as the socket *file* exists, and a
# stopped server leaves its socket behind — so readiness is gated on the process
# instead. Without this the replacement loses the lock race, refuses to start,
# and the "successful" start is talking to a corpse.
started=$(date +%s)
while pgrep -x scribe-server >/dev/null 2>&1; do
    if [ $(( "$(date +%s)" - started )) -ge 15 ]; then
        fail "PHASE 3: the server process outlived the stop"
    fi
    sleep 0.25
done
scribe-test server start
pgrep -x scribe-server >/dev/null 2>&1 || fail "PHASE 3: no replacement server came up"
grep -q "another scribe-server is already running" "$SERVER_LOG" \
    && fail "PHASE 3: the replacement server refused to start"
echo "PHASE 3 PASS: the client was killed and the server cold-restarted"

# ── Phase 4: the relaunched client claims and replays ─────────────
CLAIMS_BEFORE=$(count_log "claimed a cold-restart snapshot")
REPLAYS_BEFORE=$(count_log "replaying a cold-restart snapshot")
REQUESTS_BEFORE=$(count_log "requested a restored session")
launch_client
wait_for_log_growth "claimed a cold-restart snapshot" "$CLAIMS_BEFORE" 20 \
    || fail "PHASE 4: the relaunched client never claimed the snapshot"
wait_for_log_growth "replaying a cold-restart snapshot" "$REPLAYS_BEFORE" 25 \
    || fail "PHASE 4: the claimed snapshot was never replayed"
REPLAY_PANES=$(last_log_field "replaying a cold-restart snapshot" panes)
[ "$REPLAY_PANES" = "2" ] \
    || fail "PHASE 4: the replay rebuilt $REPLAY_PANES panes, expected 2"
started=$(date +%s)
while [ "$(count_log "requested a restored session")" -lt $(( REQUESTS_BEFORE + 2 )) ]; do
    if [ $(( "$(date +%s)" - started )) -ge 25 ]; then
        fail "PHASE 4: the replay requested fewer than two restored sessions"
    fi
    sleep 0.3
done
# The server's own account of the same event: two brand-new PTYs after the
# restart, which is the replay actually reaching it rather than the client
# merely logging its intent.
started=$(date +%s)
while [ "$(count_server_log "created new PTY session")" -lt $(( PTYS_BEFORE + 2 )) ]; do
    if [ $(( "$(date +%s)" - started )) -ge 25 ]; then
        fail "PHASE 4: the cold-restarted server spawned fewer than two PTYs"
    fi
    sleep 0.3
done
echo "PHASE 4 PASS: the snapshot was claimed and both saved panes were relaunched"

# ── Phase 5: geometry and pane tree came back ─────────────────────
focus
eval "$(xdotool getwindowgeometry --shell "$(find_window)")"
[ "$WIDTH" -ge $(( RESIZE_W - 8 )) ] && [ "$WIDTH" -le $(( RESIZE_W + 8 )) ] \
    || fail "PHASE 5: the restored window is ${WIDTH}px wide, expected about $RESIZE_W"
[ "$HEIGHT" -ge $(( RESIZE_H - 8 )) ] && [ "$HEIGHT" -le $(( RESIZE_H + 8 )) ] \
    || fail "PHASE 5: the restored window is ${HEIGHT}px tall, expected about $RESIZE_H"
wait_for_log_growth "cold-restart replay filled every restored pane" 0 20 \
    || fail "PHASE 5: not every restored pane adopted a session"
# A vertical split gives two equal halves, so the two restored sessions must
# have been requested at the same, less-than-full width. A replay that lost the
# pane tree would have asked for one full-width session instead.
REQUESTED_COLS=$(plain_log | grep "requested a restored session" \
    | sed -n 's/.*cols=\([0-9]*\).*/\1/p' | tail -2)
LEFT_COLS=$(echo "$REQUESTED_COLS" | head -1)
RIGHT_COLS=$(echo "$REQUESTED_COLS" | tail -1)
[ -n "$LEFT_COLS" ] && [ "$LEFT_COLS" = "$RIGHT_COLS" ] \
    || fail "PHASE 5: restored panes asked for '$LEFT_COLS' and '$RIGHT_COLS' columns"
# The very first grid size this run published belongs to the pre-split,
# full-width pane, so a restored half must be strictly narrower than it.
FULL_COLS=$(plain_log | grep "published a pane's grid size" | head -1 \
    | sed -n 's/.*cols=\([0-9]*\).*/\1/p')
[ "$LEFT_COLS" -lt "$FULL_COLS" ] \
    || fail "PHASE 5: each restored pane asked for $LEFT_COLS of $FULL_COLS columns"
shot /output/02-restored.png
echo "PHASE 5 PASS: the window reopened at ${WIDTH}x${HEIGHT} with both panes restored"

echo ""
echo "PASS: visual cold-restart restore test"
echo "  Inspect screenshots in test-output/:"
echo "    00-two-panes.png — the split window whose layout was saved"
echo "    01-resized.png   — the window at the geometry that had to persist"
echo "    02-restored.png  — the window rebuilt from the snapshot after the crash"
echo "  Logs: test-output/client.log, test-output/server.log"
