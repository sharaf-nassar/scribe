#!/bin/bash
[ "${SCRIBE_E2E_SANDBOX:-0}" = "1" ] || { echo "FATAL: this script only runs inside the scribe e2e container (use just e2e-func / e2e-visual)." >&2; exit 99; }
set -e

# Env persistence keys everything off the launch (env-envelope) id the client
# sends in `CreateSession`. The harness used to hardcode `None` there, which
# made every session it created inert: no id, no envelope, nothing for an
# env-persistence assertion to ever observe. This test pins the create path.

UUID_RE='^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$'
RESTORE_FAILURE_SNAPSHOT=/output/env-persistence-restore-failure.json
rm -f "$RESTORE_FAILURE_SNAPSHOT"

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

if [ "${SCRIBE_KEYRING:-0}" != "1" ]; then
    echo "PHASE 4 SKIP: encrypted envelope assertions require SCRIBE_KEYRING=1"
    echo "PASS: env-persistence degraded-path test completed"
    exit 0
fi

# ── Phase 4: A production env delta persists as encrypted .envz ──
# The server is already running, but both spawn and hook ingress read this
# config directly. A fresh session therefore gets SCRIBE_ENV_PERSIST=1 and
# emits its post-rc baseline before the export below drives the real
# PROMPT_COMMAND -> hook helper -> EnvChanged -> debounce path.
cat >"$HOME/.config/scribe/config.toml" <<'TOML'
[terminal.env_persistence]
enabled = true
TOML

# Restore staging requires the server's normal desktop-session runtime env.
# Restart only the disposable container server so it inherits both this value
# and the entrypoint's already-running D-Bus/keyring fixture. This happens
# before the persisted delta is produced, so the test never relies on a
# shutdown flush.
export XDG_RUNTIME_DIR="/run/user/$(id -u)"
scribe-test daemon stop
scribe-test server stop
scribe-test server start
scribe-test daemon start

PERSIST_SESSION=$(scribe-test session create)
PERSIST_ENVELOPE=$(scribe-test session envelope-id "$PERSIST_SESSION")
WINDOW=$(scribe-test daemon window-id)
if ! [[ "$PERSIST_ENVELOPE" =~ $UUID_RE ]] || ! [[ "$WINDOW" =~ $UUID_RE ]]; then
    echo "FAIL: persistence coordinates are invalid (window '$WINDOW', envelope '$PERSIST_ENVELOPE')"
    exit 1
fi

scribe-test wait-idle "$PERSIST_SESSION" --ms 500
PERSIST_NAME=SCRIBE_E2E_PERSISTED_VALUE
PERSIST_VALUE=envz-roundtrip-9f27d6a1
scribe-test send "$PERSIST_SESSION" "export $PERSIST_NAME=$PERSIST_VALUE\n"

STATE_HOME=${XDG_STATE_HOME:-$HOME/.local/state}
ENVZ="$STATE_HOME/scribe/restore/env/$WINDOW/$PERSIST_ENVELOPE.envz"
for _ in $(seq 1 100); do
    [ -s "$ENVZ" ] && break
    sleep 0.1
done
if [ ! -s "$ENVZ" ]; then
    echo "FAIL: debounced persistence did not create $ENVZ"
    exit 1
fi
if grep -aFq "$PERSIST_VALUE" "$ENVZ"; then
    echo "FAIL: encrypted envelope contains the exported plaintext value"
    exit 1
fi
if [ "$(stat -c '%a' "$ENVZ")" != "600" ]; then
    echo "FAIL: encrypted envelope permissions are not 600"
    exit 1
fi
echo "PHASE 4 PASS: production debounce wrote a private encrypted .envz"

# ── Phase 5: A fresh session decrypts and applies the envelope ──
# Keep the writer session alive: persistence has no shutdown flush, and closing
# a session also owns envelope lifecycle cleanup.
RESTORED=$(scribe-test session create --env-envelope-id "$PERSIST_ENVELOPE")
scribe-test wait-idle "$RESTORED" --ms 500
scribe-test send "$RESTORED" 'echo "envz-restored=${SCRIBE_E2E_PERSISTED_VALUE:-missing}"\n'
if ! scribe-test wait-output "$RESTORED" "envz-restored=$PERSIST_VALUE"; then
    scribe-test snapshot "$RESTORED" "$RESTORE_FAILURE_SNAPSHOT"
    CELLS=$(grep -oP '"c": "."' "$RESTORE_FAILURE_SNAPSHOT" \
        | cut -d'"' -f4 | tr -d '\n')
    echo "FAIL: restored session output did not contain expected value: $CELLS"
    exit 1
fi
scribe-test session close "$RESTORED"
scribe-test session close "$PERSIST_SESSION"
echo "PHASE 5 PASS: fresh session decrypted and applied the persisted delta"

echo "PASS: env-persistence encrypted round-trip test completed"
