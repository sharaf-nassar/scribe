#!/bin/bash
[ "${SCRIBE_E2E_SANDBOX:-0}" = "1" ] || { echo "FATAL: this script only runs inside the scribe e2e container (use just e2e-func / e2e-visual)." >&2; exit 99; }
# e2e-timeout: 120
set -euo pipefail

RECORD=/tmp/claude-invocation.txt
EXPECTED=/tmp/claude-expected-argv.txt
ACTUAL=/tmp/claude-actual-argv.txt
ENVELOPE_ID=ai-launch-smoke-envelope
CONVERSATION_ID="smoke conversation's id"
PI_RECORD=/tmp/pi-invocation.txt
# A directory that stands in for a workspace project root: the client picks one
# with `focused_workspace_project_root`, the harness names the same policy's
# answer explicitly, and either way the server has to spawn Pi inside it.
PI_PROJECT_ROOT=/tmp/pi-project-root
HOOK_SOCK="/run/user/$(id -u)/scribe/server.sock"

rm -f "$RECORD" "$EXPECTED" "$ACTUAL" "$PI_RECORD"
mkdir -p "$PI_PROJECT_ROOT/.git"
# An AI tab starts a plain non-login shell, so the harness stub directory goes
# on PATH from `.bashrc` — the same file every other Scribe tab reads.
printf '%s\n' 'export PATH="/tests/bin:$PATH"' >> "$HOME/.bashrc"

AI_SESSION=$(scribe-test session create \
    --ai-provider claude \
    --ai-resume-mode resume \
    --ai-conversation-id "$CONVERSATION_ID" \
    --cwd /tmp \
    --env-envelope-id "$ENVELOPE_ID")

ACTUAL_ENVELOPE=$(scribe-test session envelope-id "$AI_SESSION")
if [ "$ACTUAL_ENVELOPE" != "$ENVELOPE_ID" ]; then
    echo "FAIL: expected envelope id '$ENVELOPE_ID', got '$ACTUAL_ENVELOPE'"
    exit 1
fi

for _ in $(seq 1 50); do
    [ -f "$RECORD" ] && break
    sleep 0.1
done

if [ ! -f "$RECORD" ]; then
    echo "FAIL: claude stub did not record an invocation"
    exit 1
fi

printf '%s\n' '--resume' "$CONVERSATION_ID" > "$EXPECTED"
sed -n '1,2p' "$RECORD" > "$ACTUAL"
if ! cmp -s "$EXPECTED" "$ACTUAL"; then
    echo "FAIL: claude stub argv did not match"
    echo "expected:"
    sed 's/^/  /' "$EXPECTED"
    echo "actual:"
    sed 's/^/  /' "$ACTUAL"
    exit 1
fi

if [ "$(sed -n '3p' "$RECORD")" != "--ENV--" ]; then
    echo "FAIL: claude stub record is missing the environment delimiter"
    exit 1
fi

if ! grep -qx 'PWD=/tmp' "$RECORD"; then
    echo "FAIL: claude stub did not start in requested cwd /tmp"
    exit 1
fi

# The shell `exec`s the provider over itself, so the provider IS the PTY child
# and its exit is the session's exit. That is the whole reason the AI tab keeps
# an `exec` rather than running the CLI from a resident shell: quitting the AI
# app closes the tab instead of dropping the user at a stray prompt.
scribe-test assert-exit "$AI_SESSION" 0 --timeout 5000

echo "PHASE 1 PASS: AI launch reached claude stub with expected argv, cwd, and envelope id, and the tab exited with it"

# ── Pi launch helpers ────────────────────────────────────────────────────────
# Pi is launch-only in argv terms — `binary_name()` with no resume flag — so
# every phase below reads the same stub record: an empty argv block, then the
# environment the AI tab really started with.
wait_for_pi_record() {
    local attempt=0
    while [ "$attempt" -lt 100 ]; do
        [ -f "$PI_RECORD" ] && return 0
        attempt=$((attempt + 1))
        sleep 0.1
    done
    return 1
}

assert_pi_launch_shape() {
    local phase="$1"
    if [ "$(head -1 "$PI_RECORD")" != "--ENV--" ]; then
        echo "$phase FAIL: pi stub recorded argv before the environment delimiter"
        sed -n '1,5p' "$PI_RECORD"
        exit 1
    fi
    if ! grep -qx "PWD=$PI_PROJECT_ROOT" "$PI_RECORD"; then
        echo "$phase FAIL: pi stub did not start in the requested project root $PI_PROJECT_ROOT"
        exit 1
    fi
    # The extension discovers Scribe through exactly these three variables and
    # no-ops without them, so a Pi tab that omits one is a dead integration.
    for required in SCRIBE_HOOK_HELPER SCRIBE_HOOK_SOCK SCRIBE_SESSION_ID; do
        if ! grep -q "^$required=." "$PI_RECORD"; then
            echo "$phase FAIL: pi stub environment is missing $required"
            exit 1
        fi
    done
    if ! grep -qx "SCRIBE_SESSION_ID=$2" "$PI_RECORD"; then
        echo "$phase FAIL: pi stub does not carry the created session's id"
        exit 1
    fi
}

