#!/bin/bash
# Scripted E2E: the GPUI client's desktop notifications, driven end to end from
# real AI state changes and answered by a real D-Bus notification service.
#
# `notification_dispatcher.rs` shipped complete — zbus transport, `replaces_id`
# coalescing, click-to-focus — and had zero callers: no `spawn_dispatcher` and
# no `NotifReq` outside the module, so the running client fired nothing at all.
# The Bell parity row does NOT cover this: a bell reaches
# `Window::request_attention`, a completely different mechanism, which is why
# bell.sh passing was never evidence for notifications.
#
# Nothing here is forged. `scribe-hook-helper` posts a real provider hook event
# to the real `scribe-server`, the server broadcasts `AiStateChanged` to the
# window, and the client's own gate decides. The delivery lands on
# `notify-daemon.py`, an actual `org.freedesktop.Notifications` service on the
# session bus — so a recorded `Notify` call is proof the client's zbus
# dispatcher ran, and the `replaces_id` on the second call is proof the
# coalescing state machine is live rather than merely unit-tested.
#
# Requires: visual container with SCRIBE_NOTIFY=1 (session bus + notification
# service) and SCRIBE_SHARED_PANE=1 (so the client is attached to $SESSION and
# therefore receives its AI notices).
set -e

CLIENT_LOG="${SCRIBE_CLIENT_LOG:-/output/client.log}"
RECORD="${SCRIBE_NOTIFY_RECORD:-/output/notifications.jsonl}"
CONTROL="${SCRIBE_NOTIFY_CONTROL:-/tmp/scribe-notify.ctl}"
HOOK_SOCK="${SCRIBE_RUNTIME_DIR:-/run/user/$(id -u)/scribe}/server.sock"

WID=""

fail() {
    echo "$1"
    echo "--- client log tail ---"
    tail -40 "$CLIENT_LOG" || true
    echo "--- notification record ---"
    cat "$RECORD" || true
    echo "--- notify daemon log ---"
    tail -20 /output/notify-daemon.log || true
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

# Post one hook event for $SESSION straight to the server socket, exactly as a
# provider's hook would. No daemon involved.
hook() {
    SCRIBE_HOOK_SOCK="$HOOK_SOCK" SCRIBE_SESSION_ID="$SESSION" scribe-hook-helper "$@"
}

# One `Processing -> <state>` cycle: the only transition the tracker treats as
# notification-worthy, and the one the winit client used too.
attention_cycle() {
    hook --provider=claude_code --event=state_changed --state=processing
    sleep 0.8
    hook --provider=claude_code --event=state_changed --state="$1"
    sleep 1.5
}

notify_count() {
    grep -c '"call": "notify"' "$RECORD" 2>/dev/null || true
}

# Print field $2 of the $1-th (1-based) recorded Notify call.
notify_field() {
    grep '"call": "notify"' "$RECORD" | sed -n "${1}p" \
        | python3 -c "import json,sys; print(json.load(sys.stdin)['$2'])"
}

count_log() {
    grep -acF "$1" "$CLIENT_LOG" 2>/dev/null || true
}

wait_for_notify_growth() {
    local baseline="$1" timeout_secs="${2:-20}" started
    started=$(date +%s)
    while true; do
        [ "$(notify_count)" -gt "$baseline" ] && return 0
        [ $(("$(date +%s)" - started)) -ge "$timeout_secs" ] && return 1
        sleep 0.3
    done
}

wait_for_log_growth() {
    local pattern="$1" baseline="$2" timeout_secs="${3:-20}" started
    started=$(date +%s)
    while true; do
        [ "$(count_log "$pattern")" -gt "$baseline" ] && return 0
        [ $(("$(date +%s)" - started)) -ge "$timeout_secs" ] && return 1
        sleep 0.3
    done
}

FIRED="fired a desktop notification"
CLICKED="focused a session from a notification click"

# ── Phase 0: the client is attached and the bus service is up ─────
sleep 1.5
focus
[ -f "$RECORD" ] || fail "PHASE 0 FAIL: the notification service left no record file"
if ! grep -q "attaching to session" "$CLIENT_LOG"; then
    fail "PHASE 0 FAIL: the client never attached to the shared pane"
fi
scrot -o /output/00-notify-attached.png
echo "PHASE 0 PASS: client window $WID attached to $SESSION, notification service listening"

# ── Phase 1: an unfocused window fires a real Notify ──────────────
# Minimizing is the lever on the default `when_unfocused` condition: with the
# window unfocused the gate must let the transition through, and the delivery
# must arrive at the bus service as an actual method call.
xdotool windowminimize "$WID"
for _ in $(seq 1 40); do
    ACTIVE=$(xdotool getactivewindow 2>/dev/null || true)
    [ "$ACTIVE" != "$WID" ] && break
    sleep 0.25
done
[ "${ACTIVE:-}" = "$WID" ] && fail "PHASE 1 FAIL: the window never lost focus after being minimized"
sleep 1

BEFORE=$(notify_count)
FIRED_BEFORE=$(count_log "$FIRED")
attention_cycle idle_prompt
if ! wait_for_notify_growth "${BEFORE:-0}" 25; then
    fail "PHASE 1 FAIL: no Notify call ever reached the D-Bus notification service"
fi
if [ "$(count_log "$FIRED")" -le "${FIRED_BEFORE:-0}" ]; then
    fail "PHASE 1 FAIL: the client never logged firing a notification"
fi
FIRST_ID=$(notify_field 1 id)
FIRST_SUMMARY=$(notify_field 1 summary)
FIRST_REPLACES=$(notify_field 1 replaces_id)
case "$FIRST_SUMMARY" in
    *"Ready"*) ;;
    *) fail "PHASE 1 FAIL: summary '$FIRST_SUMMARY' does not name the Ready state" ;;
