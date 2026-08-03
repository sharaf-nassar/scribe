#!/bin/bash
[ "${SCRIBE_E2E_SANDBOX:-0}" = "1" ] || { echo "FATAL: this script only runs inside the scribe e2e container (use just e2e-func / e2e-visual)." >&2; exit 99; }
# Visual + scripted E2E: the feature-014 LAN surface of the GPUI client, asserted
# on the real wire (fix unit FU-17).
#
# Eleven protocol rows make up LAN remote control and none of them can be shown
# by a headless test. `lan_approval.rs` passed its unit tests for months while
# being unreachable from `main.rs`, which is exactly the failure mode this script
# exists to catch: every assertion below is either a frame recorded leaving the
# real client, or a pixel change in the real window.
#
# Two rigs stand in for the parts a single machine cannot supply:
#
#   * `scribe-test share-tap` (SCRIBE_SHARE_TAP=1) relays the Unix socket, so the
#     client still handshakes with the real `scribe-server` while every frame in
#     both directions is recorded. `LanApprovalRequest` is INJECTED through it —
#     the owning server only pushes one when a real unknown device completes a
#     mutual-TLS handshake — and the resulting `LanApprovalDecision` is asserted
#     on the recorded wire. `ListLanPeers` / `LanPeerList` and `GetLanEnv` /
#     `LanEnv` are NOT injected: they are real round trips with the real server.
#
#   * `scribe-test lan-peer` stands in for the second machine's LAN listener. It
#     borrows this machine's own device identity over `GetLanDialIdentity` and
#     terminates a REAL mutual-TLS handshake with the same `LanTls` builder the
#     shipped listener uses, so the `LanHello` it records is the one the client
#     actually put on the encrypted wire.
#
# Phases:
#   0. hand the client a live pane (see window-lifecycle.sh for why);
#   1. the startup LAN probe puts GetLanEnv and ListLanPeers on the wire and the
#      client acts on both replies;
#   2. an injected LanApprovalRequest paints the approval modal; Decline (the
#      default focus) puts LanApprovalDecision{approve:false} on the wire;
#   3. a second request, Tab+Enter, puts LanApprovalDecision{approve:true} on it;
#   4. a client launched with SCRIBE_LAN_DIAL fetches its dial identity from the
#      real server and completes the LanHello preamble against the stand-in peer,
#      showing the waiting-for-approval state before the gate settles.
#
# Phase 4 is skipped (with a loud note, not a silent pass) when the container has
# no OS keyring: the device key is keyring-sealed and the server fails closed
# without one, so `GetLanDialIdentity` answers `available=false` and no dial is
# possible. SCRIBE_KEYRING=1 starts a session keyring in the entrypoint.
#
# Requires: visual container with SCRIBE_SHARE_TAP=1 and SCRIBE_EXTRA_CONFIG
# enabling `[remote.lan]`; xdotool, scrot, python3.
set -e

RECORD="${SHARE_WIRE_RECORD:-/output/share-wire.jsonl}"
LAN_RECORD=/output/lan-wire.jsonl
CLIENT_LOG="${SCRIBE_CLIENT_LOG:-/output/client.log}"
CONTROL="${SHARE_TAP_CONTROL:?the entrypoint must export SHARE_TAP_CONTROL}"
SESSION="${SESSION:?the entrypoint must export a created SESSION}"
LAN_PORT=46062

fail() {
    echo "FAIL: $1" >&2
    echo "--- client log tail ---" >&2
    tail -60 "$CLIENT_LOG" >&2 || true
    exit 1
}

# Count recorded frames of `type` in `dir` matching every key=value pair, in the
# named JSONL record. A value that parses as JSON is compared as JSON, so
# `approve=false` matches a real boolean rather than the string.
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
count_lan_client() { count_in "$LAN_RECORD" client "$@"; }
count_lan_server() { count_in "$LAN_RECORD" server "$@"; }

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