# One real hook edge on the Pi session, sent the way the packaged extension
# sends it: fixed argv selectors plus a JSON payload on stdin.
pi_hook() {
    local session="$1" event="$2" payload="$3"
    printf '%s' "$payload" | SCRIBE_HOOK_SOCK="$HOOK_SOCK" SCRIBE_SESSION_ID="$session" \
        scribe-hook-helper --provider=pi --event="$event" --payload-stdin
}

pi_chrome_wait() {
    local session="$1" needle="$2" attempt=0
    while [ "$attempt" -lt 60 ]; do
        CHROME=$(scribe-test ai-chrome "$session" 2>/dev/null || true)
        if printf '%s\n' "$CHROME" | grep -q -- "$needle"; then
            return 0
        fi
        attempt=$((attempt + 1))
        sleep 0.1
    done
    return 1
}

end_pi_tab() {
    local session="$1" phase="$2"
    # The stub traps SIGTERM and exits 0. Because the launch `exec`s Pi over the
    # shell, that exit IS the session's exit — quitting Pi closes the tab.
    pkill -TERM -f '/tests/bin/pi' || true
    scribe-test assert-exit "$session" 0 --timeout 10000 \
        || { echo "$phase FAIL: the tab outlived the Pi process"; exit 1; }
}

# ── Phase 2: an unsupported peer launches Pi as the legacy shell tool ────────
# The harness daemon handshakes without `pi_provider`, exactly like a client
# that predates structured Pi. It must still get a real `pi` tab — through
# `ShellTool::Pi` — and must never be sent an `AiProvider::Pi` frame.
rm -f "$PI_RECORD"
PI_LEGACY_SESSION=$(scribe-test session create --ai-provider pi --cwd "$PI_PROJECT_ROOT")
wait_for_pi_record || { echo "PHASE 2 FAIL: legacy Pi launch never reached the pi stub"; exit 1; }
assert_pi_launch_shape "PHASE 2" "$PI_LEGACY_SESSION"

pi_hook "$PI_LEGACY_SESSION" state_changed '{"state":"processing"}'
pi_hook "$PI_LEGACY_SESSION" context_changed '{"fill_percent":33}'
sleep 0.5
CHROME=$(scribe-test ai-chrome "$PI_LEGACY_SESSION" 2>/dev/null || true)
if [ -n "$CHROME" ]; then
    echo "PHASE 2 FAIL: a peer without the Pi capability was sent Pi AI chrome"
    printf 'chrome was:\n%s\n' "$CHROME"
    exit 1
fi
end_pi_tab "$PI_LEGACY_SESSION" "PHASE 2"
echo "PHASE 2 PASS: legacy ShellTool::Pi launched zero-argv pi in the project root, with no Pi frames to the old peer"

# ── Phase 3: a negotiated peer gets the tracked Pi AI session ────────────────
# Same create request, one capability apart: this daemon advertises
# `pi_provider`, so the client half sends `AiLaunchSpec { provider: Pi }` and
# the server tracks the tab as a Pi AI session whose state reaches the peer.
scribe-test daemon stop
SCRIBE_TEST_PI_PROVIDER=1 scribe-test daemon start
rm -f "$PI_RECORD"
PI_SESSION=$(scribe-test session create --ai-provider pi --ai-resume-mode new --cwd "$PI_PROJECT_ROOT")
wait_for_pi_record || { echo "PHASE 3 FAIL: tracked Pi launch never reached the pi stub"; exit 1; }
assert_pi_launch_shape "PHASE 3" "$PI_SESSION"

pi_hook "$PI_SESSION" state_changed '{"state":"processing"}'
pi_hook "$PI_SESSION" context_changed '{"fill_percent":33}'
if ! pi_chrome_wait "$PI_SESSION" "33%"; then
    echo "PHASE 3 FAIL: the negotiated peer never rendered the tracked Pi session's chrome"
    printf 'chrome was:\n%s\n' "$CHROME"
    exit 1
fi
end_pi_tab "$PI_SESSION" "PHASE 3"
echo "PHASE 3 PASS: negotiated AiLaunchSpec { provider: Pi } launched zero-argv pi and reached the peer's AI chrome"

echo "PASS: AI launch smoke covered claude resume argv and both Pi launch representations"
