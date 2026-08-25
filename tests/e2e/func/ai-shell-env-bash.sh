#!/bin/bash
[ "${SCRIBE_E2E_SANDBOX:-0}" = "1" ] || { echo "FATAL: this script only runs inside the scribe e2e container (use just e2e-func / e2e-visual)." >&2; exit 99; }
set -euo pipefail

# @lat: [[test#E2E Functional Tests#AI Shell Environment Matrix#Bash AI Shell Environment]]
CLAUDE_RECORD=/tmp/claude-invocation.txt
CLAUDE_EXPECTED=/tmp/claude-expected-argv.txt
CLAUDE_ACTUAL=/tmp/claude-actual-argv.txt
CODEX_RECORD=/tmp/codex-invocation.txt
CODEX_EXPECTED=/tmp/codex-expected-argv.txt
CODEX_ACTUAL=/tmp/codex-actual-argv.txt
RECORD=$CLAUDE_RECORD
EXPECTED=$CLAUDE_EXPECTED
ACTUAL=$CLAUDE_ACTUAL
PROVIDER=claude
SERVER_LOG=/output/ai-shell-env-bash-server.log
REQUESTED_CWD=/tmp/scribe-ai-shell-env-bash/requested
MISSING_CWD=/tmp/scribe-ai-shell-env-bash/missing
CONVERSATION_ID="bash conversation's id"
CLAUDE_WRAPPER_MARKER=AI_CLAUDE_WRAPPER=claude-shell-function
CODEX_WRAPPER_MARKER=AI_CODEX_WRAPPER=codex-shell-function

wait_for_record() {
    for _ in $(seq 1 50); do
        [ -f "$RECORD" ] && return 0
        sleep 0.1
    done
    echo "FAIL: $PROVIDER stub did not record an invocation"
    exit 1
}

assert_env() {
    if ! grep -Fqx "$1" "$RECORD"; then
        echo "FAIL: $PROVIDER environment is missing '$1'"
        exit 1
    fi
}

assert_env_absent() {
    if grep -q "^$1=" "$RECORD"; then
        echo "FAIL: $PROVIDER environment leaked $1"
        exit 1
    fi
}

assert_invocation() {
    local expected_cwd=$1 wrapper_marker=$2

    awk '/^--ENV--$/ { exit } { print }' "$RECORD" > "$ACTUAL"
    if ! cmp -s "$EXPECTED" "$ACTUAL"; then
        echo "FAIL: $PROVIDER stub argv did not match"
        echo "expected:"
        sed 's/^/  /' "$EXPECTED"
        echo "actual:"
        sed 's/^/  /' "$ACTUAL"
        exit 1
    fi

    assert_env "PWD=$expected_cwd"
    assert_env "$wrapper_marker"
    assert_env 'AI_STARTUP_ORDER=bashrc'
    assert_env 'SCRIBE_SHELL_INTEGRATION=1'
    assert_env 'TERM_PROGRAM=Scribe'
    assert_env_absent AI_UNEXPECTED_STARTUP
    assert_env_absent SCRIBE_RESTORE_ENV_DELTA_FILE
    assert_env_absent ENV
}

# Cycle the entrypoint's disposable server so it picks up this suite's shell
# selection and log file. The server resolves the session shell from its own
# `SHELL`, so exporting it here is what makes the launch deterministic.
scribe-test daemon stop
scribe-test server stop
rm -f "$SERVER_LOG" \
    "$CLAUDE_RECORD" "$CLAUDE_EXPECTED" "$CLAUDE_ACTUAL" \
    "$CODEX_RECORD" "$CODEX_EXPECTED" "$CODEX_ACTUAL"
export SCRIBE_TEST_SERVER_LOG="$SERVER_LOG"
export RUST_LOG=scribe_server=debug
BASH_BIN=$(command -v bash)
export SHELL="$BASH_BIN"
scribe-test server start
scribe-test daemon start

install -d "$REQUESTED_CWD"
# An AI tab is a plain tab plus an interactive command, so bash starts
# non-login and reads only `.bashrc` — exactly the file every other Scribe tab
# reads. The wrappers export unique markers before forwarding all arguments to
# stubs, proving the provider occupies shell command position. The three
# login-profile files must stay untouched, and the provider is reachable only
# through the PATH `.bashrc` exports, so a regression that reintroduces login
# startup records no invocation at all.
cat > "$HOME/.bashrc" <<'BASHRC'
export AI_STARTUP_ORDER=bashrc
export PATH="/tests/bin:$PATH"
claude() {
    export AI_CLAUDE_WRAPPER=claude-shell-function
    command claude "$@"
}
codex() {
    export AI_CODEX_WRAPPER=codex-shell-function
    command codex "$@"
}
BASHRC
printf '%s\n' 'export AI_UNEXPECTED_STARTUP=bash_profile' > "$HOME/.bash_profile"
printf '%s\n' 'export AI_UNEXPECTED_STARTUP=bash_login' > "$HOME/.bash_login"
printf '%s\n' 'export AI_UNEXPECTED_STARTUP=profile' > "$HOME/.profile"
printf '%s\n' '--resume' "$CONVERSATION_ID" > "$EXPECTED"

