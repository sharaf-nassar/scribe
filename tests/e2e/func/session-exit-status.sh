#!/bin/bash
[ "${SCRIBE_E2E_SANDBOX:-0}" = "1" ] || { echo "FATAL: this script only runs inside the scribe e2e container (use just e2e-func / e2e-visual)." >&2; exit 99; }
set -e

# Spec 017 US1-2: the child-exit watcher reports the real wait status.
# Exit detection used to ride on PTY master EOF, so every SessionExited
# carried exit_code: None and a signal death was indistinguishable from a
# clean exit.

PID_FILE=/tmp/scribe-exit-status-pid
rm -f "$PID_FILE"

# ── Phase 1: real exit code ──────────────────────────────────────
CODE_SESSION=$(scribe-test session create)
scribe-test send "$CODE_SESSION" 'echo exit-status-ready\n'
scribe-test wait-output "$CODE_SESSION" "exit-status-ready"
scribe-test send "$CODE_SESSION" 'exit 42\n'
scribe-test assert-exit "$CODE_SESSION" 42 --timeout 5000
echo "PHASE 1 PASS: 'exit 42' reported as exit code 42"

# ── Phase 2: signal termination ──────────────────────────────────
# `exec` replaces the interactive shell (which ignores SIGTERM) with a
# process that dies on it, without changing the PID the server spawned.
SIG_SESSION=$(scribe-test session create)
scribe-test send "$SIG_SESSION" "echo \$\$ > $PID_FILE; exec sleep 300\n"
for _ in $(seq 1 50); do
    [ -s "$PID_FILE" ] && break
    sleep 0.1
done
CHILD_PID=$(cat "$PID_FILE")
if [ -z "$CHILD_PID" ]; then
    echo "PHASE 2 FAIL: session never reported its shell pid"
    exit 1
fi
scribe-test wait-idle "$SIG_SESSION" --ms 300
kill -TERM "$CHILD_PID"
scribe-test assert-signal "$SIG_SESSION" 15 --timeout 5000
echo "PHASE 2 PASS: SIGTERM reported as signal 15, not as an exit code"

# ── Phase 3: exit while a descendant still holds the slave ───────
# Spec 017 US1-3: the background job inherits the slave fd, so the master
# never ends and an EOF-driven exit path would miss this exit entirely. The
# child-exit watcher waits out its drain grace and reports the shell's real
# status regardless, then cancels the reader the master left orphaned.
HELD_SESSION=$(scribe-test session create)
scribe-test send "$HELD_SESSION" 'echo held-slave-ready\n'
scribe-test wait-output "$HELD_SESSION" "held-slave-ready"
# The subshell keeps the slave fds it inherited and ignores the SIGHUP the
# kernel and `Pty::Drop` aim at the dying session, so it outlives the shell
# no matter how the shell's job control is configured.
scribe-test send "$HELD_SESSION" "(trap '' HUP; sleep 30) &\n"
scribe-test wait-idle "$HELD_SESSION" --ms 300
scribe-test send "$HELD_SESSION" 'exit 9\n'
scribe-test assert-exit "$HELD_SESSION" 9 --timeout 10000
echo "PHASE 3 PASS: exit 9 reported while a descendant still held the slave"

# ── Phase 4: SessionExited stays exactly-once ────────────────────
# Every assertion above already requires a single frame; re-asserting after a
# settle window catches a late duplicate from a second exit path.
sleep 1
scribe-test assert-exit "$CODE_SESSION" 42 --timeout 1000
scribe-test assert-signal "$SIG_SESSION" 15 --timeout 1000
scribe-test assert-exit "$HELD_SESSION" 9 --timeout 1000
echo "PHASE 4 PASS: one SessionExited per session after settling"

rm -f "$PID_FILE"
echo "PASS: session-exit-status test completed"
