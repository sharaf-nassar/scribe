#!/bin/bash
[ "${SCRIBE_E2E_SANDBOX:-0}" = "1" ] || { echo "FATAL: this script only runs inside the scribe e2e container (use just e2e-func / e2e-visual)." >&2; exit 99; }
# Visual + scripted E2E: the feature-013 tailnet surface of the GPUI client,
# asserted on the real wire (fix unit FU-16).
#
# Eleven protocol rows make up tailnet remote control and none of them can be
# shown by a headless test. `remote_handshake.rs` and `lost_control.rs` passed
# their unit tests for months while sitting outside `main.rs`'s import closure,
# which is exactly the failure mode this script exists to catch: every assertion
# below is either a frame recorded leaving the real client, or a pixel change in
# the real window.
#
# Two rigs stand in for the parts a single machine cannot supply:
#
#   * `scribe-test share-tap` (SCRIBE_SHARE_TAP=1) relays the Unix socket, so the
#     client still handshakes with the real `scribe-server` while every frame in
#     both directions is recorded. `WindowTakenOver`, `RemoteDisconnect` and the
#     viewer `ShareRoster` are INJECTED through it — the owning server only
#     produces them for a real second machine — and the client's answers are
#     asserted on the recorded wire. `GetRemoteEnv` / `RemoteEnv` and
#     `ListRemotePeers` / `RemotePeerList` are NOT injected: they are real round
#     trips with the real server, which answers the fail-closed
#     `tailscale_detected = false` a container legitimately has.
#
#   * `scribe-test remote-peer` stands in for the second machine's tailnet
#     listener. It refuses any first frame but `RemoteHandshake`, records the one
#     the client actually sent, answers the mandatory `RemoteHandshakeReply`, and
#     splices an accepted connection to the real local server.
#
# Phases:
#   0. hand the client a live pane (see window-lifecycle.sh for why);
#   1. the startup remote probe puts GetRemoteEnv and ListRemotePeers on the wire
#      and the client acts on both replies;
#   2. an injected WindowTakenOver freezes the window under the displaced banner,
#      suppresses input, and Enter reclaims with ControlClaim on the wire;
#   3. an injected RemoteDisconnect names the typed reason on the status strip;
#   4. a viewer's palette row leaves as DispatchAction and comes back as
#      ActionDispatched, and an injected RunAction is executed by the window;
#   5. a client launched with SCRIBE_REMOTE_DIAL puts a real RemoteHandshake on
#      the stand-in peer's TCP wire and acts on its reply.
#
# Requires: visual container with SCRIBE_SHARE_TAP=1 and SCRIBE_EXTRA_CONFIG
# enabling `[remote]`; xdotool, scrot, python3, imagemagick.
set -e

RECORD="${SHARE_WIRE_RECORD:-/output/share-wire.jsonl}"
PEER_RECORD=/output/remote-wire.jsonl
CLIENT_LOG="${SCRIBE_CLIENT_LOG:-/output/client.log}"
CONTROL="${SHARE_TAP_CONTROL:?the entrypoint must export SHARE_TAP_CONTROL}"
SESSION="${SESSION:?the entrypoint must export a created SESSION}"
REMOTE_PORT=46061

fail() {
    echo "FAIL: $1" >&2
    echo "--- client log tail ---" >&2
    tail -60 "$CLIENT_LOG" >&2 || true
    exit 1
}

