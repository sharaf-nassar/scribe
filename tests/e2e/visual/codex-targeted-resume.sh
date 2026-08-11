#!/bin/bash
[ "${SCRIBE_E2E_SANDBOX:-0}" = "1" ] || { echo "FATAL: this script only runs inside the scribe e2e container (use just e2e-visual)." >&2; exit 99; }
set -euo pipefail

# @lat: [[test#Visual E2E Tests#Codex metadata survives update and cold replay]]
# Real-client regression for update/restart losing Codex's structured launch.

CLIENT_LOG="${SCRIBE_CLIENT_LOG:-/output/client.log}"
RECORD="${SCRIBE_AI_STUB_RECORD:?the recipe must set SCRIBE_AI_STUB_RECORD}"
RESTORE_DIR="${XDG_STATE_HOME:?the entrypoint must export XDG_STATE_HOME}/scribe/restore/windows"
HOOK_SOCK="${SCRIBE_RUNTIME_DIR:?the entrypoint must export SCRIBE_RUNTIME_DIR}/server.sock"
CONVERSATION_ID="codex-targeted-resume-e2e"
STUB_BIN=/tmp/codex-targeted-resume-bin

fail() {
    echo "FAIL: $1" >&2
    echo "--- invocations ---" >&2
    cat "$RECORD" >&2 2>/dev/null || true
    echo "--- client log tail ---" >&2
    tail -80 "$CLIENT_LOG" >&2 2>/dev/null || true
    echo "--- restore snapshots ---" >&2
    grep -R -E 'launch_id|kind|provider|resume_mode|conversation_id' "$RESTORE_DIR" >&2 2>/dev/null || true
    exit 1
}

plain_client_log() { sed 's/\x1b\[[0-9;]*m//g' "$CLIENT_LOG"; }
count_log() { plain_client_log | grep -cF "$1" || true; }
invocation_count() { grep -c '^BEGIN$' "$RECORD" 2>/dev/null || true; }

wait_for_count() {
    local command="$1" minimum="$2" timeout_secs="$3"
    local deadline=$((SECONDS + timeout_secs))
    while [ "$SECONDS" -lt "$deadline" ]; do
        [ "$(eval "$command")" -ge "$minimum" ] && return 0
        sleep 0.2
    done
    return 1
}

wait_for_client_exit() {
    local deadline=$((SECONDS + 20))
    while pgrep -f 'scribe-client' >/dev/null 2>&1; do
        [ "$SECONDS" -ge "$deadline" ] && return 1
        sleep 0.2
    done
}

wait_for_server_exit() {
    local deadline=$((SECONDS + 20))
    while pgrep -x scribe-server >/dev/null 2>&1; do
        [ "$SECONDS" -ge "$deadline" ] && return 1
        sleep 0.2
    done
}

launch_client() {
    scribe-client >>"$CLIENT_LOG" 2>&1 &
    xdotool search --sync --name Scribe >/dev/null 2>&1 || true
}

focus() {
    local wid
    wid=$(xdotool search --onlyvisible --class '[Ss]cribe' 2>/dev/null | tail -1 || true)
    [ -n "$wid" ] \
        || wid=$(xdotool search --onlyvisible --name '[Ss]cribe' 2>/dev/null | tail -1 || true)
    [ -n "$wid" ] || fail "no Scribe window found"
    xdotool windowactivate --sync "$wid" 2>/dev/null \
        || xdotool windowfocus --sync "$wid" 2>/dev/null || true
    sleep 0.8
}

