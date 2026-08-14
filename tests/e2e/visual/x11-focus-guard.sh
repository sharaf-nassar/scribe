#!/bin/bash
[ "${SCRIBE_E2E_SANDBOX:-0}" = "1" ] || { echo "FATAL: this script only runs inside the scribe e2e container (use just e2e-func / e2e-visual)." >&2; exit 99; }
# Visual E2E test: the GPUI client's X11 active-window guard, started from the
# live window-open path (`main.rs::open_window` -> `TerminalView::new`).
#
# The guard reads this window's Xcb window id from GPUI's `RawWindowHandle` and
# compares it with the root `_NET_ACTIVE_WINDOW` property that openbox owns. It
# sits ahead of every keyboard consumer, so while another window is the active
# one — the compositor-overlay case, e.g. GNOME Screenshot covering the terminal
# — a keystroke delivered to this window must reach nothing at all.
#
# The probe keystroke is Ctrl+Shift+U, the client-local tooltip-demo toggle: it
# is consumed by the overlay router and never reaches the PTY, so the guard's
# verdict is visible as a pure pixel change inside the window and nothing leaks
# into a shell session. Each phase compares the tooltip region of the window,
# cropped away from the live status-bar sparklines so the only thing that can
# move a pixel is the toggle itself.
#
# Requires: visual container (Xvfb + openbox + xdotool + scrot + imagemagick).
set -e

LOG="${SCRIBE_CLIENT_LOG:-/output/client.log}"
OUT=/output
# Tooltip-demo region of the 960x680 window: the demo anchors the box at
# (780, 120) and clamps it into the viewport, so this crop contains the whole
# tooltip and none of the status bar, whose sparklines resample every 2 s.
CROP="420x140+520+90"
SUPPRESSED="x11 focus guard suppressed keystroke"

fail() {
    echo "FAIL: $1" >&2
    exit 1
}

suppression_count() {
    grep -c "$SUPPRESSED" "$LOG" 2>/dev/null || true
}

# Raise, focus, and capture just $1's own pixels, cropped to the tooltip region.
shot() {
    local wid="$1" name="$2"
    xdotool windowactivate --sync "$wid" 2>/dev/null || xdotool windowfocus --sync "$wid"
    sleep 0.4
    import -window "$wid" -crop "$CROP" +repage "$OUT/$name"
    echo "captured $name"
}

# Absolute pixel difference between two captures.
diff_pixels() {
    compare -metric AE "$OUT/$1" "$OUT/$2" null: 2>&1 || true
}

# The window openbox currently publishes in the root `_NET_ACTIVE_WINDOW` —
# exactly the property the guard reads.
active_window() {
    xdotool getactivewindow 2>/dev/null || echo none
}

send_toggle() {
    xdotool key --window "$1" ctrl+shift+u
    sleep 0.6
}

sleep 1.0

W1=$(xdotool search --name '[Ss]cribe' | head -1)
[ -n "$W1" ] || fail "no Scribe window found"
echo "client window: $W1"

# ── Phase 0: the guard is started by the running client, not merely present ──
grep -q "X11 active-window guard enabled" "$LOG" \
    || fail "client never logged the X11 active-window guard starting"
echo "PHASE 0 PASS: open_window started the X11 active-window guard"

# ── Phase 1: window is the active window, so the keystroke is delivered ──────
xdotool windowactivate --sync "$W1"
sleep 0.6
[ "$(active_window)" = "$W1" ] \
    || fail "phase 1: _NET_ACTIVE_WINDOW does not name the client window"
shot "$W1" 00-baseline.png
before=$(suppression_count)
send_toggle "$W1"
shot "$W1" 01-tooltip-on.png
[ "$(diff_pixels 00-baseline.png 01-tooltip-on.png)" != "0" ] \
    || fail "phase 1: Ctrl+Shift+U changed nothing while the window was active"
[ "$(suppression_count)" = "$before" ] \
    || fail "phase 1: guard suppressed a keystroke while our window was active"
echo "PHASE 1 PASS: active window — keystroke reached the overlay router"

# ── Phase 2: another window owns _NET_ACTIVE_WINDOW, so the key is dropped ───
# xmessage is the stand-in for the compositor overlay: it takes
# _NET_ACTIVE_WINDOW, and the keystroke is still delivered straight to the
# client with XSendEvent, so only the guard can stop it. A plain second Scribe
# launch now hands focus back through the terminal singleton instead.
command -v xmessage >/dev/null || fail "phase 2: xmessage is missing from the image"
xmessage -geometry 140x60-0-0 'focus guard overlay' >"$OUT/xmessage-overlay.log" 2>&1 &
OVERLAY_PID=$!
trap 'kill "$OVERLAY_PID" 2>/dev/null || true' EXIT
for _ in $(seq 1 40); do
    W2=$(xdotool search --name '[Xx]message' 2>/dev/null | tail -1)
    [ -n "$W2" ] && break
    sleep 0.25
done
[ -n "$W2" ] || fail "phase 2: the overlay stand-in window never appeared"
xdotool windowactivate --sync "$W2"
sleep 0.6
[ "$(active_window)" = "$W2" ] \
    || fail "phase 2: _NET_ACTIVE_WINDOW does not name the overlay stand-in"
echo "overlay window: $W2 (active: $(active_window))"

before=$(suppression_count)
send_toggle "$W1"
shot "$W1" 02-suppressed.png
[ "$(suppression_count)" -gt "$before" ] \
    || fail "phase 2: the keystroke never reached the guard (nothing suppressed)"
[ "$(diff_pixels 01-tooltip-on.png 02-suppressed.png)" = "0" ] \
    || fail "phase 2: the suppressed keystroke still changed the window"
echo "PHASE 2 PASS: inactive window — guard dropped the keystroke, pixels unchanged"

# ── Phase 3: activation returns, and the same keystroke lands again ──────────
kill "$OVERLAY_PID" 2>/dev/null || true
trap - EXIT
xdotool windowactivate --sync "$W1"
# Past REACTIVATION_DEBOUNCE (300 ms) so the assertion is about the guard's
# steady state, not the debounce window (unit-tested in x11_focus.rs).
sleep 0.8
[ "$(active_window)" = "$W1" ] \
    || fail "phase 3: the client window never became active again"
before=$(suppression_count)
send_toggle "$W1"
shot "$W1" 03-tooltip-off.png
[ "$(suppression_count)" = "$before" ] \
    || fail "phase 3: guard still suppressed after the window became active again"
[ "$(diff_pixels 01-tooltip-on.png 03-tooltip-off.png)" != "0" ] \
    || fail "phase 3: Ctrl+Shift+U changed nothing after re-activation"
[ "$(diff_pixels 00-baseline.png 03-tooltip-off.png)" = "0" ] \
    || fail "phase 3: the window did not return to its pre-toggle state"
echo "PHASE 3 PASS: re-activated window — keystroke reached the router again"

echo ""
echo "PASS: X11 active-window guard is live on the GPUI key path"
echo "  Inspect screenshots in test-output/:"
echo "    00-baseline.png    — tooltip demo off"
echo "    01-tooltip-on.png  — Ctrl+Shift+U landed while active"
echo "    02-suppressed.png  — same key dropped while another window was active"
echo "    03-tooltip-off.png — key landed again after re-activation"
