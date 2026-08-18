#!/bin/bash
[ "${SCRIBE_E2E_SANDBOX:-0}" = "1" ] || { echo "FATAL: this script only runs inside the scribe e2e container (use just e2e-func)." >&2; exit 99; }
# e2e-timeout: 180
set -euo pipefail

# =============================================================================
# Pi AI lifecycle — functional E2E test
#
# Drives a real tracked Pi session through the same helper contract the
# packaged extension emits — `--provider=pi --event=<name> --payload-stdin`
# with a JSON document on stdin — and asserts what the server made of it.
# `tests/e2e/func/pi-extension-harness.mjs` (`just e2e-pi-extension-harness`)
# owns the other half: that `dist/pi-extension.ts` produces exactly this
# stream, in this order, with no permission event, and no emission from a
# `PI_SUBAGENT_CHILD=1` child. This script owns everything downstream of the
# helper's argv:
#
#   1. a tracked Pi tab really runs `pi` and carries hook discovery env;
#   2. Processing plus prompt, task label, and a clamped context percentage
#      reach a Pi-capable peer's AI chrome;
#   3. a settled question classifies as WaitingForInput (the pulse suppresses
#      the tab suffix) and a settled statement classifies as IdlePrompt;
#   4. an error edge replaces the attention pulse rather than adding one;
#   5. `state_cleared` drops the chrome;
#   6. an abrupt Pi death (SIGKILL, no `session_shutdown`) ends the tab and
#      leaves no live session a late Pi event could re-arm;
#   7. a peer that never advertised `pi_provider` — a server-only upgrade —
#      receives no Pi frame at all while the same events still reach the
#      server.
#
# Pi never emits `PermissionPrompt`: no phase below sends one, and the pulse
# assertions would catch it if the pipeline invented one.
# =============================================================================

HOOK_SOCK="/run/user/$(id -u)/scribe/server.sock"
SERVER_LOG=/output/pi-ai-lifecycle-server.log
PI_RECORD=/tmp/pi-invocation.txt
PI_PROJECT_ROOT=/tmp/pi-lifecycle-root

CHROME=""

fail() {
    echo "$1"
    [ -n "${2:-}" ] && printf 'chrome was:\n%s\n' "$2"
    echo "--- server log tail ---"
    tail -30 "$SERVER_LOG" 2>/dev/null || true
    exit 1
}

# One real hook edge, shaped exactly like the extension's `invoke`.
pi_hook() {
    local session="$1" event="$2" payload="$3"
    printf '%s' "$payload" | SCRIBE_HOOK_SOCK="$HOOK_SOCK" SCRIBE_SESSION_ID="$session" \
        scribe-hook-helper --provider=pi --event="$event" --payload-stdin
}

chrome_wait() {
    local session="$1" needle="$2" attempt=0
    while [ "$attempt" -lt 80 ]; do
        CHROME=$(scribe-test ai-chrome "$session" 2>/dev/null || true)
        if printf '%s\n' "$CHROME" | grep -q -- "$needle"; then
            return 0
        fi
        attempt=$((attempt + 1))
        sleep 0.1
    done
    return 1
}

# `scribe-test ai-chrome` prints the prompt-bar meter and, unless the state
# pulses for attention, the tab-inline suffix. Absence of the tab line is
# therefore the client-visible signature of WaitingForInput.
chrome_tab_line() {
    scribe-test ai-chrome "$1" 2>/dev/null | sed -n 's/^tab://p'
}

chrome_tab_wait() {
    local session="$1" want="$2" attempt=0 tab
    while [ "$attempt" -lt 80 ]; do
        tab=$(chrome_tab_line "$session")
        if [ "$want" = "absent" ] && [ -z "$tab" ]; then
            return 0
        fi
        if [ "$want" != "absent" ] && [ "$tab" = "$want" ]; then
            return 0
        fi
        attempt=$((attempt + 1))
        sleep 0.1
    done
    CHROME=$(scribe-test ai-chrome "$session" 2>/dev/null || true)
    return 1
}

wait_for_pi_record() {
    local attempt=0
    while [ "$attempt" -lt 100 ]; do
        [ -f "$PI_RECORD" ] && return 0
        attempt=$((attempt + 1))
        sleep 0.1
    done
    return 1
}

