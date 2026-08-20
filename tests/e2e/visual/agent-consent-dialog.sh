#!/bin/bash
[ "${SCRIBE_E2E_SANDBOX:-0}" = "1" ] || { echo "FATAL: this script only runs inside the scribe e2e container (use just e2e-visual-agent-consent)." >&2; exit 99; }
# @lat: [[test#Test Harness#Visual E2E Tests]]
set -euo pipefail

# shellcheck source=tests/e2e/visual/agent-visual-common.bash
. /tests/visual/agent-visual-common.bash

[ "${SCRIBE_SHARED_PANE:-0}" = "1" ] \
    || fail "agent consent requires the shared-pane visual harness"
command -v scribe >/dev/null 2>&1 || fail "scribe CLI is absent from the visual harness"

WID=$(find_scribe_window)
[ -n "$WID" ] || fail "no Scribe window"
focus_scribe_window "$WID"

# The dialog paint can lag the just-raised window, so retry the capture
# briefly instead of racing it with the shared plain shot.
shot() {
    local path="$1"
    for _ in {1..20}; do
        if import -window "$WID" +repage "$path" 2>/output/agent-consent-import.err \
            && [ -s "$path" ]; then
            return 0
        fi
        sleep 0.1
    done
    fail "could not capture Scribe window: $(cat /output/agent-consent-import.err)"
}

sleep 1
shot /output/agent-consent-before.png

RAISED_BEFORE=$(grep -cF 'raising the agent capability prompt' "$CLIENT_LOG" 2>/dev/null || true)
(
    set +e
    SCRIBE_SESSION_ID="$SESSION" RUST_LOG=off \
        scribe agent --agent agent-consent-e2e read "$SESSION" \
        >/output/agent-consent-result.json 2>/output/agent-consent.stderr
    printf '%s\n' "$?" >/output/agent-consent-status
) &
CLI_PID=$!

RAISED_AFTER=$RAISED_BEFORE
for _ in {1..100}; do
    RAISED_AFTER=$(grep -cF 'raising the agent capability prompt' "$CLIENT_LOG" 2>/dev/null || true)
    [ "$RAISED_AFTER" -gt "$RAISED_BEFORE" ] && break
    sleep 0.02
done
[ "$RAISED_AFTER" -gt "$RAISED_BEFORE" ] \
    || fail "the real AgentPromptRequest never raised its modal"

DIALOG_DELTA=0
for _ in {1..20}; do
    shot /output/agent-consent-dialog.png
    DIALOG_DELTA=$(delta /output/agent-consent-before.png /output/agent-consent-dialog.png)
    [ "${DIALOG_DELTA:-0}" -ge 500 ] && break
    sleep 0.05
done
[ "${DIALOG_DELTA:-0}" -ge 500 ] \
    || fail "agent consent dialog changed only $DIALOG_DELTA pixels"

# Escape must choose the safe default and resolve the parked CLI call as denied.
xdotool key --clearmodifiers Escape
wait "$CLI_PID"
[ "$(cat /output/agent-consent-status)" = "1" ] \
    || fail "Escape did not return the typed denial: $(cat /output/agent-consent-result.json)"
grep -q '"code":"denied"' /output/agent-consent-result.json \
    || fail "Escape response was not denied"
if grep -q '"screen"\|"text"' /output/agent-consent-result.json; then
    fail "consent denial disclosed terminal content"
fi

echo "PASS: Scribe-owned agent consent dialog painted ($DIALOG_DELTA px) and Escape denied"
echo "  Screenshot: /output/agent-consent-dialog.png"
