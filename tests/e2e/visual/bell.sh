#!/bin/bash
[ "${SCRIBE_E2E_SANDBOX:-0}" = "1" ] || { echo "FATAL: this script only runs inside the scribe e2e container (use just e2e-func / e2e-visual)." >&2; exit 99; }
# Scripted E2E: the terminal bell of the GPUI client, driven end to end from a
# real BEL byte written by a real shell in a real server-owned PTY.
#
# Backs the `Bell` parity row. `bell.rs` held the ported suppression gate but
# was outside the binary's import closure, and the live reader had no `Bell`
# arm, so every `ServerMessage::Bell` fell into the reader's catch-all: the
# module's own `#[gpui::test]`s were green while the running client did nothing
# at all with a bell.
#
# The routed behaviour is the winit client's: `request_user_attention`, which on
# X11 is the WM_HINTS urgency flag. That is what makes this assertable from
# outside the process — `xprop` reads the flag straight off the X server, so
# "the client asked the window manager for attention" is a property on the
# window rather than an inference from a screenshot. The suppressed case is
# asserted the same way, by its *absence* plus the reader's own "received" line:
# together they separate "the bell was ingested" from "the bell was routed",
# which is exactly the distinction the unwired client failed.
#
# Requires: visual container with SCRIBE_SHARED_PANE=1 (so `scribe-test send`
# types into the very pane the client renders), xdotool, x11-utils, scrot,
# imagemagick.
set -e

CLIENT_LOG="${SCRIBE_CLIENT_LOG:-/output/client.log}"

WID=""
GRID_X=0
GRID_Y=0
GRID_W=0
GRID_H=0

find_window() {
    local wid
    wid=$(xdotool search --class '[Ss]cribe' 2>/dev/null | tail -1)
    [ -z "$wid" ] && wid=$(xdotool search --name '[Ss]cribe' 2>/dev/null | tail -1)
    printf '%s' "$wid"
}

fail() {
    echo "$1"
    echo "--- client log tail ---"
    tail -40 "$CLIENT_LOG" || true
    echo "--- WM_HINTS ---"
    [ -n "$WID" ] && xprop -id "$WID" WM_HINTS || true
    exit 1
}

