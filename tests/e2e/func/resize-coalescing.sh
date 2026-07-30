#!/bin/bash
set -e

# In a `SingleController` window (the default mode) a client `Resize` is a direct
# grid-set: the server reflows the session's `Term` and drives `TIOCSWINSZ`, and
# the child pays a `SIGWINCH`. A drag republishes the pane's grid every frame, so
# without pacing the reflows run at event rate — dozens of full reflows and
# dozens of signals for one gesture.
#
# This test drives a drag long enough to span several pacing intervals and pins
# both halves of the contract: the applies settle to at most four per second, and
# the drag still lands on the size it stopped at rather than on a mid-drag one.
# The in-pane `trap` is the oracle because the intermediate grids leave no trace
# in any screen snapshot — the pane ends exactly where the last report put it.

ROWS=34
FIRST_COLS=118
FINAL_COLS=79
REPORTS=40
INTERVAL_MS=250

# ── Phase 1: a pane that counts its own SIGWINCHes ────────────────
PANE=$(scribe-test session create --cols 120 --rows "$ROWS")
scribe-test send "$PANE" "winch=0; trap 'winch=\$((winch+1))' WINCH\n"
scribe-test wait-idle "$PANE" --ms 500
echo "PHASE 1 PASS: pane armed at 120x${ROWS}"

# ── Phase 2: a continuous drag, one column per step ───────────────
START_MS=$(($(date +%s%N) / 1000000))
for cols in $(seq "$FIRST_COLS" -1 "$FINAL_COLS"); do
    scribe-test resize "$PANE" "$cols" "$ROWS"
    sleep 0.03
done
ELAPSED_MS=$(($(date +%s%N) / 1000000 - START_MS))
echo "PHASE 2: ${REPORTS} resize reports spanned ${ELAPSED_MS}ms"

# The pacer applies leading-edge and then no more often than one interval, with
# the last report held as the trailing apply — so the whole drag can cost at
# most one apply per interval it spans, plus that leading one.
MAX_APPLIES=$(((ELAPSED_MS + INTERVAL_MS) / INTERVAL_MS + 1))
if [ "$MAX_APPLIES" -ge "$REPORTS" ]; then
    echo "FAIL: the drag ran too slowly (${ELAPSED_MS}ms) to distinguish"
    echo "      pacing from applying every report"
    exit 1
fi
echo "PHASE 2 PASS: ${REPORTS} reports may cost at most ${MAX_APPLIES} applies"

# ── Phase 3: the applies settled, at the last reported size ───────
scribe-test wait-idle "$PANE" --ms 800
scribe-test send "$PANE" "if [ \$winch -ge 1 ] && [ \$winch -le $MAX_APPLIES ]; then v=PASS; else v=FAIL; fi\n"
scribe-test wait-idle "$PANE" --ms 300
scribe-test send "$PANE" 'echo "drag winch=$winch verdict=$v rc=$(stty size)"\n'
scribe-test wait-output "$PANE" "verdict=PASS rc=$ROWS $FINAL_COLS"
echo "PHASE 3 PASS: the drag settled to <=${MAX_APPLIES} applies at ${FINAL_COLS}x${ROWS}"

scribe-test session close "$PANE"
echo "PASS: resize coalescing test completed"