esac
if [ "$FIRST_REPLACES" != "0" ]; then
    fail "PHASE 1 FAIL: the first toast asked to replace id $FIRST_REPLACES"
fi
echo "PHASE 1 PASS: unfocused window fired notification id $FIRST_ID — '$FIRST_SUMMARY'"

# ── Phase 2: a second transition coalesces onto the same toast ────
# The `replaces_id` contract is the whole point of the ported dispatcher: one
# live toast per session, swapped in place. A client that stacked a second toast
# would send `replaces_id = 0` here.
BEFORE=$(notify_count)
attention_cycle permission_prompt
if ! wait_for_notify_growth "${BEFORE:-0}" 25; then
    fail "PHASE 2 FAIL: the second attention transition fired no notification"
fi
SECOND_REPLACES=$(notify_field 2 replaces_id)
SECOND_ID=$(notify_field 2 id)
SECOND_SUMMARY=$(notify_field 2 summary)
if [ "$SECOND_REPLACES" != "$FIRST_ID" ]; then
    fail "PHASE 2 FAIL: second toast replaces_id=$SECOND_REPLACES, expected $FIRST_ID"
fi
if [ "$SECOND_ID" != "$FIRST_ID" ]; then
    fail "PHASE 2 FAIL: the service allocated a new id ($SECOND_ID) — no coalescing"
fi
case "$SECOND_SUMMARY" in
    *"Permission required"*) ;;
    *) fail "PHASE 2 FAIL: summary '$SECOND_SUMMARY' does not name the permission state" ;;
esac
echo "PHASE 2 PASS: the second transition reused replaces_id $FIRST_ID — one live toast"

# ── Phase 3: clicking the toast focuses the session and raises ────
# `ActionInvoked` is emitted by the service the client actually subscribed to,
# so this is the real signal path, not an injected one. The client must both
# report the click and raise its window — the raise is observable from outside
# the process through `_NET_ACTIVE_WINDOW`.
CLICKED_BEFORE=$(count_log "$CLICKED")
printf 'invoke %s default\n' "$SECOND_ID" > "$CONTROL"
if ! wait_for_log_growth "$CLICKED" "${CLICKED_BEFORE:-0}" 25; then
    fail "PHASE 3 FAIL: the client never acted on the ActionInvoked signal"
fi
RAISED=""
for _ in $(seq 1 40); do
    ACTIVE=$(xdotool getactivewindow 2>/dev/null || true)
    if [ "$ACTIVE" = "$WID" ]; then
        RAISED=1
        break
    fi
    sleep 0.25
done
[ -z "$RAISED" ] && fail "PHASE 3 FAIL: the clicked notification never raised the window"
scrot -o /output/01-notify-clicked.png
echo "PHASE 3 PASS: the click raised window $WID and focused its session"

# ── Phase 4: the focused foreground pane is silent ────────────────
# The gate is state, not a one-shot: with the window focused on the very session
# that would notify, the default `when_unfocused` condition must suppress it.
# The transition must still be *ingested* — that is the half a broken drain
# would lose.
focus
BEFORE=$(notify_count)
attention_cycle idle_prompt
sleep 2
if [ "$(notify_count)" -ne "${BEFORE:-0}" ]; then
    fail "PHASE 4 FAIL: a transition on the focused foreground pane still notified"
fi
echo "PHASE 4 PASS: the focused foreground pane fired nothing"

echo ""
echo "PASS: visual notification test"
echo "  Recorded D-Bus calls: $RECORD"
echo "  Inspect screenshots in test-output/:"
echo "    00-notify-attached.png — client attached to the shared pane"
echo "    01-notify-clicked.png  — window raised by the notification click"
