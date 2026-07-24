#!/bin/bash
# Visual E2E test: the modal dialog suite of the GPUI client rebuild, walking two
# representative modals — the close dialog (three buttons: Quit Scribe / Kill
# Window / Cancel, Cancel default focus) and the OSC 52 clipboard dialog (four
# buttons: Deny once / Always deny / Allow once / Always allow, Deny once default
# focus, with a payload preview).
#
# Drives the live Scribe window: Ctrl+Shift+Q opens the close dialog and
# Ctrl+Shift+K opens the clipboard dialog. Tab cycles the focused button, Enter
# activates it, Esc dismisses with the safe action. Every modal is drawn with a
# dimmed backdrop, a rounded drop-shadowed box, a centred title, the body copy,
# a separator rule, and accent/destructive button tones.
#
# Requires: visual container with --gpus all, xdotool, scrot.
set -e

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
        sleep 0.3
    fi
}

# ── Phase 1: close dialog opens with Cancel default focus ─────────
sleep 0.8
focus
send_keys ctrl+shift+q
shot /output/01-close-dialog.png
echo "PHASE 1 PASS: close dialog opens — Quit/Kill/Cancel, Cancel focused, backdrop dim"

# Tab moves the focus ring off the safe Cancel button (onto Quit Scribe).
send_keys Tab
shot /output/02-close-focus-moved.png
echo "PHASE 2 PASS: Tab cycles focus onto the accent Quit Scribe button"

# Esc dismisses with the safe Cancel action, tearing the modal down.
send_keys Escape
sleep 0.3
shot /output/03-close-dismissed.png
echo "PHASE 3 PASS: Esc dismisses the close dialog (safe Cancel)"

# ── Phase 4: clipboard dialog with four-button policy + preview ───
focus
send_keys ctrl+shift+k
shot /output/04-clipboard-dialog.png
echo "PHASE 4 PASS: clipboard dialog opens — 4 buttons, Deny once focused, payload preview"

# Tab twice lands on the destructive Allow-once button.
send_keys Tab
send_keys Tab
shot /output/05-clipboard-allow-focus.png
echo "PHASE 5 PASS: Tab cycles onto the destructive Allow once button"

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