ai_snapshot() {
    grep -l 'provider = "codex_code"' "$RESTORE_DIR"/*.toml 2>/dev/null | head -1
}

ai_record() {
    python3 - "$1" <<'PY'
import sys, tomllib

with open(sys.argv[1], "rb") as source:
    launches = tomllib.load(source)["launches"]
launch = next(item for item in launches if item.get("kind", {}).get("kind") == "ai")
kind = launch["kind"]
print("|".join((
    launch["launch_id"],
    kind["provider"],
    kind["resume_mode"],
    kind.get("conversation_id") or "",
)))
PY
}

wait_for_targeted_snapshot() {
    local deadline=$((SECONDS + 20)) snapshot record
    while [ "$SECONDS" -lt "$deadline" ]; do
        snapshot=$(ai_snapshot || true)
        if [ -n "$snapshot" ]; then
            record=$(ai_record "$snapshot" 2>/dev/null || true)
            case "$record" in
                *"|codex_code|Resume|$CONVERSATION_ID") printf '%s\n' "$snapshot"; return 0 ;;
            esac
        fi
        sleep 0.2
    done
    return 1
}

rm -f "$RECORD"
mkdir -p "$STUB_BIN"
ln -sf /tests/bin/record-ai-invocation "$STUB_BIN/codex"
printf 'export PATH="%s:$PATH"\n' "$STUB_BIN" >> "$HOME/.bashrc"

# First launch is intentionally generic. The live hook supplies the target the
# client must retain through handoff, reattach, and later cold replay.
focus
xdotool key --clearmodifiers ctrl+alt+e
wait_for_count invocation_count 1 20 || fail "Ctrl+Alt+E never launched the Codex stub"
FIRST_SESSION=$(awk '/^SESSION=/{sub(/^SESSION=/, ""); print; exit}' "$RECORD")
[ -n "$FIRST_SESSION" ] || fail "Codex stub did not inherit SCRIBE_SESSION_ID"
FIRST_ARGS=$(awk '/^ARG=/{sub(/^ARG=/, ""); print} /^END$/{exit}' "$RECORD")
[ "$FIRST_ARGS" = resume ] || fail "initial Codex resume argv was '$FIRST_ARGS', expected 'resume'"

SCRIBE_HOOK_SOCK="$HOOK_SOCK" SCRIBE_SESSION_ID="$FIRST_SESSION" scribe-hook-helper \
    --provider=codex_code --event=state_changed --state=processing \
    --conversation-id="$CONVERSATION_ID"
SNAPSHOT=$(wait_for_targeted_snapshot) \
    || fail "live Codex conversation metadata never reached the restore snapshot"
IFS='|' read -r LAUNCH_ID PROVIDER RESUME_MODE SAVED_CONVERSATION <<<"$(ai_record "$SNAPSHOT")"
[ "$PROVIDER|$RESUME_MODE|$SAVED_CONVERSATION" = "codex_code|Resume|$CONVERSATION_ID" ] \
    || fail "initial AI launch metadata was incomplete"
echo "PHASE 1 PASS: live Codex metadata produced a targeted restore binding"

# Exercise the production update handoff, then replace the client while the
# successor server still owns the PTY. The replacement must rebuild its binding
# from SessionList rather than from the pending cold snapshot.
REATTACHES_BEFORE=$(count_log "rebuilt reconnect topology")
scribe-test server upgrade
wait_for_count "count_log 'rebuilt reconnect topology'" "$((REATTACHES_BEFORE + 1))" 25 \
    || fail "client never reattached after server upgrade"
[ "$(invocation_count)" -eq 1 ] || fail "server upgrade relaunched Codex"

SKIPS_BEFORE=$(count_log "skipping the cold-restart replay")
touch -d @1 "$SNAPSHOT"
pkill -KILL -f 'scribe-client' || true
wait_for_client_exit || fail "client survived the warm restart"
launch_client
wait_for_count "count_log 'skipping the cold-restart replay'" "$((SKIPS_BEFORE + 1))" 25 \
    || fail "replacement client did not reattach to retained sessions"
wait_for_count "stat -c %Y '$SNAPSHOT'" 2 20 \
    || fail "replacement client never rewrote the restore snapshot"
IFS='|' read -r WARM_LAUNCH_ID PROVIDER RESUME_MODE SAVED_CONVERSATION <<<"$(ai_record "$SNAPSHOT")"
[ "$WARM_LAUNCH_ID" = "$LAUNCH_ID" ] || fail "warm reattach replaced launch identity"
[ "$PROVIDER|$RESUME_MODE|$SAVED_CONVERSATION" = "codex_code|Resume|$CONVERSATION_ID" ] \
    || fail "warm reattach saved Codex as a generic resume or shell"
[ "$(invocation_count)" -eq 1 ] || fail "warm client reattach relaunched Codex"
echo "PHASE 2 PASS: update and warm client reattach retained exact launch metadata"

# Now remove both live processes. Only the rewritten snapshot can tell the new
# server what to launch, and the stub is the argv oracle for that CreateSession.
pkill -KILL -f 'scribe-client' || true
wait_for_client_exit || fail "client survived the cold restart"
scribe-test server stop
wait_for_server_exit || fail "server process outlived stop"
scribe-test server start
pgrep -x scribe-server >/dev/null 2>&1 || fail "replacement server did not start"
launch_client
wait_for_count invocation_count 2 30 || fail "cold replay never relaunched Codex"
[ "$(invocation_count)" -eq 2 ] || fail "cold replay launched Codex more than once"
SECOND_ARGS=$(awk '
    /^BEGIN$/ { invocation++ }
    invocation == 2 && /^ARG=/ { sub(/^ARG=/, ""); print }
' "$RECORD")
[ "$SECOND_ARGS" = "$(printf 'resume\n%s' "$CONVERSATION_ID")" ] \
    || fail "cold replay Codex argv was '$SECOND_ARGS'"
echo "PHASE 3 PASS: cold replay invoked exact targeted Codex resume"

echo "PASS: Codex metadata survives update, warm reattach, and cold replay"
