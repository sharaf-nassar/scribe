#!/bin/bash
[ "${SCRIBE_E2E_SANDBOX:-0}" = "1" ] || { echo "FATAL: this script only runs inside the scribe e2e container (use just e2e-func / e2e-visual)." >&2; exit 99; }
set -e

# Env persistence keys everything off the launch (env-envelope) id the client
# sends in `CreateSession`. The harness used to hardcode `None` there, which
# made every session it created inert: no id, no envelope, nothing for an
# env-persistence assertion to ever observe. This test pins the create path.

UUID_RE='^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$'

# ── Phase 1: The entrypoint's session carries an envelope id ─────
scribe-test wait-idle "$SESSION" --ms 500

ENVELOPE=$(scribe-test session envelope-id "$SESSION")
if ! [[ "$ENVELOPE" =~ $UUID_RE ]]; then
    echo "FAIL: session create path reported no envelope id (got '$ENVELOPE')"
    exit 1
fi
echo "PHASE 1 PASS: create path minted envelope id $ENVELOPE"

# ── Phase 2: Ids are per-launch, not a shared constant ───────────
SECOND=$(scribe-test session create)
SECOND_ENVELOPE=$(scribe-test session envelope-id "$SECOND")
if ! [[ "$SECOND_ENVELOPE" =~ $UUID_RE ]]; then
    echo "FAIL: second create path reported no envelope id (got '$SECOND_ENVELOPE')"
    exit 1
fi
if [ "$ENVELOPE" = "$SECOND_ENVELOPE" ]; then
    echo "FAIL: two launches share envelope id $ENVELOPE"
    exit 1
fi
echo "PHASE 2 PASS: second launch minted a distinct envelope id $SECOND_ENVELOPE"

# ── Phase 3: The session the id belongs to actually runs ─────────
scribe-test send "$SECOND" 'echo envelope-session-alive\n'
scribe-test wait-output "$SECOND" "envelope-session-alive"
scribe-test session close "$SECOND"
echo "PHASE 3 PASS: the launch behind the envelope id is a live session"

echo "PASS: env-persistence create-path test completed"