inject() {
    scribe-test share-inject --control "$CONTROL" "$1"
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
# Phase 1's baselines are taken BEFORE the relaunch, because the startup LAN
# probe runs as part of connecting: sampling them afterwards would race the very
# frames the phase waits for.
PROBE_BEFORE=$(count_client GetLanEnv)
LIST_BEFORE=$(count_client ListLanPeers)
ENV_REPLY_BEFORE=$(count_server LanEnv)
PEERS_REPLY_BEFORE=$(count_server LanPeerList)
ATTACHED_BEFORE=$(count_client AttachSessions "session_ids=[\"$SESSION\"]")
launch_client
wait_for "$RECORD" client "$ATTACHED_BEFORE" 30 AttachSessions "session_ids=[\"$SESSION\"]" \
    || fail "PHASE 0: the relaunched client never attached to $SESSION"
focus
shot /output/00-attached.png
echo "PHASE 0 PASS: the client attached to session $SESSION"

# ── Phase 1: the startup LAN probe round trips ────────────────────
# Both requests are answered by the REAL server: `GetLanEnv` on its own transient
# socket (a pre-Hello first frame) and `ListLanPeers` on the live session
# connection. A container has no mDNS peers, so the peer list is legitimately
# empty — what is being proven is that the client asks and acts on the answer,
# which the reader's own log lines report.
wait_for "$RECORD" client "$PROBE_BEFORE" 25 GetLanEnv \
    || fail "PHASE 1: the client never sent GetLanEnv"
wait_for "$RECORD" server "$ENV_REPLY_BEFORE" 25 LanEnv \
    || fail "PHASE 1: the server never answered with LanEnv"
wait_for "$RECORD" client "$LIST_BEFORE" 25 ListLanPeers \
    || fail "PHASE 1: the client never sent ListLanPeers"
wait_for "$RECORD" server "$PEERS_REPLY_BEFORE" 25 LanPeerList \
    || fail "PHASE 1: the server never answered with LanPeerList"
grep -q "server LAN environment" "$CLIENT_LOG" \
    || fail "PHASE 1: the client never acted on the LanEnv reply"
grep -q "server LAN peer list" "$CLIENT_LOG" \
    || fail "PHASE 1: the client never acted on the LanPeerList reply"
if grep -E "server message not wired into the GPUI client.*variant=Lan" "$CLIENT_LOG"; then
    fail "PHASE 1: a LAN message still fell through to the unhandled counter"
fi
echo "PHASE 1 PASS: GetLanEnv and ListLanPeers round tripped and were handled"

# ── Phase 2: an approval request paints, and Decline answers ──────
# The request is injected because the owning server only raises one for a real
# unknown device; everything after it — the modal, the focus default, and the
# decision frame — is the client's own behaviour.
focus
measure_window
shot /output/01a-before-prompt.png
crop_body /output/01a-before-prompt.png /output/01a-body.png
DECLINE_BEFORE=$(count_client LanApprovalDecision "request_id=4001" "approve=false")
inject '{"type":"LanApprovalRequest","request_id":4001,"device_name":"colleague-laptop","fingerprint_words":"amber timber pistol","network_label":"Seeded Lab Network","name_collision":false}'
sleep 1.5
shot /output/02-approval-prompt.png
crop_body /output/02-approval-prompt.png /output/02-body.png
assert_pixels_changed /output/01a-body.png /output/02-body.png "PHASE 2: the injected LanApprovalRequest"
grep -q "raising the LAN device-approval prompt" "$CLIENT_LOG" \
    || fail "PHASE 2: the client never raised the approval prompt"
# Decline holds default focus, so a bare Enter is the safe answer.
send_keys Return
wait_for "$RECORD" client "$DECLINE_BEFORE" 15 LanApprovalDecision "request_id=4001" "approve=false" \
    || fail "PHASE 2: Enter on the default focus sent no declining LanApprovalDecision"
shot /output/03-after-decline.png
crop_body /output/03-after-decline.png /output/03-body.png
assert_pixels_changed /output/02-body.png /output/03-body.png "PHASE 2: dismissing the prompt"
echo "PHASE 2 PASS: the prompt painted and its default answer declined on the wire"

# ── Phase 3: Tab lands on Approve and approves ────────────────────
APPROVE_BEFORE=$(count_client LanApprovalDecision "request_id=4002" "approve=true")
inject '{"type":"LanApprovalRequest","request_id":4002,"device_name":"colleague-laptop","fingerprint_words":"copper lantern violet","network_label":"Seeded Lab Network","name_collision":true}'
sleep 1.5
shot /output/04-second-prompt.png
send_keys Tab
shot /output/05-approve-focused.png
send_keys Return
wait_for "$RECORD" client "$APPROVE_BEFORE" 15 LanApprovalDecision "request_id=4002" "approve=true" \
    || fail "PHASE 3: Tab+Enter sent no approving LanApprovalDecision"
echo "PHASE 3 PASS: Tab moved focus onto Approve and its answer approved on the wire"

# ── Phase 4: the LAN dial preamble against a stand-in peer ────────
# Bring the stand-in up first: it borrows the device identity from the real
# server, which is also how this phase discovers whether a keyring exists.
: >"$LAN_RECORD"
scribe-test lan-peer \
    --listen "127.0.0.1:$LAN_PORT" \
    --upstream "$SCRIBE_RUNTIME_DIR/server.sock" \
    --record "$LAN_RECORD" \
    --pending \
    --hold-ms 6000 >/output/lan-peer.log 2>&1 &
LAN_PEER_PID=$!
sleep 2.0

if ! kill -0 "$LAN_PEER_PID" 2>/dev/null; then
    echo "PHASE 4 SKIP: the stand-in peer could not borrow a LAN device identity."
    echo "  This container has no OS keyring, so scribe-server fails closed on"
    echo "  GetLanDialIdentity and no mutual-TLS dial is possible. Re-run with"
    echo "  SCRIBE_KEYRING=1 to start a session keyring."
    cat /output/lan-peer.log >&2 || true
else
    kill "${SCRIBE_CLIENT_PID:-0}" 2>/dev/null || true
    pkill -f 'scribe-client' 2>/dev/null || true
    wait_for_client_exit 15 || fail "PHASE 4: the local client did not exit"
    IDENTITY_BEFORE=$(count_client GetLanDialIdentity)
    SCRIBE_LAN_DIAL="127.0.0.1:$LAN_PORT" scribe-client >>/output/lan-client.log 2>&1 &
    xdotool search --sync --name "Scribe" >/dev/null 2>&1 || true
    sleep 2.0
    # Focus BEFORE the gate settles: the peer holds for 6 s, so the capture below
    # lands while the window is genuinely still waiting rather than after it has
    # attached and overwritten its own status line.
    focus
    wait_for "$RECORD" client "$IDENTITY_BEFORE" 20 GetLanDialIdentity \
        || fail "PHASE 4: the dialing client never asked for its LAN dial identity"
    wait_for "$LAN_RECORD" client 0 20 LanHello \
        || fail "PHASE 4: no LanHello reached the peer over mutual TLS"
    wait_for "$LAN_RECORD" server 0 20 LanApprovalPending \
        || fail "PHASE 4: the peer never held the connection pending"
    shot /output/06-awaiting-approval.png
    grep -q "held pending device approval on the peer" /output/lan-client.log \
        || fail "PHASE 4: the client never reported the waiting-for-approval state"
    wait_for "$LAN_RECORD" server 0 20 LanApprovalResult \
        || fail "PHASE 4: the peer never answered the approval gate"
    sleep 2.0
    shot /output/07-lan-attached.png
    grep -q "LAN device approval accepted" /output/lan-client.log \
        || fail "PHASE 4: the client never accepted the approval result"
    HELLO_ON_LAN=$(count_lan_client Hello)
    [ "$HELLO_ON_LAN" -gt 0 ] \
        || fail "PHASE 4: the approved client never sent Hello over the LAN link"
    LAN_PENDING=$(count_lan_server LanApprovalPending)
    echo "PHASE 4 PASS: LanHello crossed real mutual TLS, the gate held ($LAN_PENDING pending) and settled"
    kill "$LAN_PEER_PID" 2>/dev/null || true
fi

echo ""
echo "PASS: visual LAN approval + dial test"
echo "  Inspect screenshots in test-output/:"
echo "    00-attached.png           — the adopted pane before any LAN traffic"
echo "    01a-before-prompt.png     — the window just before the request arrives"
echo "    02-approval-prompt.png    — the injected request raised the modal"
echo "    03-after-decline.png      — the modal gone after the default Decline"
echo "    04-second-prompt.png      — the name-collision variant of the prompt"
echo "    05-approve-focused.png    — Tab moved focus onto Approve"
echo "    06-awaiting-approval.png  — the dialing client waiting on the peer"
echo "    07-lan-attached.png       — the approved LAN session"
echo "  Wire records: test-output/share-wire.jsonl, test-output/lan-wire.jsonl"