count_unknown_session_drops() {
    grep -c "hook event for unknown session" "$SERVER_LOG" 2>/dev/null || true
}

# ── Phase 0: a Pi-capable peer over a server that logs its ingress ───────────
# The entrypoint's server discards its output and its daemon handshakes as an
# old client, so both are replaced: the server now writes a log the ingress
# assertions read, and the daemon advertises `pi_provider` the way the real
# GPUI client does.
mkdir -p "$PI_PROJECT_ROOT"
rm -f "$PI_RECORD"
printf '%s\n' 'export PATH="/tests/bin:$PATH"' >> "$HOME/.bashrc"
scribe-test daemon stop
scribe-test server stop
: >"$SERVER_LOG"
export SCRIBE_TEST_SERVER_LOG="$SERVER_LOG"
scribe-test server start
SCRIBE_TEST_PI_PROVIDER=1 scribe-test daemon start

PI_SESSION=$(scribe-test session create --ai-provider pi --ai-resume-mode new --cwd "$PI_PROJECT_ROOT")
wait_for_pi_record || fail "PHASE 0 FAIL: the tracked Pi launch never reached the pi stub"
[ "$(head -1 "$PI_RECORD")" = "--ENV--" ] \
    || fail "PHASE 0 FAIL: pi was launched with argv of its own"
grep -qx "SCRIBE_SESSION_ID=$PI_SESSION" "$PI_RECORD" \
    || fail "PHASE 0 FAIL: the Pi tab does not carry its own session id"
grep -q '^SCRIBE_HOOK_HELPER=.' "$PI_RECORD" \
    || fail "PHASE 0 FAIL: the Pi tab has no helper path to emit through"
echo "PHASE 0 PASS: a tracked Pi tab runs zero-argv pi with hook discovery env"

# ── Phase 1: Processing, prompt, task label, and context reach the chrome ────
# The extension's real-input burst, in its documented order. The percentage is
# deliberately out of range: the extension clamps, and the server clamps again,
# so the meter must read 100% rather than wrap or drop the edge.
pi_hook "$PI_SESSION" state_changed '{"state":"processing"}'
pi_hook "$PI_SESSION" prompt_received '{"text":"Prove the Pi lifecycle end to end"}'
pi_hook "$PI_SESSION" task_label_changed '{"label":"Prove the Pi lifecycle end to end"}'
pi_hook "$PI_SESSION" context_changed '{"fill_percent":42}'
chrome_wait "$PI_SESSION" "42%" \
    || fail "PHASE 1 FAIL: Processing plus context never rendered as 42%" "$CHROME"
chrome_tab_wait "$PI_SESSION" " 42%" \
    || fail "PHASE 1 FAIL: Processing must not suppress the tab context suffix" "$CHROME"
pi_hook "$PI_SESSION" context_changed '{"fill_percent":140}'
chrome_wait "$PI_SESSION" "100%" \
    || fail "PHASE 1 FAIL: an over-range context was not clamped to 100%" "$CHROME"
echo "PHASE 1 PASS: Processing, prompt, task label, and a clamped context reached the peer"

# ── Phase 2: settled work classifies as WaitingForInput, then IdlePrompt ─────
# `agent_settled` carries no state: the server's provider-independent stop
# classifier reads the retained final assistant message. A question pulses for
# attention, which is what withholds the tab suffix; a statement does not.
pi_hook "$PI_SESSION" session_stopped '{"last_message":"Which option should I use?"}'
chrome_tab_wait "$PI_SESSION" absent \
    || fail "PHASE 2 FAIL: a settled question did not classify as WaitingForInput" "$CHROME"
CHROME=$(scribe-test ai-chrome "$PI_SESSION" 2>/dev/null || true)
printf '%s\n' "$CHROME" | grep -q "100%" \
    || fail "PHASE 2 FAIL: the attention state dropped the retained context" "$CHROME"
pi_hook "$PI_SESSION" session_stopped '{"last_message":"Done."}'
chrome_tab_wait "$PI_SESSION" " 100%" \
    || fail "PHASE 2 FAIL: a settled statement did not classify as a non-pulsing IdlePrompt" "$CHROME"
echo "PHASE 2 PASS: a question settled to WaitingForInput and a statement to IdlePrompt"

