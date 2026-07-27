#!/bin/bash
# Scripted E2E: IME composition in the GPUI client, driven by a real input
# method (ibus + the table-driven CangJie3 engine) over XIM.
#
# Backs the `IME / preedit composition` parity row. `preedit.rs` shipped the
# whole ported surface — state machine, overlay geometry, and a
# `gpui::EntityInputHandler` — but nothing ever called `Window::handle_input`
# with it, so the platform had no handler to deliver marked or committed text
# to. The failure mode is silent and looks like "the IME is off": every key
# press falls through to the byte encoder and the raw latin letters land in the
# shell, which is precisely what phase 2 below asserts must NOT happen.
#
# This is the only assertion shape that separates "wired" from "unwired":
#   * the client's own log proves the platform reached the handler at all
#     (`IME preedit updated`, then `IME committed text`),
#   * the server-owned PTY proves the raw keys never leaked (`hqi` absent),
#   * and the committed pane content proves the composed characters did land.
#
# The engine is CangJie3, whose composition is a fixed table lookup rather than
# a phonetic guess: h-q-i is 竹手戈, which composes 我 every time.
#
# Requires: visual container with SCRIBE_IME=1 (starts ibus with an XIM server
# and exports XMODIFIERS before the client launches) and SCRIBE_SHARED_PANE=1
# (so `scribe-test` reads the very pane the client types into), xdotool, scrot,
# imagemagick.
set -e

CLIENT_LOG="${SCRIBE_CLIENT_LOG:-/output/client.log}"
# The CangJie3 radical keys for 我 (竹手戈). Distinctive as a latin substring,
# so its presence in the PTY is unambiguous evidence of a leak.
COMPOSE_KEYS="h q i"
COMPOSE_LATIN="hqi"

WID=""
GRID_X=0
GRID_Y=0
GRID_W=0
GRID_H=0

fail() {
    echo "$1"
    echo "--- client log tail ---"
    tail -60 "$CLIENT_LOG" 2>/dev/null || true
    echo "--- ibus log ---"
    tail -30 /output/ibus.log 2>/dev/null || true
    echo "--- ibus engine ---"
    ibus engine 2>&1 || true
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
    sleep 0.4
    scrot -o "$1"
    echo "captured $1"
}

# Pixels that differ between two full-screen captures inside the client window.
window_diff() {
    local value
    value=$(compare -metric AE \
        \( "$1" -crop "${GRID_W}x${GRID_H}+${GRID_X}+${GRID_Y}" +repage \) \
        \( "$2" -crop "${GRID_W}x${GRID_H}+${GRID_X}+${GRID_Y}" +repage \) \
        null: 2>&1 || true)
    printf '%s' "${value%%.*}"
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

# Type one key through XTEST, slowly enough that the IME's own event loop keeps
# up with each forwarded key.
#
# XTEST rather than `--window`: a synthetic `XSendEvent` reaches the window but
# not the IM's own grab, so the composition would be driven by one path and the
# commit delivered on another.
tap() {
    xdotool key --clearmodifiers "$1"
    sleep 0.4
}

# Print the pane's visible grid as plain text.
#
# The snapshot is the SERVER's copy of the terminal, so what it says arrived is
# what the PTY really received — the one place a leaked keystroke cannot hide.
pty_text() {
    scribe-test snapshot "$SESSION" "$1" >/dev/null
    python3 - "$1" <<'PY'
import json, sys

with open(sys.argv[1], encoding="utf-8") as handle:
    snapshot = json.load(handle)
cells, cols, rows = snapshot["cells"], snapshot["cols"], snapshot["rows"]
for row in range(rows):
    line = "".join(cell["c"] for cell in cells[row * cols : (row + 1) * cols])
    line = line.rstrip()
    if line:
        print(line)
PY
}

MARKED="IME preedit updated"
COMMITTED="IME committed text"
SENT="committing IME text to the focused pane"

# ── Phase 0: the input method is live and the client owns the keyboard ──
if ! ibus engine 2>/dev/null | grep -q 'table:cangjie'; then
    # Re-selecting is cheap and covers an engine that was not yet registered
    # when the entrypoint asked for it.
    ibus engine table:cangjie3 >/dev/null 2>&1 || true
    sleep 1
fi
ENGINE=$(ibus engine 2>&1 || true)
case "$ENGINE" in
    *cangjie*) ;;
    *) fail "PHASE 0 FAIL: ibus is not running the CangJie engine (got: $ENGINE)" ;;
esac
focus
sleep 1.0
shot /output/ime-00-idle.png
echo "PHASE 0 PASS: ibus engine '$ENGINE' is active and Scribe window $WID has focus"

# ── Phase 1: composing raises a preedit the client actually received ──
# `IME preedit updated` is emitted by `preedit.rs`'s
# `replace_and_mark_text_in_range`, which only the platform can call, and only
# through the handler the paint pass registers. Its appearance is therefore
# direct evidence that `Window::handle_input` ran with the `Ime` entity.
MARKED_BEFORE=$(count_log "$MARKED")
for key in $COMPOSE_KEYS; do
    tap "$key"
done
if ! wait_for_log_growth "$MARKED" "${MARKED_BEFORE:-0}" 20; then
    fail "PHASE 1 FAIL: the client never received marked text — no input handler is registered"
