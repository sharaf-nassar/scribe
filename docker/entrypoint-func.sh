#!/bin/bash
set -euo pipefail

export SCRIBE_E2E_SANDBOX=1
export PATH="/tests/bin:$PATH"

# Start a session D-Bus and an unlocked gnome-keyring, then re-exec.
#
# Env-envelope DEKs are sealed through Secret Service on Linux. Keep the
# fixture opt-in so functional tests that do not need a keyring retain the
# lighter startup path.
start_session_keyring() {
    if [ -n "${DBUS_SESSION_BUS_ADDRESS:-}" ]; then
        return 0
    fi
    export $(dbus-launch)
    # `--unlock` reads the (empty) password on stdin and prints the environment
    # of the daemon it started; only the secrets component is needed.
    eval "$(printf '\n' | gnome-keyring-daemon --unlock --components=secrets)"
    export GNOME_KEYRING_CONTROL
}

cleanup() {
    scribe-test daemon stop || true
    scribe-test server stop || true
}
trap cleanup EXIT

UID_DIR="/run/user/$(id -u)/scribe"
mkdir -p "$UID_DIR"
chmod 700 "$UID_DIR"

# Ensure config directory exists so the file watcher can be initialised.
mkdir -p "${HOME}/.config/scribe"

if [ "${SCRIBE_KEYRING:-0}" = "1" ]; then
    start_session_keyring
fi

scribe-test server start
scribe-test daemon start

SESSION=$(scribe-test session create)
export SESSION

EXIT_CODE=0
timeout "${TEST_TIMEOUT:-30}" "$1" 2>&1 | tee /output/result.log || EXIT_CODE=$?

exit $EXIT_CODE
