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
    assert_env 'AI_STARTUP_ORDER=zshenv,zshrc'
    assert_env 'AI_ZSHRC_SAW_INTEGRATION=1'
    assert_env_absent AI_UNEXPECTED_STARTUP
    assert_env 'SCRIBE_SHELL_INTEGRATION=1'
    assert_env 'TERM_PROGRAM=Scribe'
    assert_env_absent SCRIBE_RESTORE_ENV_DELTA_FILE
    assert_env_absent SCRIBE_ORIG_ZDOTDIR
    assert_env_absent ZDOTDIR
}

scribe-test daemon stop
scribe-test server stop
rm -f "$SERVER_LOG" "$RECORD" "$EXPECTED" "$ACTUAL"
export SCRIBE_TEST_SERVER_LOG="$SERVER_LOG"
export RUST_LOG=scribe_server=debug
ZSH_BIN=$(command -v zsh)
export SHELL="$ZSH_BIN"
scribe-test server start
scribe-test daemon start

install -d "$REQUESTED_CWD"
# An AI tab is a plain tab plus an interactive `exec`, so zsh starts non-login:
# `.zshenv` then `.zshrc` through the redirected ZDOTDIR bootstrap, with
# `.zprofile` deliberately unread. The provider lives on the `.zshrc` PATH.
# Scribe's ZDOTDIR bootstrap sources the user's `.zshenv` before the
# integration script, so the "integration is attached" probe belongs in
# `.zshrc`, which runs after it.
printf '%s\n' 'export AI_STARTUP_ORDER=zshenv' > "$HOME/.zshenv"
printf '%s\n' 'export AI_UNEXPECTED_STARTUP=zprofile' > "$HOME/.zprofile"
cat > "$HOME/.zshrc" <<'ZSHRC'
export AI_STARTUP_ORDER="$AI_STARTUP_ORDER,zshrc"
if [[ -n "${_SCRIBE_INTEGRATION_SOURCED:-}" ]]; then
    export AI_ZSHRC_SAW_INTEGRATION=1
else
    export AI_ZSHRC_SAW_INTEGRATION=missing
fi
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

if [ "${SCRIBE_KEYRING:-0}" != "1" ]; then
    echo "KEYRING SKIP: encrypted zsh restore requires SCRIBE_KEYRING=1"
    echo "PASS: zsh AI launch used plain-tab rc startup, integration env, argv, and cwd guard"
    exit 0
fi

# Seed through the same plain-shell integration, hook ingress, and debounced
# encrypted store path as env-persistence.sh. The entrypoint owns the D-Bus and
# Secret Service fixture; this restart only gives the disposable server its
# normal desktop-session runtime directory before the writer is created.
cat >"$HOME/.config/scribe/config.toml" <<'TOML'
[terminal.env_persistence]
enabled = true
TOML
printf '%s\n' 'export AI_ENCRYPTED_RESTORE=rc' >>"$HOME/.zshrc"
export XDG_RUNTIME_DIR="/run/user/$(id -u)"
scribe-test daemon stop
scribe-test server stop
scribe-test server start
scribe-test daemon start

WRITER=$(scribe-test session create)
WRITER_ENVELOPE=$(scribe-test session envelope-id "$WRITER")
WINDOW=$(scribe-test daemon window-id)
scribe-test wait-idle "$WRITER" --ms 500
RESTORED_VALUE=zsh-envz-restore-7d25b8
scribe-test send "$WRITER" "export AI_ENCRYPTED_RESTORE=$RESTORED_VALUE\n"

STATE_HOME=${XDG_STATE_HOME:-$HOME/.local/state}
ENVZ="$STATE_HOME/scribe/restore/env/$WINDOW/$WRITER_ENVELOPE.envz"
for _ in $(seq 1 100); do
    [ -s "$ENVZ" ] && break
    sleep 0.1
done
if [ ! -s "$ENVZ" ]; then
    echo "FAIL: zsh writer did not persist encrypted envelope $ENVZ"
    exit 1
fi
if grep -aFq "$RESTORED_VALUE" "$ENVZ"; then
    echo "FAIL: zsh envelope leaked restored plaintext"
    exit 1
fi

rm -f "$RECORD"
AI_SESSION=$(scribe-test session create \
    --ai-provider claude \
    --ai-resume-mode resume \
    --ai-conversation-id "$CONVERSATION_ID" \
    --cwd "$REQUESTED_CWD" \
    --env-envelope-id "$WRITER_ENVELOPE")
wait_for_record
assert_invocation "$REQUESTED_CWD"
assert_env "AI_ENCRYPTED_RESTORE=$RESTORED_VALUE"

# prepare_restore_env_file stages this exact session-specific name beneath
# $XDG_RUNTIME_DIR/scribe/env-apply. Seeing the delta in the provider proves
# the file was sourced; its absence proves the launch consumed it.
STAGING_DIR="$XDG_RUNTIME_DIR/scribe/env-apply"
if [ ! -d "$STAGING_DIR" ]; then
    echo "FAIL: zsh restore staging directory was not created"
    exit 1
fi
STAGED_FILE=$(find "$STAGING_DIR" -maxdepth 1 -type f \
    -name "$AI_SESSION-*.sh" -print -quit)
if [ -n "$STAGED_FILE" ]; then
    echo "FAIL: zsh AI restore staging file was not consumed: $STAGED_FILE"
    exit 1
fi

scribe-test session close "$WRITER"
echo "KEYRING PASS: zsh AI launch restored encrypted delta and consumed staging file"
echo "PASS: zsh AI launch used plain-tab rc startup, integration env, argv, and cwd guard"