fi
shot /output/ime-01-composing.png
DIFF=$(window_diff /output/ime-00-idle.png /output/ime-01-composing.png)
if [ "${DIFF:-0}" -lt 20 ]; then
    fail "PHASE 1 FAIL: composing changed only ${DIFF:-0} px; nothing was drawn for the preedit"
fi
echo "PHASE 1 PASS: composition delivered to the client and painted (+$DIFF px changed)"

# ── Phase 2: the raw keys never reached the PTY ────────────────────────
# This is the regression the bug was: with no input handler the keystrokes fall
# through to the encoder and the shell's command line reads `hqi`.
COMPOSING_PTY=$(pty_text /output/ime-02-composing-pty.json)
if printf '%s' "$COMPOSING_PTY" | grep -qF "$COMPOSE_LATIN"; then
    echo "$COMPOSING_PTY"
    fail "PHASE 2 FAIL: the raw composition keys leaked to the PTY"
fi
echo "PHASE 2 PASS: the composition stayed in the IME; no raw '$COMPOSE_LATIN' on the PTY"

# ── Phase 3: committing sends the composed characters to the pane ──────
COMMITTED_BEFORE=$(count_log "$COMMITTED")
SENT_BEFORE=$(count_log "$SENT")
tap space
if ! wait_for_log_growth "$COMMITTED" "${COMMITTED_BEFORE:-0}" 20; then
    fail "PHASE 3 FAIL: selecting a candidate produced no commit in the client"
fi
if ! wait_for_log_growth "$SENT" "${SENT_BEFORE:-0}" 20; then
    fail "PHASE 3 FAIL: the commit never reached the focused pane's KeyInput path"
fi
# Poll rather than sleep: the commit still has to cross the IPC socket, be
# written to the PTY, be echoed by the shell, and come back as a screen update,
# and a fixed wait either flakes or is dead time on every green run.
COMMITTED_PTY=""
for _ in $(seq 1 40); do
    COMMITTED_PTY=$(pty_text /output/ime-03-committed-pty.json)
    if printf '%s' "$COMMITTED_PTY" | grep -qP '[^\x00-\x7F]'; then
        break
    fi
    sleep 0.5
done
shot /output/ime-03-committed.png
if printf '%s' "$COMMITTED_PTY" | grep -qF "$COMPOSE_LATIN"; then
    echo "$COMMITTED_PTY"
    fail "PHASE 3 FAIL: the raw composition keys reached the PTY after the commit"
fi
if ! printf '%s' "$COMMITTED_PTY" | grep -qP '[^\x00-\x7F]'; then
    echo "$COMMITTED_PTY"
    fail "PHASE 3 FAIL: no composed character reached the pane"
fi
echo "PHASE 3 PASS: the composed characters landed on the PTY and the latin keys never did"
printf '  pane now reads: %s\n' "$(printf '%s' "$COMMITTED_PTY" | tail -1)"

# ── Phase 4: a passthrough key is typed exactly once ───────────────────
# Registering an input handler changes what gpui does with an ordinary key: an
# un-stopped `KeyDown` is followed by `replace_text_in_range(key_char)`, so the
# character arrives at the terminal twice — once from its own encoder and once
# through the IME entity. The regression only exists while a handler is
# registered, which is exactly the state this test establishes, so it belongs
# here and nowhere else.
ibus engine xkb:us::eng >/dev/null 2>&1 || fail "PHASE 4 FAIL: no passthrough engine available"
# Selecting an engine is asynchronous, and ibus swallows keys outright while the
# switch is in flight — so wait for it to report the new engine and then let it
# settle, or the keypress this phase counts never leaves the input method.
for _ in $(seq 1 40); do
    case "$(ibus engine 2>&1 || true)" in
        xkb:us::eng) break ;;
    esac
    sleep 0.5
done
sleep 2
# Spaces are squeezed out of both sides: 我 is a double-width glyph, so the grid
# spends two cells on it and the snapshot reads the trailing spacer cell back as
# a space. Dropping spaces symmetrically keeps the comparison about the letter
# that was typed — a doubled keystroke still reads `zz` and still fails.
BEFORE_LINE=$(pty_text /output/ime-04-before-passthrough.json | tail -1 | tr -d ' ')
tap z
# Wait for the keystroke to echo, then keep waiting: a duplicate arrives on the
# heels of the first, so sampling the moment the line changes would read a
# doubled keystroke as a single one.
for _ in $(seq 1 30); do
    AFTER_LINE=$(pty_text /output/ime-04-passthrough.json | tail -1 | tr -d ' ')
    if [ "$AFTER_LINE" != "$BEFORE_LINE" ]; then
        break
    fi
    sleep 0.5
done
sleep 2
shot /output/ime-04-passthrough.png
AFTER_LINE=$(pty_text /output/ime-04-passthrough.json | tail -1 | tr -d ' ')
if [ "$AFTER_LINE" != "${BEFORE_LINE}z" ]; then
    fail "PHASE 4 FAIL: one keypress produced '$AFTER_LINE' from '$BEFORE_LINE' (expected one 'z')"
fi
echo "PHASE 4 PASS: a passthrough keystroke reached the PTY exactly once"

echo ""
echo "PASS: visual IME preedit test"
echo "  Inspect screenshots in test-output/:"
echo "    ime-00-idle.png        — pane before any composition"
echo "    ime-01-composing.png   — preedit overlay while composing 竹手戈"
echo "    ime-03-committed.png   — committed characters echoed by the shell"
echo "    ime-04-passthrough.png — a passthrough key typed exactly once"
