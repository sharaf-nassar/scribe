#!/bin/bash
# Visual E2E test: spec-011 paste confirmation (US2 control-character trigger
# + non-risky passthrough edge cases).
#
# Pastes into the REAL scribe-client window via xdotool (the only way to
# exercise the client-side paste gate; scribe-test `Send` injects server-side
# and bypasses it). Requires: visual container with --gpus all, xclip, and
# SCRIBE_EXTRA_CONFIG seeding `terminal.paste_confirmation = true`.
#
# Screenshots land in /output for inspection. A risky paste is gated only when
# the focused app has NOT enabled bracketed paste, so each case runs inside
# `cat` (which leaves the terminal unbracketed).
set -e

# The window title follows the shell (e.g. "root@host:~#"), so match the
# stable WM_CLASS instead of the volatile name; fall back to name just in case.
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
        # With a WM present, windowactivate sets _NET_ACTIVE_WINDOW so the
        # client's X11 focus guard accepts synthetic key input; fall back to
        # windowfocus if activate is unavailable.
        xdotool windowactivate --sync "$wid" 2>/dev/null \
            || xdotool windowfocus --sync "$wid" 2>/dev/null || true
        sleep 0.4
    fi
}

shot() {
    focus
    sleep 0.2
    scrot "$1"
    echo "captured $1"
}

paste_clipboard() {
    # Populate the X clipboard, then trigger the client's paste keybinding.
    # Redirect xclip's fds: it forks a daemon to serve the selection, and if
    # that daemon inherits the entrypoint's `tee` pipe the container never exits.
    printf '%s' "$1" | xclip -selection clipboard >/dev/null 2>&1
    sleep 0.3
    focus
    xdotool key ctrl+shift+v
    sleep 0.9
}

echo "DIAG: start $(date -u +%H:%M:%SZ)"
date -u +%s > /output/RUN_MARKER.txt 2>&1 && echo "DIAG: wrote /output/RUN_MARKER.txt" || echo "DIAG: /output NOT writable"
echo "DIAG: class=[$(xdotool search --class '[Ss]cribe' 2>/dev/null | tr '\n' ' ')] name=[$(xdotool search --name '[Ss]cribe' 2>/dev/null | tr '\n' ' ')] all=[$(xdotool search --onlyvisible '' 2>/dev/null | tr '\n' ' ')]"

focus
sleep 0.5

# Enter an unbracketed context: cat echoes stdin and does not enable bracketed
# paste, so the gate is not deferred.
xdotool type --delay 30 "cat"
xdotool key Return
sleep 0.6

# ── Phase A (US2): single-line paste containing control/escape bytes ──────
# Embedded ESC (\033), NO line break — exercises the control-only trigger and
# the caret-notation preview (ESC must render as ^[ , never a raw byte).
paste_clipboard "$(printf 'echo \033[31mRED\033[0m here')"
shot /output/A1-control-char-dialog.png
echo "PHASE A: expect dialog — reason 'contains control characters', preview shows ^[ (caret notation)"

# Cancel must drop the paste entirely.
xdotool key Escape
sleep 0.6
shot /output/A2-after-cancel.png
echo "PHASE A: expect Esc dropped the paste — cat shows nothing"

# ── Phase B (edge): plain single line, no control — must NOT trigger ──────
paste_clipboard "plain-single-line-no-newline"
shot /output/B-plain-no-dialog.png
echo "PHASE B: expect NO dialog — plain single line pasted straight into cat"
xdotool key ctrl+u 2>/dev/null || true   # clear cat's current input line

# ── Phase C (edge): tabs-only single line — tab is excluded, NO trigger ───
paste_clipboard "$(printf 'col1\tcol2\tcol3')"
shot /output/C-tabs-no-dialog.png
echo "PHASE C: expect NO dialog — tab is not a control trigger"

# Reap any lingering xclip selection daemons so the entrypoint's pipe closes
# and the container exits promptly.
pkill -x xclip 2>/dev/null || true
echo "PASS: paste-confirmation visual scenarios captured to /output"