focus() {
    WID=$(find_window)
    if [ -z "$WID" ]; then
        fail "FAIL: no Scribe window found"
    fi
    xdotool windowactivate --sync "$WID" 2>/dev/null \
        || xdotool windowfocus --sync "$WID" 2>/dev/null || true
    sleep 0.5
    eval "$(xdotool getwindowgeometry --shell "$WID")"
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

# Lit pixels inside the client window of a full-screen capture, used only to
# prove the window is really rendering a pane before any bell is sent.
grid_ink() {
    local value
    value=$(convert "$1" -crop "${GRID_W}x${GRID_H}+${GRID_X}+${GRID_Y}" +repage \
        -colorspace Gray -threshold 35% -format "%[fx:mean*w*h]" info:)
    printf '%s' "${value%.*}"
}

count_log() {
    grep -acF "$1" "$CLIENT_LOG" 2>/dev/null || true
}

wait_for_log_growth() {
    local pattern="$1" baseline="$2" timeout_secs="${3:-15}" now started
    started=$(date +%s)
    while true; do
        now=$(count_log "$pattern")
        if [ "$now" -gt "$baseline" ]; then
            return 0
        fi
        if [ $(( "$(date +%s)" - started )) -ge "$timeout_secs" ]; then
            return 1
        fi
        sleep 0.3
    done
}

# `xprop` prints the urgency line only when the WM_HINTS urgency flag is
# actually set, so its presence is the assertion and its absence is the
# suppressed case.
urgency_is_set() {
    xprop -id "$WID" WM_HINTS 2>/dev/null | grep -qi 'urgency hint bit is set'
}

# Ring the terminal bell from inside the pane: the shell writes a BEL byte, the
# server's `Term` turns it into `MetadataEvent::Bell`, and the server broadcasts
# `ServerMessage::Bell` to this window's clients. Nothing here is forged.
ring_bell() {
    scribe-test send "$SESSION" 'printf "\a"\n'
}

RECEIVED="terminal bell received"
SIGNALLED="terminal bell requested window attention"

# ── Phase 0: the client is attached and painting the shared pane ──
sleep 1.0
BASE_INK=0
for _ in $(seq 1 40); do
    focus
    shot /output/00-bell-attached.png >/dev/null
    BASE_INK=$(grid_ink /output/00-bell-attached.png)
    [ "$BASE_INK" -ge 20 ] && break
    sleep 0.5
done
if [ "$BASE_INK" -lt 20 ]; then
    fail "PHASE 0 FAIL: the client rendered no pane content (ink $BASE_INK)"
fi
echo "PHASE 0 PASS: client window $WID is attached to session $SESSION (grid ink $BASE_INK)"

# ── Phase 1: a bell to the focused foreground pane is suppressed ──
# The window is focused and $SESSION is the pane it shows, which is precisely
# the winit condition for staying silent. The bell must still be *ingested* —
# that is the half the catch-all used to swallow.
focus
if urgency_is_set; then
    fail "PHASE 1 FAIL: the window already carried an urgency hint before any bell"
fi
RECEIVED_BEFORE=$(count_log "$RECEIVED")
SIGNALLED_BEFORE=$(count_log "$SIGNALLED")
ring_bell
if ! wait_for_log_growth "$RECEIVED" "${RECEIVED_BEFORE:-0}" 20; then
    fail "PHASE 1 FAIL: the running client never ingested a ServerMessage::Bell"
fi
# Give the 200 ms gate tick several chances to have signalled if it were going
# to, so "no signal" is a settled result rather than a race.
sleep 2
if [ "$(count_log "$SIGNALLED")" -ne "${SIGNALLED_BEFORE:-0}" ]; then
    fail "PHASE 1 FAIL: a bell to the focused foreground pane requested attention"
fi
if urgency_is_set; then
    fail "PHASE 1 FAIL: a suppressed bell still set the WM_HINTS urgency flag"
fi
echo "PHASE 1 PASS: bell ingested by the running client and suppressed on the focused pane"

# ── Phase 2: a bell to an unfocused window is routed ──────────────
# Iconifying is the one lever this rig has on the other half of the gate: with
# the window unfocused every pane is a background pane, so the same bell that
# was just silent must now reach `Window::request_attention`.
xdotool windowminimize "$WID"
for _ in $(seq 1 40); do
    ACTIVE=$(xdotool getactivewindow 2>/dev/null || true)
    [ "$ACTIVE" != "$WID" ] && break
    sleep 0.25
done
if [ "${ACTIVE:-}" = "$WID" ]; then
    fail "PHASE 2 FAIL: the window never lost focus after being minimized"
fi
sleep 1
SIGNALLED_BEFORE=$(count_log "$SIGNALLED")
ring_bell
if ! wait_for_log_growth "$SIGNALLED" "${SIGNALLED_BEFORE:-0}" 20; then
    fail "PHASE 2 FAIL: the unfocused client never routed the bell to an attention request"
fi
if ! urgency_is_set; then
    fail "PHASE 2 FAIL: the attention request left no WM_HINTS urgency flag on the window"
fi
echo "PHASE 2 PASS: the unfocused window carries the WM_HINTS urgency hint the bell asked for"
xprop -id "$WID" WM_HINTS | sed -n 's/^\t*//p' | tail -3

# ── Phase 3: refocusing restores the suppressed behaviour ─────────
# The gate is state, not a one-shot: bringing the window back must make the
# foreground pane silent again, which is what proves phase 2 was the gate
# opening rather than the routing simply having warmed up.
focus
shot /output/01-bell-refocused.png
SIGNALLED_BEFORE=$(count_log "$SIGNALLED")
RECEIVED_BEFORE=$(count_log "$RECEIVED")
ring_bell
if ! wait_for_log_growth "$RECEIVED" "${RECEIVED_BEFORE:-0}" 20; then
    fail "PHASE 3 FAIL: the refocused client stopped ingesting bells"
fi
sleep 2
if [ "$(count_log "$SIGNALLED")" -ne "${SIGNALLED_BEFORE:-0}" ]; then
    fail "PHASE 3 FAIL: a bell to the refocused foreground pane requested attention again"
fi
echo "PHASE 3 PASS: the refocused foreground pane is silent again"

echo ""
echo "PASS: visual bell test"
echo "  Inspect screenshots in test-output/:"
echo "    00-bell-attached.png   — client attached to the shared pane"
echo "    01-bell-refocused.png  — window restored after the urgency hint was asserted"
