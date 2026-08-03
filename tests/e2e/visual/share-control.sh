#!/bin/bash
[ "${SCRIBE_E2E_SANDBOX:-0}" = "1" ] || { echo "FATAL: this script only runs inside the scribe e2e container (use just e2e-func / e2e-visual)." >&2; exit 99; }
# Visual + scripted E2E test: the feature-015 window-sharing and control
# claim/grant surface of the GPUI client.
#
# Runs against the real client and the real scribe-server, with the wire tap
# (`scribe-test share-tap`, SCRIBE_SHARE_TAP=1) interposed on the server socket
# so the notices a second machine would have produced can be injected and every
# frame the client sends is recorded for on-the-wire assertions.
#
# Walks the whole surface: the roster/presence panel, the modal grant/deny
# prompt (and the `ControlGrant` it puts on the wire), viewer keystroke
# suppression with the take-control hint, the `ControlClaim` Enter sends, and
# the denied / ended notices.
#
# The client's own window holds no PTY here (it cannot create its first session
# until `CreateWorkspace` lands, FU-6), so keystroke *suppression* is asserted
# from the client log while the control frames themselves are asserted from the
# recorded wire.
#
# Requires: visual container with SCRIBE_SHARE_TAP=1, xdotool, scrot, python3.
set -e

RECORD="${SHARE_WIRE_RECORD:-/output/share-wire.jsonl}"
CONTROL="${SHARE_TAP_CONTROL:?share tap control socket is required}"

find_window() {
    local wid
    wid=$(xdotool search --class '[Ss]cribe' 2>/dev/null | head -1)
    [ -z "$wid" ] && wid=$(xdotool search --name '[Ss]cribe' 2>/dev/null | head -1)
    printf '%s' "$wid"
}

focus() {
    local wid
    wid=$(find_window) || true
    if [ -n "$wid" ]; then
        xdotool windowactivate --sync "$wid" 2>/dev/null \
            || xdotool windowfocus --sync "$wid" 2>/dev/null || true
        sleep 0.3
    fi
}

shot() {
    focus
    sleep 0.2
    scrot "$1"
    echo "captured $1"
}

send_keys() {
    local wid
    wid=$(find_window) || true
    if [ -n "$wid" ]; then
        xdotool key --window "$wid" "$@"
        sleep 0.4
    fi
}

inject() {
    scribe-test share-inject --control "$CONTROL" "$1"
    sleep 0.5
}

# How many keystrokes the client's share surfaces have swallowed so far.
#
# This is a log assertion rather than an absence-of-`KeyInput` assertion because
# the GPUI client does not create a first session of its own — it adopts one the
# server already has — so its window holds no PTY in this rig and would emit no
# `KeyInput` either way. The `KeyInput` check below is kept as a regression
# guard; the log line is the load-bearing evidence.
count_swallowed() {
    grep -c "share surfaces swallowed a keystroke" /output/client.log || true
}

# Count how many recorded client frames carry a message type.
count_client() {
    python3 - "$RECORD" "$1" <<'PY'
import json, sys
path, wanted = sys.argv[1], sys.argv[2]
total = 0
with open(path) as fh:
    for line in fh:
        try:
            row = json.loads(line)
        except ValueError:
            continue
        if row.get("dir") == "client" and row.get("message", {}).get("type") == wanted:
            total += 1
print(total)
PY
}

# Assert that a recorded client frame of `type` matches every key=value pair.
assert_client_frame() {
    python3 - "$RECORD" "$@" <<'PY'
import json, sys
path, wanted = sys.argv[1], sys.argv[2]
def norm(value):
    try:
        return json.loads(value)
    except ValueError:
        return value

pairs = [(k, norm(v)) for k, v in (p.split("=", 1) for p in sys.argv[3:])]
with open(path) as fh:
    for line in fh:
        try:
            row = json.loads(line)
        except ValueError:
            continue
        if row.get("dir") != "client":
            continue
        msg = row.get("message", {})
        if msg.get("type") != wanted:
            continue
        if all(msg.get(k) == v for k, v in pairs):
            print(json.dumps(msg))
            sys.exit(0)
sys.exit(1)
PY
}

# The window id and own participant id the server handed this client in its
# Welcome. The injected roster must name both: the window id so the client's own
# control frames echo it back, and the participant id so the client recognises
# which roster seat is itself (the server assigns it, so it cannot be guessed).
welcome_field() {
    python3 - "$RECORD" "$1" <<'PY'
import json, sys
with open(sys.argv[1]) as fh:
    for line in fh:
        try:
            row = json.loads(line)
        except ValueError:
            continue
        msg = row.get("message", {})
        if row.get("dir") == "server" and msg.get("type") == "Welcome":
            print(msg[sys.argv[2]])
            sys.exit(0)
sys.exit(1)
PY
}

