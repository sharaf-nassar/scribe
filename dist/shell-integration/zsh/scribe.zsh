# Scribe shell integration — zsh
# Sourced from .zshenv bootstrap after ZDOTDIR is restored.

# Guards
[[ "${TERM_PROGRAM:-}" != "Scribe" ]] && return 0
[[ "${SCRIBE_SHELL_INTEGRATION:-1}" == "0" ]] && return 0
[[ -n "${_SCRIBE_INTEGRATION_SOURCED:-}" ]] && return 0
_SCRIBE_INTEGRATION_SOURCED=1

# ── Colored completions ──────────────────────────────────────────
zstyle ':completion:*' list-colors "${(s.:.)LS_COLORS}"

# ── OSC 133 prompt marking ───────────────────────────────────────
# Track last exit status for D mark
typeset -gi _scribe_last_status=0

__scribe_sanitize_context() {
	local value="${1//$'\n'/ }"
	value="${value//$'\r'/ }"
	value="${value//;/_}"
	print -rn -- "$value"
}

__scribe_emit_context() {
	local remote=0
	local host tmux_session=""
	if [[ -n "${SSH_CONNECTION:-}" || -n "${SSH_CLIENT:-}" || -n "${SSH_TTY:-}" ]]; then
		remote=1
	fi

	host="$(__scribe_sanitize_context "${HOST:-$(hostname 2>/dev/null)}")"
	if [[ -n "${TMUX:-}" ]] && command -v tmux >/dev/null 2>&1; then
		tmux_session="$(tmux display-message -p '#S' 2>/dev/null || true)"
		tmux_session="$(__scribe_sanitize_context "$tmux_session")"
	fi

	printf '\e]1337;ScribeContext;remote=%s' "$remote"
	[[ -n "$host" ]] && printf ';host=%s' "$host"
	[[ -n "$tmux_session" ]] && printf ';tmux=%s' "$tmux_session"
	printf '\e\\'
}

__scribe_precmd() {
	_scribe_last_status=$?

	# OSC 133;D — end of previous command
	printf '\e]133;D;%d\e\\' "$_scribe_last_status"

	# OSC 7 — report CWD
	printf '\e]7;file://%s%s\e\\' "${HOST}" "$(__scribe_urlencode "${PWD}")"

	# OSC 1337 — report remote host/tmux context
	__scribe_emit_context

	# Clear any stale Codex task label once control returns to the shell.
	printf '\e]1337;CodexTaskLabelCleared\e\\'

	# OSC 2 — window title
	printf '\e]2;%s\e\\' "${PWD:t}"

	# OSC 133;A — prompt start (with click_events=1)
	printf '\e]133;A;click_events=1\e\\'
}

__scribe_preexec() {
	# OSC 133;C — command execution start
	printf '\e]133;C\e\\'

	# OSC 2 — update title with running command
	printf '\e]2;%s\e\\' "$1"
}

# URL-encode a path
__scribe_urlencode() {
	local input="$1"
	local output=""
	local i c
	for (( i = 0; i < ${#input}; i++ )); do
		c="${input:$i:1}"
		case "$c" in
			[a-zA-Z0-9/:@._~!-]) output+="$c" ;;
			*) output+="$(printf '%%%02X' "'$c")" ;;
		esac
	done
	printf '%s' "$output"
}

# Register hooks using zsh's hook arrays (composes with oh-my-zsh, prezto, etc.)
autoload -Uz add-zsh-hook
add-zsh-hook precmd __scribe_precmd
add-zsh-hook preexec __scribe_preexec

# OSC 133;B — prompt end / input start (via zle-line-init widget)
# This fires when the line editor starts, which is right after the prompt is drawn.
__scribe_zle_line_init() {
	printf '\e]133;B\e\\'
}
# Only install if not already wrapped (avoid conflicts with other integrations)
if [[ "${widgets[zle-line-init]:-}" != *"scribe"* ]]; then
	if [[ -n "${widgets[zle-line-init]:-}" ]]; then
		# Preserve existing zle-line-init
		zle -A zle-line-init __scribe_orig_zle_line_init
		__scribe_zle_line_init_wrapper() {
			__scribe_zle_line_init
			zle __scribe_orig_zle_line_init
		}
		zle -N zle-line-init __scribe_zle_line_init_wrapper
	else
		zle -N zle-line-init __scribe_zle_line_init
	fi
fi