# Count recorded frames of `type` in `dir` matching every key=value pair, in the
# named JSONL record. A value that parses as JSON is compared as JSON, so
# `takeover=false` matches a real boolean rather than the string.
count_in() {
    python3 - "$@" <<'PY'
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

count_client() { count_in "$RECORD" client "$@"; }
count_server() { count_in "$RECORD" server "$@"; }
count_peer_client() { count_in "$PEER_RECORD" client "$@"; }
count_peer_server() { count_in "$PEER_RECORD" server "$@"; }

# Wait until a matcher's count in `record`/`dir` exceeds `baseline`.
wait_for() {
    local record="$1" direction="$2" baseline="$3" timeout_secs="$4"
    shift 4
    local started now
    started=$(date +%s)
    while true; do
        now=$(count_in "$record" "$direction" "$@")
        if [ "$now" -gt "$baseline" ]; then
            return 0
        fi
        if [ $(( "$(date +%s)" - started )) -ge "$timeout_secs" ]; then
            return 1
        fi
        sleep 0.3
    done
}

count_log() { grep -c "$1" "$CLIENT_LOG" 2>/dev/null || true; }

wait_for_log_growth() {
    local pattern="$1" baseline="$2" timeout_secs="$3" started
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

inject() {
    scribe-test share-inject --control "$CONTROL" "$1"
}

# The window id and own participant id the server handed this client in its
# Welcome. Both are server-assigned, so neither can be guessed.
welcome_field() {
    python3 - "$RECORD" "$1" <<'PY'
import json, sys
value = None
with open(sys.argv[1]) as fh:
    for line in fh:
        try:
            row = json.loads(line)
        except ValueError:
            continue
        msg = row.get("message", {})
        if row.get("dir") == "server" and msg.get("type") == "Welcome":
            value = msg[sys.argv[2]]
if value is None:
    sys.exit(1)
print(value)
PY
}

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

type_text() {
    xdotool type --clearmodifiers --delay 40 "$1"
    sleep 0.5
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

# Crop to the window minus its bottom band: the status bar's sparklines resample
# every 2 s and would move pixels on their own.
crop_body() {
    convert "$1" -crop "${WIN_W}x$(( WIN_H - 60 ))+${WIN_X}+${WIN_Y}" +repage "$2"
}

assert_pixels_changed() {
    local before="$1" after="$2" what="$3"
    local diff
    diff=$(compare -metric AE "$before" "$after" null: 2>&1 || true)
    [ "${diff%% *}" != "0" ] || fail "$what painted nothing"
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

# ── Phase 0: hand the client a live pane to act in ────────────────
sleep 1.0
kill "${SCRIBE_CLIENT_PID:-0}" 2>/dev/null || true
wait_for_client_exit 15 || fail "PHASE 0: the original client did not exit"
scribe-test daemon stop >/dev/null 2>&1 || true
sleep 1.0
# Phase 1's baselines are taken BEFORE the relaunch, because the startup remote
# probe runs as part of connecting: sampling them afterwards would race the very
# frames the phase waits for.
ENV_BEFORE=$(count_client GetRemoteEnv)
LIST_BEFORE=$(count_client ListRemotePeers)
ENV_REPLY_BEFORE=$(count_server RemoteEnv)
PEERS_REPLY_BEFORE=$(count_server RemotePeerList)
ATTACHED_BEFORE=$(count_client AttachSessions "session_ids=[\"$SESSION\"]")
launch_client
wait_for "$RECORD" client "$ATTACHED_BEFORE" 30 AttachSessions "session_ids=[\"$SESSION\"]" \
    || fail "PHASE 0: the relaunched client never attached to $SESSION"
focus
shot /output/00-attached.png
echo "PHASE 0 PASS: the client attached to session $SESSION"

# ── Phase 1: the startup remote probe round trips ─────────────────
# Both requests are answered by the REAL server: `GetRemoteEnv` on its own
# transient socket (a pre-Hello first frame) and `ListRemotePeers` on the live
# session connection. A container has no tailnet, so the environment is
# legitimately the fail-closed `tailscale_detected = false` and the peer list is
# empty — what is being proven is that the client asks and acts on the answer,
# which the reader's own log lines report.
wait_for "$RECORD" client "$ENV_BEFORE" 25 GetRemoteEnv \
    || fail "PHASE 1: the client never sent GetRemoteEnv"
wait_for "$RECORD" server "$ENV_REPLY_BEFORE" 25 RemoteEnv \
    || fail "PHASE 1: the server never answered with RemoteEnv"
wait_for "$RECORD" client "$LIST_BEFORE" 25 ListRemotePeers \
    || fail "PHASE 1: the client never sent ListRemotePeers"
wait_for "$RECORD" server "$PEERS_REPLY_BEFORE" 25 RemotePeerList \
    || fail "PHASE 1: the server never answered with RemotePeerList"
grep -q "server tailnet environment" "$CLIENT_LOG" \
    || fail "PHASE 1: the client never acted on the RemoteEnv reply"
grep -q "server tailnet peer list" "$CLIENT_LOG" \
    || fail "PHASE 1: the client never acted on the RemotePeerList reply"
if grep -E "server message not wired into the GPUI client.*variant=Remote" "$CLIENT_LOG"; then
    fail "PHASE 1: a remote message still fell through to the unhandled counter"
fi
echo "PHASE 1 PASS: GetRemoteEnv and ListRemotePeers round tripped and were handled"

WIN=$(welcome_field window_id)
SELF=$(welcome_field participant_id)
PEER=$((SELF + 1))
echo "client window id: $WIN (own participant id $SELF, injected peer $PEER)"

# ── Phase 1b: the real-window remote picker dials a selected peer ──────────
# The local server has no tailnet inside this container, so inject one discovery
# reply only after the palette has opened the picker. The selected address points
# at the real TCP stand-in below: its handshake and WindowList flow are recorded.
: >"$PEER_RECORD"
scribe-test remote-peer \
    --listen "127.0.0.1:$REMOTE_PORT" \
    --upstream "$SCRIBE_RUNTIME_DIR/server-upstream.sock" \
    --record "$PEER_RECORD" \
    --hold-ms 300 >/output/remote-peer.log 2>&1 &
REMOTE_PEER_PID=$!
sleep 1.0
kill -0 "$REMOTE_PEER_PID" 2>/dev/null || fail "PHASE 1b: the stand-in peer did not start"

focus
measure_window
shot /output/01b-before-picker.png
crop_body /output/01b-before-picker.png /output/01b-before-picker-body.png
send_keys ctrl+shift+p
type_text "Connect to remote"
send_keys Return
inject '{"type":"RemotePeerList","peers":[{"name":"picker-peer","addr":"127.0.0.1","online":true}]}'
sleep 1.0
shot /output/01c-picker-peers.png
crop_body /output/01c-picker-peers.png /output/01c-picker-peers-body.png
assert_pixels_changed /output/01b-before-picker-body.png /output/01c-picker-peers-body.png \
    "PHASE 1b: remote picker with discovered peer"
grep -q "opened remote-connect picker" "$CLIENT_LOG" \
    || fail "PHASE 1b: palette did not open the remote picker"

PICKER_DIAL_BEFORE=$(count_peer_client RemoteHandshake)
send_keys Return
wait_for "$PEER_RECORD" client "$PICKER_DIAL_BEFORE" 20 RemoteHandshake \
    || fail "PHASE 1b: selecting the peer sent no RemoteHandshake"
wait_for "$PEER_RECORD" client 0 20 ListWindows \
    || fail "PHASE 1b: picker probe sent no ListWindows"
wait_for "$PEER_RECORD" server 0 20 WindowList \
    || fail "PHASE 1b: picker probe received no WindowList"
shot /output/01d-picker-windows.png
# Escape returns from the window list to peers; the second Escape closes the
# picker so later command-palette input cannot select a probed remote window.
send_keys Escape
send_keys Escape
echo "PHASE 1b PASS: picker opened, rendered a peer, and dialed its window list"

# ── Phase 2: the displaced banner freezes the window ──────────────
# The notice is injected because the owning server only raises one for a real
# second controller; everything after it — the banner, the input freeze, and the
# reclaim frame — is the client's own behaviour.
focus
measure_window
shot /output/01a-before-takeover.png
crop_body /output/01a-before-takeover.png /output/01a-body.png
KEYS_BEFORE=$(count_client KeyInput)
CLAIM_BEFORE=$(count_client ControlClaim "window_id=\"$WIN\"")
inject '{"type":"WindowTakenOver","device_name":"desk-mini","login_name":"alex@example.com"}'
sleep 1.5
shot /output/02-taken-over.png
crop_body /output/02-taken-over.png /output/02-body.png
assert_pixels_changed /output/01a-body.png /output/02-body.png \
    "PHASE 2: the injected WindowTakenOver"
grep -q "window taken over by another controller" "$CLIENT_LOG" \
    || fail "PHASE 2: the client never recorded the takeover"
# A frozen window suppresses ALL input: a plain letter must not reach the PTY.
send_keys a
if [ "$(count_client KeyInput)" != "$KEYS_BEFORE" ]; then
    fail "PHASE 2: a keystroke reached the PTY while the window was frozen"
fi
shot /output/03-frozen-input-suppressed.png
# Enter is the one affordance the banner offers.
send_keys Return
wait_for "$RECORD" client "$CLAIM_BEFORE" 15 ControlClaim "window_id=\"$WIN\"" \
    || fail "PHASE 2: Enter on the displaced banner sent no ControlClaim"
sleep 1.0
shot /output/04-after-reclaim.png
crop_body /output/04-after-reclaim.png /output/04-body.png
assert_pixels_changed /output/02-body.png /output/04-body.png "PHASE 2: reclaiming the window"
echo "PHASE 2 PASS: the banner froze the window and Enter reclaimed it on the wire"

# ── Phase 3: a severed link names its typed reason ────────────────
inject '{"type":"RemoteDisconnect","reason":"disabled"}'
sleep 1.5
shot /output/05-remote-disconnect.png
crop_body /output/05-remote-disconnect.png /output/05-body.png
assert_pixels_changed /output/04-body.png /output/05-body.png \
    "PHASE 3: the injected RemoteDisconnect"
grep -q "remote peer severed the connection" "$CLIENT_LOG" \
    || fail "PHASE 3: the client never recorded the severed connection"
echo "PHASE 3 PASS: RemoteDisconnect surfaced its typed reason"

# ── Phase 4: the automation round trip ────────────────────────────
# A viewer's window-mutating palette row cannot run locally — the server refuses
# a CreateSession from a non-controller — so it goes out as DispatchAction and
# the server answers ActionDispatched. Making this client a viewer needs a
# roster naming a remote holder, which only a second machine produces.
roster=$(cat <<JSON
{"type":"ShareRoster","window_id":"$WIN","mode":"shared_single_typist","holder":$PEER,
 "participants":[
   {"participant_id":$SELF,"device_name":"this machine","login_name":"","is_local":true,"is_holder":false},
   {"participant_id":$PEER,"device_name":"desk-mini","login_name":"alex","is_local":false,"is_holder":true}
 ]}
JSON
)
inject "$(printf '%s' "$roster" | tr -d '\n')"
sleep 1.0
DISPATCH_BEFORE=$(count_client DispatchAction)
ACK_BEFORE=$(count_server ActionDispatched)
focus
send_keys ctrl+shift+p
type_text "New Tab"
shot /output/06-viewer-palette.png
send_keys Return
wait_for "$RECORD" client "$DISPATCH_BEFORE" 15 DispatchAction \
    || fail "PHASE 4: a viewer's palette row put no DispatchAction on the wire"
wait_for "$RECORD" server "$ACK_BEFORE" 15 ActionDispatched \
    || fail "PHASE 4: the server never acknowledged the dispatch"
grep -q "automation action routed by the server" "$CLIENT_LOG" \
    || fail "PHASE 4: the client never acted on the ActionDispatched ack"

# The other half: an inbound RunAction has to be executed by the window, which
# only a real round trip through the foreground's tick can do.
TABS_BEFORE=$(count_log "opened a new tab")
inject '{"type":"RunAction","action":{"type":"new_tab"}}'
wait_for_log_growth "opened a new tab" "$TABS_BEFORE" 20 \
    || fail "PHASE 4: an injected RunAction created no session"
sleep 1.0
shot /output/07-run-action-tab.png
echo "PHASE 4 PASS: DispatchAction/ActionDispatched round tripped and RunAction ran"

# ── Phase 5: the tailnet dial preamble against the same stand-in peer ───────
kill "${SCRIBE_CLIENT_PID:-0}" 2>/dev/null || true
pkill -f 'scribe-client' 2>/dev/null || true
wait_for_client_exit 15 || fail "PHASE 5: the local client did not exit"
SCRIBE_REMOTE_DIAL="127.0.0.1:$REMOTE_PORT" scribe-client >/output/remote-client.log 2>&1 &
xdotool search --sync --name "Scribe" >/dev/null 2>&1 || true
sleep 2.0
focus
wait_for "$PEER_RECORD" client 0 25 RemoteHandshake \
    || fail "PHASE 5: no RemoteHandshake reached the peer over TCP"
wait_for "$PEER_RECORD" server 0 25 RemoteHandshakeReply "accepted=true" \
    || fail "PHASE 5: the peer never accepted the handshake"
grep -q "remote handshake accepted" /output/remote-client.log \
    || fail "PHASE 5: the client never acted on the RemoteHandshakeReply"
HELLO_ON_TAILNET=$(count_peer_client Hello)
[ "$HELLO_ON_TAILNET" -gt 0 ] \
    || fail "PHASE 5: the accepted client never sent Hello over the tailnet link"
sleep 1.5
shot /output/08-tailnet-attached.png
kill "$REMOTE_PEER_PID" 2>/dev/null || true
echo "PHASE 5 PASS: RemoteHandshake crossed a real TCP link and its reply was accepted"

echo ""
echo "PASS: visual remote-control test"
echo "  Inspect screenshots in test-output/:"
echo "    00-attached.png                 — the adopted pane before any remote traffic"
echo "    01a-before-takeover.png         — the window just before the notice arrives"
echo "    02-taken-over.png               — the displaced banner over the frozen grid"
echo "    03-frozen-input-suppressed.png  — a keystroke that reached nothing"
echo "    04-after-reclaim.png            — the window after the one-action reclaim"
echo "    05-remote-disconnect.png        — the typed severance reason on the strip"
echo "    06-viewer-palette.png           — a viewer's palette filtered to 'New Tab'"
echo "    07-run-action-tab.png           — the tab an injected RunAction opened"
echo "    08-tailnet-attached.png         — the session reached over the tailnet dial"
echo "    01c-picker-peers.png           — the remote picker with a discovered peer"
echo "    01d-picker-windows.png         — the selected peer's real WindowList"
echo "  Wire records: test-output/share-wire.jsonl, test-output/remote-wire.jsonl"
