#!/bin/bash
[ "${SCRIBE_E2E_SANDBOX:-0}" = "1" ] || { echo "FATAL: this script only runs inside the scribe e2e container (use just e2e-func / e2e-visual)." >&2; exit 99; }
set -euo pipefail

# @lat: [[test#E2E Functional Tests#AI Shell Environment Matrix#Zsh AI Shell Environment]]
RECORD=/tmp/claude-invocation.txt
EXPECTED=/tmp/claude-expected-argv.txt
ACTUAL=/tmp/claude-actual-argv.txt
SERVER_LOG=/output/ai-shell-env-zsh-server.log
REQUESTED_CWD=/tmp/scribe-ai-shell-env-zsh/requested
MISSING_CWD=/tmp/scribe-ai-shell-env-zsh/missing
CONVERSATION_ID="zsh conversation's id"

wait_for_record() {
    for _ in $(seq 1 50); do
        [ -f "$RECORD" ] && return 0
        sleep 0.1
    done
    echo "FAIL: claude stub did not record an invocation"
    exit 1
}

assert_env() {
    if ! grep -Fqx "$1" "$RECORD"; then
        echo "FAIL: claude environment is missing '$1'"
        exit 1
    fi
}

assert_env_absent() {
    if grep -q "^$1=" "$RECORD"; then
        echo "FAIL: claude environment leaked $1"
        exit 1
    fi
}

assert_invocation() {
    local expected_cwd=$1

    awk '/^--ENV--$/ { exit } { print }' "$RECORD" > "$ACTUAL"
    if ! cmp -s "$EXPECTED" "$ACTUAL"; then
        echo "FAIL: claude stub argv did not match"
        echo "expected:"
        sed 's/^/  /' "$EXPECTED"
        echo "actual:"
        sed 's/^/  /' "$ACTUAL"
        exit 1
    fi

    assert_env "PWD=$expected_cwd"
    assert_env 'AI_STARTUP_ORDER=zshenv,zprofile,zshrc'
    assert_env 'AI_ZSHENV_SAW_AI_TAB=1'
    assert_env 'AI_ZPROFILE_SAW_INTEGRATION=1'
    assert_env 'SCRIBE_SHELL_INTEGRATION=1'
    assert_env 'TERM_PROGRAM=Scribe'
    assert_env_absent SCRIBE_AI_TAB
    assert_env_absent SCRIBE_INTEGRATION_SCRIPT
    assert_env_absent SCRIBE_RESTORE_ENV_DELTA_FILE
    assert_env_absent SCRIBE_ORIG_ZDOTDIR
    assert_env_absent ZDOTDIR
}

scribe-test daemon stop
scribe-test server stop
rm -f "$SERVER_LOG" "$RECORD" "$EXPECTED" "$ACTUAL"
export SCRIBE_TEST_SERVER_LOG="$SERVER_LOG"
export RUST_LOG=scribe_server=debug
scribe-test server start
scribe-test daemon start

ZSH_BIN=$(command -v zsh)
usermod -s "$ZSH_BIN" "$(id -un)"

install -d "$REQUESTED_CWD"
cat > "$HOME/.zshenv" <<'ZSHENV'
export AI_STARTUP_ORDER=zshenv
export AI_ZSHENV_SAW_AI_TAB="${SCRIBE_AI_TAB:-missing}"
ZSHENV
cat > "$HOME/.zprofile" <<'ZPROFILE'
export AI_STARTUP_ORDER="$AI_STARTUP_ORDER,zprofile"
if [[ -n "${_SCRIBE_INTEGRATION_SOURCED:-}" ]]; then
    export AI_ZPROFILE_SAW_INTEGRATION=1
else
    export AI_ZPROFILE_SAW_INTEGRATION=missing
fi
ZPROFILE
cat > "$HOME/.zshrc" <<'ZSHRC'
export AI_STARTUP_ORDER="$AI_STARTUP_ORDER,zshrc"
export PATH="/tests/bin:$PATH"
ZSHRC
printf '%s\n' '--resume' "$CONVERSATION_ID" > "$EXPECTED"

scribe-test session create \
    --ai-provider claude \
    --ai-resume-mode resume \
    --ai-conversation-id "$CONVERSATION_ID" \
    --cwd "$REQUESTED_CWD" >/dev/null
wait_for_record
assert_invocation "$REQUESTED_CWD"

rm -f "$RECORD"
scribe-test session create \
    --ai-provider claude \
    --ai-resume-mode resume \
    --ai-conversation-id "$CONVERSATION_ID" \
    --cwd "$MISSING_CWD" >/dev/null
wait_for_record
assert_invocation "$HOME"

if ! grep -F 'resolved host login shell for AI launch' "$SERVER_LOG" \
    | grep -F "$ZSH_BIN" \
    | grep -Fq '"passwd"'; then
    echo "FAIL: server log did not prove passwd-tier zsh resolution"
    tail -40 "$SERVER_LOG"
    exit 1
fi

echo "PASS: zsh AI launch used passwd shell, startup order, integration env, argv, and cwd guard"
