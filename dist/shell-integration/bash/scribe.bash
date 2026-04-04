#!/usr/bin/env bash
# Scribe terminal emulator — bash shell integration
# Loaded via --rcfile for interactive bash sessions inside Scribe.

# ---------------------------------------------------------------------------
# Guards
# ---------------------------------------------------------------------------

# Only run inside Scribe
[[ "${TERM_PROGRAM:-}" != "Scribe" ]] && return 0

# Opt-out check
[[ "${SCRIBE_SHELL_INTEGRATION:-1}" == "0" ]] && return 0

# Re-entrancy guard
[[ -n "${_SCRIBE_INTEGRATION_SOURCED:-}" ]] && return 0
_SCRIBE_INTEGRATION_SOURCED=1

# Non-interactive: skip
[[ $- != *i* ]] && return 0

# ---------------------------------------------------------------------------
# Disable POSIX mode if active (e.g. legacy ENV-based injection).
# With --rcfile this is a no-op, but kept for safety.
# ---------------------------------------------------------------------------
set +o posix 2>/dev/null

# ---------------------------------------------------------------------------
# Source user startup files
# --rcfile replaces ~/.bashrc, so we must source startup files ourselves.
# On macOS, Terminal.app historically launches bash as a login shell, so many
# users keep their interactive setup in ~/.bash_profile. Scribe still uses a
# non-login bash to keep shell integration attached, so emulate the login-shell
# startup order on Darwin before falling back to ~/.bashrc.
# ---------------------------------------------------------------------------

# Unset ENV to prevent re-sourcing in child shells
unset ENV

__scribe_source_login_profile() {
	[[ -f /etc/profile ]] && source /etc/profile
	for f in ~/.bash_profile ~/.bash_login ~/.profile; do
		if [[ -f "$f" ]]; then
			source "$f"
			return 0  # bash only sources the FIRST one found
		fi
	done
	return 1
}

if shopt -q login_shell; then
	__scribe_source_login_profile
elif [[ "$(uname -s 2>/dev/null)" == "Darwin" ]]; then
	if ! __scribe_source_login_profile; then
		[[ -f ~/.bashrc ]] && source ~/.bashrc
	fi
else
	# Non-login interactive: source .bashrc
	[[ -f ~/.bashrc ]] && source ~/.bashrc
fi

# ---------------------------------------------------------------------------
# Colored completions (readline)
# ---------------------------------------------------------------------------
bind "set colored-stats on" 2>/dev/null
bind "set colored-completion-prefix on" 2>/dev/null
bind "set visible-stats on" 2>/dev/null
bind "set mark-symlinked-directories on" 2>/dev/null

# ---------------------------------------------------------------------------
# OSC 133 prompt marking + OSC 7 (CWD) + OSC 2 (title)
# ---------------------------------------------------------------------------

# URL-encode a path (spaces and special chars)
__scribe_urlencode() {
	local string="$1"
	local length=${#string}
	local i c o
	for (( i = 0; i < length; i++ )); do
		c="${string:i:1}"
		case "$c" in
			[a-zA-Z0-9/:@._~!-]) o="$c" ;;
			*) printf -v o '%%%02X' "'$c" ;;
		esac
		printf '%s' "$o"
	done
}

__scribe_sanitize_context() {
	local value="${1//$'\n'/ }"
	value="${value//$'\r'/ }"
	value="${value//;/_}"
	printf '%s' "$value"
}

__scribe_emit_context() {
	local remote=0 host tmux_session=""
	if [[ -n "${SSH_CONNECTION:-}" || -n "${SSH_CLIENT:-}" || -n "${SSH_TTY:-}" ]]; then
		remote=1
	fi

	host="$(__scribe_sanitize_context "${HOSTNAME:-$(hostname 2>/dev/null)}")"
	if [[ -n "${TMUX:-}" ]] && command -v tmux >/dev/null 2>&1; then
		tmux_session="$(tmux display-message -p '#S' 2>/dev/null || true)"
		tmux_session="$(__scribe_sanitize_context "$tmux_session")"
	fi

	printf '\e]1337;ScribeContext;remote=%s' "$remote"
	[[ -n "$host" ]] && printf ';host=%s' "$host"
	[[ -n "$tmux_session" ]] && printf ';tmux=%s' "$tmux_session"
	printf '\e\\'
}

__scribe_prompt_command() {
	local last_status=$?

	# OSC 133;D — end of previous command (with exit code)
	printf '\e]133;D;%d\e\\' "$last_status"

	# OSC 7 — report CWD
	printf '\e]7;file://%s%s\e\\' "${HOSTNAME}" "$(__scribe_urlencode "$PWD")"

	# OSC 1337 — report remote host/tmux context
	__scribe_emit_context

	# Clear any stale Codex task label once control returns to the shell.
	printf '\e]1337;CodexTaskLabelCleared\e\\'

	# OSC 2 — window title (basename of CWD)
	printf '\e]2;%s\e\\' "${PWD##*/}"

	# OSC 133;A — prompt start (with click_events=1)
	printf '\e]133;A;click_events=1\e\\'
}

# Prepend to PROMPT_COMMAND (don't replace existing)
if [[ -z "${PROMPT_COMMAND:-}" ]]; then
	PROMPT_COMMAND="__scribe_prompt_command"
elif [[ "${PROMPT_COMMAND}" != *"__scribe_prompt_command"* ]]; then
	PROMPT_COMMAND="__scribe_prompt_command;${PROMPT_COMMAND}"
fi

# OSC 133;B — prompt end / input start (embedded in PS1)
# Wrap PS1 so the B mark appears right after the prompt text
PS1="${PS1:-\\$ }\[\e]133;B\e\\\\\]"

# OSC 133;C — command start (via PS0, bash 4.4+)
if [[ "${BASH_VERSINFO[0]}" -ge 5 || ( "${BASH_VERSINFO[0]}" -eq 4 && "${BASH_VERSINFO[1]}" -ge 4 ) ]]; then
	PS0=$'\e]133;C\e\\'
fi
