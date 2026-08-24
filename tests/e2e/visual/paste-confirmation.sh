#!/bin/bash
[ "${SCRIBE_E2E_SANDBOX:-0}" = "1" ] || { echo "FATAL: this script only runs inside the scribe visual e2e container." >&2; exit 99; }
# @lat: [[test#Visual E2E Tests#Paste confirmation blocks risky bytes]]
set -euo pipefail

SESSION="${SESSION:?paste confirmation requires the shared-pane rig}"
RISKY_MARKER=RISKY_PASTE_PROBE
PLAIN_MARKER=PLAIN_PASTE_PROBE

fail() {
    echo "FAIL: $1" >&2
    exit 1
}

find_window() {
    local wid
    wid=$(xdotool search --class '[Ss]cribe' 2>/dev/null | head -1)
    [ -n "$wid" ] || wid=$(xdotool search --name '[Ss]cribe' 2>/dev/null | head -1)
    printf '%s' "$wid"
}

focus() {
    WID=$(find_window)
    [ -n "$WID" ] || fail "no Scribe window"
    xdotool windowactivate --sync "$WID" 2>/dev/null \
        || xdotool windowfocus --sync "$WID" 2>/dev/null || true
    sleep 0.4
}

shot() {
    focus
    import -window "$WID" +repage "$1" 2>/dev/null || fail "could not capture Scribe window"
}

pixel_diff() {
    local value
    value=$(compare -metric AE "$1" "$2" null: 2>&1 || true)
    printf '%s' "${value%%.*}"
}

paste_clipboard() {
    printf '%s' "$1" | xclip -selection clipboard >/dev/null 2>&1
    sleep 0.2
    focus
    xdotool key --clearmodifiers ctrl+shift+v
    sleep 0.5
}

assert_not_delivered() {
    if scribe-test wait-output "$SESSION" "$1" --timeout 600 >/dev/null 2>&1; then
        fail "$1 reached the PTY"
    fi
}

focus
xdotool type --clearmodifiers --delay 30 'cat -v'
xdotool key --clearmodifiers Return
scribe-test wait-idle "$SESSION" --ms 300
shot /output/paste-00-baseline.png

paste_clipboard "$(printf '%s\033[31m' "$RISKY_MARKER")"
shot /output/paste-01-dialog.png
DIFF=$(pixel_diff /output/paste-00-baseline.png /output/paste-01-dialog.png)
[ "$DIFF" -ge 500 ] || fail "risky paste dialog changed only $DIFF pixels"
assert_not_delivered "$RISKY_MARKER"
echo "PHASE 1 PASS: risky control paste was parked behind a visible dialog ($DIFF px)"

xdotool key --clearmodifiers Escape
sleep 0.4
assert_not_delivered "$RISKY_MARKER"
echo "PHASE 2 PASS: Escape discarded the parked bytes"

paste_clipboard "$PLAIN_MARKER"
scribe-test wait-output "$SESSION" "$PLAIN_MARKER" --timeout 3000 >/dev/null \
    || fail "plain paste did not reach the PTY"
echo "PHASE 3 PASS: plain single-line paste bypassed the gate"

xdotool key --clearmodifiers ctrl+c
pkill -x xclip 2>/dev/null || true
echo "PASS: paste confirmation blocks risky bytes and passes plain text"