CLAUDE_SESSION=$(scribe-test session create \
    --ai-provider claude \
    --ai-resume-mode resume \
    --ai-conversation-id "$CONVERSATION_ID" \
    --cwd "$REQUESTED_CWD")
wait_for_record
assert_invocation "$REQUESTED_CWD" "$CLAUDE_WRAPPER_MARKER"
scribe-test assert-exit "$CLAUDE_SESSION" 0 --timeout 5000

rm -f "$RECORD"
scribe-test session create \
    --ai-provider claude \
    --ai-resume-mode resume \
    --ai-conversation-id "$CONVERSATION_ID" \
    --cwd "$MISSING_CWD" >/dev/null
wait_for_record
assert_invocation "$HOME" "$CLAUDE_WRAPPER_MARKER"

PROVIDER=codex
RECORD=$CODEX_RECORD
EXPECTED=$CODEX_EXPECTED
ACTUAL=$CODEX_ACTUAL
printf '%s\n' 'resume' "$CONVERSATION_ID" > "$EXPECTED"
CODEX_SESSION=$(scribe-test session create \
    --ai-provider codex \
    --ai-resume-mode resume \
    --ai-conversation-id "$CONVERSATION_ID" \
    --cwd "$REQUESTED_CWD")
wait_for_record
assert_invocation "$REQUESTED_CWD" "$CODEX_WRAPPER_MARKER"
scribe-test assert-exit "$CODEX_SESSION" 0 --timeout 5000

rm -f "$RECORD"
scribe-test session create \
    --ai-provider codex \
    --ai-resume-mode resume \
    --ai-conversation-id "$CONVERSATION_ID" \
    --cwd "$MISSING_CWD" >/dev/null
wait_for_record
assert_invocation "$HOME" "$CODEX_WRAPPER_MARKER"

PROVIDER=claude
RECORD=$CLAUDE_RECORD
EXPECTED=$CLAUDE_EXPECTED
ACTUAL=$CLAUDE_ACTUAL
if [ "${SCRIBE_KEYRING:-0}" != "1" ]; then
    echo "KEYRING SKIP: encrypted bash restore requires SCRIBE_KEYRING=1"
    echo "PASS: bash AI launches resolved Claude and Codex wrappers with plain-tab startup, argv, and cwd guard"
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
printf '%s\n' 'export AI_ENCRYPTED_RESTORE=rc' >>"$HOME/.bashrc"
export XDG_RUNTIME_DIR="/run/user/$(id -u)"
scribe-test daemon stop
scribe-test server stop
scribe-test server start
scribe-test daemon start

WRITER=$(scribe-test session create)
WRITER_ENVELOPE=$(scribe-test session envelope-id "$WRITER")
WINDOW=$(scribe-test daemon window-id)
scribe-test wait-idle "$WRITER" --ms 500
RESTORED_VALUE=bash-envz-restore-4c19a7
scribe-test send "$WRITER" "export AI_ENCRYPTED_RESTORE=$RESTORED_VALUE\n"

STATE_HOME=${XDG_STATE_HOME:-$HOME/.local/state}
ENVZ="$STATE_HOME/scribe/restore/env/$WINDOW/$WRITER_ENVELOPE.envz"
for _ in $(seq 1 100); do
    [ -s "$ENVZ" ] && break
    sleep 0.1
done
if [ ! -s "$ENVZ" ]; then
    echo "FAIL: bash writer did not persist encrypted envelope $ENVZ"
    exit 1
fi
if grep -aFq "$RESTORED_VALUE" "$ENVZ"; then
    echo "FAIL: bash envelope leaked restored plaintext"
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
assert_invocation "$REQUESTED_CWD" "$CLAUDE_WRAPPER_MARKER"
assert_env "AI_ENCRYPTED_RESTORE=$RESTORED_VALUE"

# prepare_restore_env_file stages this exact session-specific name beneath
# $XDG_RUNTIME_DIR/scribe/env-apply. Seeing the delta in the provider proves
# the file was sourced; its absence proves the launch consumed it.
STAGING_DIR="$XDG_RUNTIME_DIR/scribe/env-apply"
if [ ! -d "$STAGING_DIR" ]; then
    echo "FAIL: bash restore staging directory was not created"
    exit 1
fi
STAGED_FILE=$(find "$STAGING_DIR" -maxdepth 1 -type f \
    -name "$AI_SESSION-*.sh" -print -quit)
if [ -n "$STAGED_FILE" ]; then
    echo "FAIL: bash AI restore staging file was not consumed: $STAGED_FILE"
    exit 1
fi

scribe-test session close "$WRITER"
echo "KEYRING PASS: bash AI launch restored encrypted delta and consumed staging file"
echo "PASS: bash AI launches resolved Claude and Codex wrappers with plain-tab startup, argv, and cwd guard"
