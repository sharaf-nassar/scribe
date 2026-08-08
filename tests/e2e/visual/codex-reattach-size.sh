#!/bin/bash
[ "${SCRIBE_E2E_SANDBOX:-0}" = "1" ] || { echo "FATAL: this script only runs inside the scribe e2e container (use just e2e-func / e2e-visual)." >&2; exit 99; }
set -euo pipefail

# @lat: [[test#Visual E2E Tests#Codex reattach announces no grid]]
# Scripted E2E: the Codex 0x0 exception on a live window's reconnect.
#
# The shared-pane rig makes $SESSION the pane this exact window renders, and
# `scribe-test server upgrade` replaces that window's connection without
# restarting the client — the one reconnect in the harness that reaches
# `reattach_visible_sessions` with a non-empty attached set.
#
# TWO LIMITATIONS, both deliberate:
#   * There is no Codex binary in the image. The provider is faked through the
#     hook channel exactly as tests/e2e/func/ai-context-thresholds.sh fakes it,
#     so nothing here proves Ink's repaint behaviour — only that the client
#     announces what the exception says it must.
#   * The oracle is the client log, not the share tap. The tap moves
#     $UID_DIR/server.sock aside to interpose itself (docker/entrypoint-visual.sh
#     `start_share_tap`), and a handed-off server binds that same fixed path
#     (crates/scribe-common/src/socket.rs), so tap + upgrade cannot coexist.
#
# Requires: visual container with SCRIBE_SHARED_PANE=1.

CLIENT_LOG="${SCRIBE_CLIENT_LOG:-/output/client.log}"
SESSION="${SESSION:?the entrypoint must export a created SESSION}"
# `SessionId`'s Display is the first uuid group behind a "session-" prefix
# (crates/scribe-common/src/ids.rs), which is what the log carries.
SHORT="session-${SESSION:0:8}"

# tracing colours every FIELD NAME, so a literal `cols=` only matches once the
# escapes are stripped; message prose is uncoloured either way.
plain_client_log() {
    sed 's/\x1b\[[0-9;]*m//g' "$CLIENT_LOG"
}

count_client() {
    plain_client_log | grep -cF "$1" || true
}

wait_for_count() {
    local needle="$1" minimum="$2" deadline=$((SECONDS + 30))
    while [ "$SECONDS" -lt "$deadline" ]; do
        if [ "$(count_client "$needle")" -ge "$minimum" ]; then
            return 0
        fi
        sleep 0.2
    done
    return 1
}

fail() {
    echo "$1" >&2
    echo "--- attach lines ---" >&2
    plain_client_log | grep -F "attaching to session" | tail -5 >&2 || true
    echo "--- published grid lines ---" >&2
    plain_client_log | grep -F "published a pane's grid size" | tail -5 >&2 || true
    exit 1
}

# ── Phase 1: make the server call this pane a Codex session ──────────────────
# The shell parks in `read` so no returning prompt (OSC 133;A) clears the AI
# state before the reconnect reads it back off the SessionList.
scribe-test send "$SESSION" "scribe-hook-helper --provider=codex_code --event=state_changed --state=processing; scribe-hook-helper --provider=codex_code --event=context_changed --fill-percent=51; read -r\n"
CHROME=""
for _ in $(seq 1 80); do
    CHROME=$(scribe-test ai-chrome "$SESSION" 2>/dev/null || true)
    printf '%s\n' "$CHROME" | grep -q '51%' && break
    sleep 0.2
done
printf '%s\n' "$CHROME" | grep -q '51%' \
    || fail "PHASE 1 FAIL: the codex_code hook events never reached the server"
echo "PHASE 1 PASS: the server holds a codex_code AI state for $SHORT"

# ── Phase 2: force the live window onto a replacement connection ─────────────
PUBLISHED_BEFORE=$(count_client "published a pane's grid size")
ATTACHES_BEFORE=$(count_client "attaching to session")
scribe-test server upgrade
wait_for_count "attaching to session" "$((ATTACHES_BEFORE + 1))" \
    || fail "PHASE 2 FAIL: the window never reattached after the handoff"
kill -0 "$SCRIBE_CLIENT_PID"
echo "PHASE 2 PASS: the original window reattached over a new connection"

# ── Phase 3: the reattach announced no grid and sent no resize ───────────────
LINE=$(plain_client_log | grep -F "attaching to session" | grep -F "session_id=$SHORT" | tail -1 || true)
[ -n "$LINE" ] || fail "PHASE 3 FAIL: no attach line names $SHORT"
case "$LINE" in
    *"cols=0 rows=0"*) ;;
    *) fail "PHASE 3 FAIL: the Codex pane attached pre-sized: $LINE" ;;
esac
case "$LINE" in
    *"resize_now=false"*) ;;
    *) fail "PHASE 3 FAIL: a resize rode in behind the Codex attach: $LINE" ;;
esac
echo "PHASE 3 PASS: the reattach announced 0x0 and skipped the follow-up resize"

# ── Phase 4: the real grid still arrives, after the replay ───────────────────
# The attach deliberately said nothing about geometry, so the pane's cached
# size is dropped and the next publish re-sends it as an ordinary resize.
wait_for_count "published a pane's grid size" "$((PUBLISHED_BEFORE + 1))" \
    || fail "PHASE 4 FAIL: the deferred grid was never republished"
GRID=$(plain_client_log | grep -F "published a pane's grid size" | tail -1 || true)
case "$GRID" in
    *"cols=0 "*) fail "PHASE 4 FAIL: the republished grid is empty: $GRID" ;;
esac
echo "PHASE 4 PASS: the pane's real grid followed as an ordinary resize"

echo "PASS: a reattached Codex pane announces no grid and republishes it after the replay"
