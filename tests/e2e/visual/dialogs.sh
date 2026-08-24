#!/bin/bash
[ "${SCRIBE_E2E_SANDBOX:-0}" = "1" ] || { echo "FATAL: this script only runs inside the scribe e2e container (use just e2e-func / e2e-visual)." >&2; exit 99; }
# Visual E2E test: the modal dialog suite of the GPUI client rebuild, walking two
# representative modals — the close dialog (three buttons: Quit Scribe / Kill
# Window / Cancel, Cancel default focus) and the OSC 52 clipboard dialog (four
# buttons: Deny once / Always deny / Allow once / Always allow, Deny once default
# focus, with a payload preview).
#
# Drives the live Scribe window: Ctrl+Shift+D opens the close dialog and
# Ctrl+Shift+K opens the clipboard dialog. The close dialog moved off
# Ctrl+Shift+Q, which is the Linux default for `close_tab` and now reaches that
# action instead (see tests/e2e/visual/tab-window-chords.sh).
#
# Tab cycles the focused button, Enter activates it, Esc dismisses with the safe
# action. Every modal is drawn with a dimmed backdrop, a rounded drop-shadowed
# box, a centred title, the body copy, a separator rule, and accent/destructive
# button tones.
#
# Requires: visual container (optional GPU passthrough via SCRIBE_E2E_GPUS), xdotool, scrot.
set -e

fail() {
    echo "FAIL: $1" >&2
    exit 1
}

pixel_diff() {
    local value
    value=$(compare -metric AE "$1" "$2" null: 2>&1 || true)
    printf '%s' "${value%%.*}"
}

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
    scrot -o "$1"
    echo "captured $1"
}

send_keys() {
    local wid
    wid=$(find_window) || true
    if [ -n "$wid" ]; then
        xdotool key --window "$wid" "$@"
        sleep 0.3
    fi
}

# ── Phase 1: close dialog opens with Cancel default focus ─────────
# Wait for the window to be mapped *and* genuinely active before typing:
# X11FocusGuard drops every keystroke while `_NET_ACTIVE_WINDOW` names someone
# else, so a chord sent too early is swallowed and phase 1 captures a black
# frame instead of the dialog.
xdotool search --sync --name "Scribe" >/dev/null 2>&1 || true
sleep 1.5
focus
sleep 0.5
shot /output/00-dialog-baseline.png
send_keys ctrl+shift+d
shot /output/01-close-dialog.png
DIFF=$(pixel_diff /output/00-dialog-baseline.png /output/01-close-dialog.png)
[ "$DIFF" -ge 500 ] || fail "close dialog changed only $DIFF pixels"
echo "PHASE 1 PASS: close dialog painted ($DIFF px)"

send_keys Tab
shot /output/02-close-focus-moved.png
DIFF=$(pixel_diff /output/01-close-dialog.png /output/02-close-focus-moved.png)
[ "$DIFF" -ge 10 ] || fail "close-dialog focus changed only $DIFF pixels"
echo "PHASE 2 PASS: Tab repainted the focused button ($DIFF px)"

send_keys Escape
sleep 0.3
shot /output/03-close-dismissed.png
DIFF=$(pixel_diff /output/01-close-dialog.png /output/03-close-dismissed.png)
[ "$DIFF" -ge 500 ] || fail "Escape left the close dialog visible"
echo "PHASE 3 PASS: Escape dismissed the close dialog"

# ── Phase 4: clipboard dialog with four-button policy + preview ───
focus
send_keys ctrl+shift+k
shot /output/04-clipboard-dialog.png
DIFF=$(pixel_diff /output/03-close-dismissed.png /output/04-clipboard-dialog.png)
[ "$DIFF" -ge 500 ] || fail "clipboard dialog changed only $DIFF pixels"
echo "PHASE 4 PASS: clipboard dialog painted ($DIFF px)"

send_keys Tab
send_keys Tab
shot /output/05-clipboard-allow-focus.png
DIFF=$(pixel_diff /output/04-clipboard-dialog.png /output/05-clipboard-allow-focus.png)
[ "$DIFF" -ge 10 ] || fail "clipboard-dialog focus changed only $DIFF pixels"
echo "PHASE 5 PASS: Tab repainted the focused button ($DIFF px)"

send_keys Escape
sleep 0.3

echo ""
echo "PASS: visual dialog-suite test"
echo "  Inspect screenshots in test-output/:"
echo "    01-close-dialog.png         — close dialog at rest (Cancel focused)"
echo "    02-close-focus-moved.png    — Tab moved focus to Quit Scribe"
echo "    03-close-dismissed.png      — Esc tore the modal down"
echo "    04-clipboard-dialog.png     — OSC 52 clipboard dialog + preview"
echo "    05-clipboard-allow-focus.png — focus on the destructive Allow once"
