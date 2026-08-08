#!/bin/bash
[ "${SCRIBE_E2E_SANDBOX:-0}" = "1" ] || { echo "FATAL: this script only runs inside the scribe e2e container (use just e2e-func / e2e-visual)." >&2; exit 99; }
set -euo pipefail

RECORD=/tmp/claude-invocation.txt
EXPECTED=/tmp/claude-expected-argv.txt
ACTUAL=/tmp/claude-actual-argv.txt
ENVELOPE_ID=ai-launch-smoke-envelope
CONVERSATION_ID="smoke conversation's id"

rm -f "$RECORD" "$EXPECTED" "$ACTUAL"
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

echo "PASS: AI launch reached claude stub with expected argv, cwd, and envelope id, and the tab exited with it"
