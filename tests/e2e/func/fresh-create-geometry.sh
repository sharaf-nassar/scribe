#!/bin/bash
[ "${SCRIBE_E2E_SANDBOX:-0}" = "1" ] || { echo "FATAL: this script only runs inside the scribe e2e container (use just e2e-func / e2e-visual)." >&2; exit 99; }
set -e

# A fresh session is created *and attached* by one `CreateSession`: the server
# installs the requesting connection's sink while it starts the session and
# spawns the PTY at the grid the request named. Anything the client adds on top
# — a second attach, a resize onto a placeholder grid — is redundant work the
# foreground process pays for in SIGWINCHes.
#
# This test pins the three halves of that contract the harness can observe:
# the PTY really starts on the requested grid, the create is never answered with
# a SessionReplay, and re-attaching a pane at its own geometry costs no signal
# at all while a genuine geometry change costs exactly one.

COLS=100
ROWS=30

# ── Phase 1: the PTY starts on the grid the create asked for ──────
NEW=$(scribe-test session create --cols "$COLS" --rows "$ROWS")
scribe-test send "$NEW" 'stty size\n'
scribe-test wait-output "$NEW" "$ROWS $COLS"
echo "PHASE 1 PASS: fresh session spawned at ${COLS}x${ROWS}"

# ── Phase 2: a create is not answered with a replay ───────────────
# There is nothing to replay — the terminal has emitted nothing yet — and the
# redundant full-state replay could overwrite the shell's own startup bytes.
scribe-test replay status "$NEW" --expect-frames 0
echo "PHASE 2 PASS: fresh create sent no SessionReplay"

# ── Phase 3: an in-pane size reporter sees no shrink/regrow ───────
# The trap counts every SIGWINCH the foreground shell receives from here on.
scribe-test send "$NEW" "winch=0; trap 'winch=\$((winch+1))' WINCH\n"
scribe-test wait-idle "$NEW" --ms 300
scribe-test session attach "$NEW" --cols "$COLS" --rows "$ROWS"
scribe-test wait-idle "$NEW" --ms 500
scribe-test send "$NEW" 'echo "winch-a=$winch rc=$(stty size)"\n'
scribe-test wait-output "$NEW" "winch-a=0 rc=$ROWS $COLS"
echo "PHASE 3 PASS: attaching at the pane's own grid raised no SIGWINCH"

# ── Phase 4: that attach really did replay, and one grid change is
#            one signal ───────────────────────────────────────────
# Asserting the attach replayed is what keeps phase 2 from passing vacuously.
scribe-test replay status "$NEW" --min-frames 1
scribe-test resize "$NEW" 90 25
scribe-test wait-idle "$NEW" --ms 500
scribe-test send "$NEW" 'echo "winch-b=$winch rc=$(stty size)"\n'
scribe-test wait-output "$NEW" "winch-b=1 rc=25 90"
echo "PHASE 4 PASS: one geometry change raised exactly one SIGWINCH"

scribe-test session close "$NEW"
echo "PASS: fresh-create geometry test completed"
