#!/bin/bash
set -e

# In a shared window (feature 015, D3) a client `Resize` is an informational
# per-participant viewport report, not a direct grid-set: the server folds every
# attached viewport into the smallest-wins authoritative grid and drives one
# `TIOCSWINSZ` once the reports settle. A drag reports continuously, so the
# debounce is what decides whether the shell pays one SIGWINCH or one per event.
#
# This test drives a drag that lasts LONGER than one debounce window — the case
# an uncancelled per-report timer gets wrong, because each timer matures 250ms
# behind its own report and applies whatever the grid has shrunk to by then. It
# pins the two halves a trailing debounce owes: the whole drag costs the
# foreground process exactly ONE SIGWINCH, and the grid it lands on is the LAST
# reported size — a trailing apply, not a stream of mid-drag ones.

ROWS=34
FINAL_COLS=78

# ── Phase 1: put the window in a shared mode ──────────────────────
# `SingleController` (the default) keeps the legacy controller-gated direct
# grid-set, where `Resize` never reaches the viewport path at all. The mode is
# snapshotted onto the share when the connection claims its window, so the
# config has to be in place before the harness daemon says Hello.
scribe-test daemon stop
scribe-test server stop
mkdir -p "$HOME/.config/scribe"
cat > "$HOME/.config/scribe/config.toml" <<'EOF'
[remote]
sharing_mode = "free_for_all"
EOF
scribe-test server start
scribe-test daemon start
echo "PHASE 1 PASS: server restarted with free-for-all sharing"

# ── Phase 2: a pane that counts its own SIGWINCHes ────────────────
PANE=$(scribe-test session create --cols 110 --rows "$ROWS")
scribe-test send "$PANE" "winch=0; trap 'winch=\$((winch+1))' WINCH\n"
scribe-test wait-idle "$PANE" --ms 500
echo "PHASE 2 PASS: pane armed at 110x${ROWS}"

# ── Phase 3: a drag that outlives the debounce window ─────────────
# Reports stay closer together than the 250ms debounce, so no window ever
# completes mid-drag, but the drag as a whole runs several windows long.
START_MS=$(($(date +%s%N) / 1000000))
for cols in 108 106 104 102 100 98 96 94 92 90 88 86 84 82 80 "$FINAL_COLS"; do
    scribe-test resize "$PANE" "$cols" "$ROWS"
    sleep 0.05
done
ELAPSED_MS=$(($(date +%s%N) / 1000000 - START_MS))
echo "PHASE 3: 16 viewport reports spanned ${ELAPSED_MS}ms (debounce is 250ms)"
if [ "$ELAPSED_MS" -le 250 ]; then
    echo "FAIL: the drag fit inside one debounce window; nothing was restarted"
    exit 1
fi

# ── Phase 4: one trailing apply, at the last reported size ────────
scribe-test wait-idle "$PANE" --ms 800
scribe-test send "$PANE" 'echo "drag winch=$winch rc=$(stty size)"\n'
scribe-test wait-output "$PANE" "drag winch=1 rc=$ROWS $FINAL_COLS"
echo "PHASE 4 PASS: the drag settled to a single apply at ${FINAL_COLS}x${ROWS}"

scribe-test session close "$PANE"
echo "PASS: viewport-report debounce test completed"