# ── Phase 3: an error edge replaces the attention pulse ──────────────────────
pi_hook "$PI_SESSION" session_stopped '{"last_message":"Should I retry?"}'
chrome_tab_wait "$PI_SESSION" absent \
    || fail "PHASE 3 FAIL: the attention pulse never armed before the error edge" "$CHROME"
pi_hook "$PI_SESSION" state_changed '{"state":"error"}'
chrome_tab_wait "$PI_SESSION" " 100%" \
    || fail "PHASE 3 FAIL: the error edge never replaced the attention pulse" "$CHROME"
echo "PHASE 3 PASS: an unambiguous error edge replaced the attention state"

# ── Phase 4: session_shutdown's clear drops the chrome ───────────────────────
pi_hook "$PI_SESSION" state_cleared '{}'
for _ in $(seq 1 60); do
    CHROME=$(scribe-test ai-chrome "$PI_SESSION" 2>/dev/null || true)
    [ -z "$CHROME" ] && break
    sleep 0.1
done
[ -z "$CHROME" ] || fail "PHASE 4 FAIL: state_cleared left AI chrome behind" "$CHROME"
echo "PHASE 4 PASS: state_cleared dropped the Pi chrome"

# ── Phase 5: an abrupt Pi death ends the tab and the session ─────────────────
# SIGKILL skips `session_shutdown` entirely, so nothing clears the state from
# inside Pi. Existing session teardown has to do it: the tab dies with Pi, and
# the server forgets the session, which is what stops a late event from
# re-arming a stale Processing indicator.
pi_hook "$PI_SESSION" state_changed '{"state":"processing"}'
pi_hook "$PI_SESSION" context_changed '{"fill_percent":61}'
chrome_wait "$PI_SESSION" "61%" \
    || fail "PHASE 5 FAIL: the pre-death Processing state never armed" "$CHROME"
DROPS_BEFORE=$(count_unknown_session_drops)
pkill -KILL -f '/tests/bin/pi' || true
scribe-test assert-signal "$PI_SESSION" 9 --timeout 10000 \
    || fail "PHASE 5 FAIL: the tab did not die with the killed Pi process"
pi_hook "$PI_SESSION" state_changed '{"state":"processing"}'
DROPPED=0
for _ in $(seq 1 60); do
    [ "$(count_unknown_session_drops)" -gt "$DROPS_BEFORE" ] && { DROPPED=1; break; }
    sleep 0.1
done
[ "$DROPPED" -eq 1 ] \
    || fail "PHASE 5 FAIL: the server still accepted Pi state for the dead session"
echo "PHASE 5 PASS: an abrupt Pi death ended the tab and left no session to re-arm"

# ── Phase 6: a server-only upgrade stays silent toward an old peer ───────────
# Same server, same events; only the peer changes. A daemon that handshakes
# without `pi_provider` is the running old client of a server-only upgrade, so
# it must be sent no Pi frame while the server still ingests every event.
scribe-test daemon stop
scribe-test daemon start
OLD_PEER_SESSION=$(scribe-test session create --ai-provider pi --cwd "$PI_PROJECT_ROOT")
DROPS_BEFORE=$(count_unknown_session_drops)
pi_hook "$OLD_PEER_SESSION" state_changed '{"state":"processing"}'
pi_hook "$OLD_PEER_SESSION" prompt_received '{"text":"Old peer must not see this"}'
pi_hook "$OLD_PEER_SESSION" task_label_changed '{"label":"Old peer must not see this"}'
pi_hook "$OLD_PEER_SESSION" context_changed '{"fill_percent":77}'
sleep 1
CHROME=$(scribe-test ai-chrome "$OLD_PEER_SESSION" 2>/dev/null || true)
[ -z "$CHROME" ] \
    || fail "PHASE 6 FAIL: an old peer was sent Pi AI chrome" "$CHROME"
[ "$(count_unknown_session_drops)" -eq "$DROPS_BEFORE" ] \
    || fail "PHASE 6 FAIL: the server dropped the old peer's Pi events instead of ingesting them"
pkill -TERM -f '/tests/bin/pi' || true
scribe-test assert-exit "$OLD_PEER_SESSION" 0 --timeout 10000 \
    || fail "PHASE 6 FAIL: the old peer's Pi tab outlived its process"
echo "PHASE 6 PASS: a server-only upgrade sent an old peer no Pi enums or events"

echo "PASS: Pi AI lifecycle reached the server over the real helper path"