sleep 1.5
WIN=$(welcome_field window_id)
SELF=$(welcome_field participant_id)
PEER=$((SELF + 1))
echo "client window id: $WIN (own participant id $SELF, injected peer $PEER)"

roster() {
    local holder="$1"
    cat <<JSON
{"type":"ShareRoster","window_id":"$WIN","mode":"shared_single_typist","holder":$holder,
 "participants":[
   {"participant_id":$SELF,"device_name":"this machine","login_name":"","is_local":true,"is_holder":$([ "$holder" = "$SELF" ] && echo true || echo false)},
   {"participant_id":$PEER,"device_name":"laptop","login_name":"alex","is_local":false,"is_holder":$([ "$holder" = "$PEER" ] && echo true || echo false)}
 ]}
JSON
}


# ── Phase 1: roster arrives, presence surfaces appear ─────────────
inject "$(roster "$SELF" | tr -d '\n')"
shot /output/01-share-roster.png
echo "PHASE 1 PASS: ShareRoster renders the roster panel and presence badge"

# ── Phase 2: incoming control request is modal, Esc denies ────────
inject "{\"type\":\"ControlRequested\",\"window_id\":\"$WIN\",\"from\":{\"participant_id\":$PEER,\"device_name\":\"laptop\",\"login_name\":\"alex\",\"is_local\":false,\"is_holder\":false}}"
shot /output/02-control-requested.png
SWALLOWED_BEFORE=$(count_swallowed)
send_keys x
if [ "$(count_swallowed)" = "$SWALLOWED_BEFORE" ]; then
    echo "FAIL: a keystroke was not swallowed while the grant prompt was modal" >&2
    exit 1
fi
send_keys Escape
assert_client_frame ControlGrant "participant_id=$PEER" accept=false >/dev/null
echo "PHASE 2 PASS: ControlRequested is modal; Esc puts ControlGrant{accept:false} on the wire"

# ── Phase 3: viewer suppression and the take-control claim ────────
inject "$(roster "$PEER" | tr -d '\n')"
shot /output/03-viewer-roster.png
SWALLOWED_BEFORE=$(count_swallowed)
KEYS_BEFORE=$(count_client KeyInput)
send_keys a
if [ "$(count_swallowed)" = "$SWALLOWED_BEFORE" ]; then
    echo "FAIL: a viewer keystroke was not swallowed" >&2
    exit 1
fi
if [ "$(count_client KeyInput)" != "$KEYS_BEFORE" ]; then
    echo "FAIL: a viewer keystroke reached the PTY" >&2
    exit 1
fi
shot /output/04-control-hint.png
send_keys Return
assert_client_frame ControlClaim "window_id=$WIN" >/dev/null
shot /output/05-claim-requested.png
echo "PHASE 3 PASS: a viewer's keystroke is suppressed and Enter puts ControlClaim on the wire"

# ── Phase 4: the denial notice ────────────────────────────────────
inject "{\"type\":\"ControlDenied\",\"window_id\":\"$WIN\"}"
shot /output/06-control-denied.png
echo "PHASE 4 PASS: ControlDenied surfaces the transient notice"

# ── Phase 5: the share ends ───────────────────────────────────────
inject "{\"type\":\"ShareEnded\",\"window_id\":\"$WIN\",\"reason\":\"owner_closed\"}"
shot /output/07-share-ended.png
if grep -q "server message not wired into the GPUI client" /output/client.log; then
    if grep -E "variant=(ShareRoster|ControlRequested|ControlDenied|ShareEnded)" /output/client.log; then
        echo "FAIL: a share message still fell through to the unhandled counter" >&2
        exit 1
    fi
fi
echo "PHASE 5 PASS: ShareEnded tears the share surfaces down and leaves a notice"

echo ""
echo "PASS: visual share/control test"
echo "  Inspect screenshots in test-output/:"
echo "    01-share-roster.png     — roster panel + presence badge"
echo "    02-control-requested.png— modal grant/deny prompt"
echo "    03-viewer-roster.png    — remote peer holds control"
echo "    04-control-hint.png     — suppressed keystroke raises the hint"
echo "    05-claim-requested.png  — Enter claimed control"
echo "    06-control-denied.png   — denial notice"
echo "    07-share-ended.png      — share torn down with its notice"
echo "  Wire record: test-output/share-wire.jsonl"
