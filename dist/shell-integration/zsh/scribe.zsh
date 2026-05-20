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

	# Clear any stale provider task label once control returns to the shell.
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

	# OSC 1337 ScribeAiLaunch — pre-arm Scribe's ED 3 filter when the user
	# runs an AI binary, so `<tool> --resume`'s pre-OSC-1337 \x1b[3J still
	# hits the filter even after ai_provider was cleared by the previous
	# 133;A on shell-prompt return.
	local first_word="${${1%% *}##*/}"
	case "$first_word" in
		claude) printf '\e]1337;ScribeAiLaunch=claude_code\e\\' ;;
		codex) printf '\e]1337;ScribeAiLaunch=codex_code\e\\' ;;
	esac
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

# ── Env-delta capture (feature 006) ──────────────────────────────
# Three additions, in this order, per spec contract:
#   1. Source the restore-delta file if the server staged one (post-rc, so
#      user-set values from the previous session beat any rc-driven defaults).
#   2. Initialize the per-session "last emitted" snapshot.
#   3. One-shot baseline emit (--baseline-ready), then register an
#      add-zsh-hook precmd that emits subsequent deltas.
#
# Helper invocations fail open: stdout/stderr discarded, exit code ignored.

# Source restore-delta file (FR-008: applied AFTER rc has run).
if [[ -n "${SCRIBE_RESTORE_ENV_DELTA_FILE:-}" && -f "${SCRIBE_RESTORE_ENV_DELTA_FILE}" ]]; then
	source "${SCRIBE_RESTORE_ENV_DELTA_FILE}"
	rm -f "${SCRIBE_RESTORE_ENV_DELTA_FILE}" 2>/dev/null || true
	unset SCRIBE_RESTORE_ENV_DELTA_FILE
fi

# Per-session "last emitted" snapshot (associative array, name → value).
typeset -gA __scribe_env_last

# JSON-escape a single string for embedding in a JSON object/array literal.
# Echoes the escaped form (no surrounding quotes).
__scribe_json_escape() {
	local s="$1" out="" i c
	local -i len=${#s} hex
	for (( i = 1; i <= len; i++ )); do
		c="${s[i]}"
		case "$c" in
			'\') out+='\\' ;;
			'"') out+='\"' ;;
			$'\b') out+='\b' ;;
			$'\f') out+='\f' ;;
			$'\n') out+='\n' ;;
			$'\r') out+='\r' ;;
			$'\t') out+='\t' ;;
			*)
				hex=$(printf '%d' "'$c")
				if (( hex < 0x20 )); then
					out+=$(printf '\\u%04x' "$hex")
				else
					out+="$c"
				fi
				;;
		esac
	done
	print -rn -- "$out"
}

# Snapshot the current exported env into the associative array whose
# name is in $1. Uses zsh's `typeset -px` listing of exported parameters
# parsed via the `${(k)parameters[(R)*export*]}` pattern: iterate
# `${(k)parameters}` and include only those whose value contains the
# `export` flag. This is more reliable than parsing `env` output, which
# can split on newlines inside values.
__scribe_snapshot_env() {
	local destvar="$1"
	# Reset the destination array. Use `set -A` indirection through eval
	# to clear it before re-populating.
	eval "${destvar}=()"
	local name
	for name in "${(k)parameters[@]}"; do
		# Skip empty names, scribe-internal vars, and non-exported names.
		[[ -z "$name" || "$name" == _SCRIBE_* || "$name" == __scribe_* ]] && continue
		[[ "${parameters[$name]}" == *export* ]] || continue
		# Indirect read of the value; use ${(P)name} for parameter expansion
		# by name. Default to empty for unset/null.
		eval "${destvar}[\$name]=\${(P)name-}"
	done
}

# Build a JSON object literal `{"NAME":"value",...}` from the assoc
# array whose name is in $1. Uses eval-based indirection because zsh
# associative-array key/value indirection through `${(P)var}` is brittle.
__scribe_build_added_json() {
	local srcvar="$1"
	local -i first=1
	local name esc_name esc_value value keys_str
	# Capture keys via eval into a positional array.
	local -a keys
	eval "keys=(\"\${(@k)${srcvar}}\")"
	print -rn -- '{'
	for name in "${keys[@]}"; do
		esc_name=$(__scribe_json_escape "$name")
		eval "value=\${${srcvar}[\$name]-}"
		esc_value=$(__scribe_json_escape "$value")
		if (( first )); then
			first=0
		else
			print -rn -- ','
		fi
		print -rn -- "\"$esc_name\":\"$esc_value\""
	done
	print -rn -- '}'
}

# Per-prompt env-delta emit. Skips the helper invocation entirely when
# the diff is empty.
__scribe_emit_env_delta() {
	typeset -A __scribe_env_now
	__scribe_snapshot_env __scribe_env_now

	local -i first_added=1 first_removed=1
	local added_json='{' removed_json='[' name esc_name esc_value
	local cur prev

	for name in ${(k)__scribe_env_now[@]}; do
		cur="${__scribe_env_now[$name]}"
		if (( ${+__scribe_env_last[$name]} )); then
			prev="${__scribe_env_last[$name]}"
			[[ "$prev" == "$cur" ]] && continue
		fi
		esc_name=$(__scribe_json_escape "$name")
		esc_value=$(__scribe_json_escape "$cur")
		if (( first_added )); then
			first_added=0
		else
			added_json+=','
		fi
		added_json+="\"$esc_name\":\"$esc_value\""
	done
	added_json+='}'

	for name in ${(k)__scribe_env_last[@]}; do
		if ! (( ${+__scribe_env_now[$name]} )); then
			esc_name=$(__scribe_json_escape "$name")
			if (( first_removed )); then
				first_removed=0
			else
				removed_json+=','
			fi
			removed_json+="\"$esc_name\""
		fi
	done
	removed_json+=']'

	if [[ "$added_json" == '{}' && "$removed_json" == '[]' ]]; then
		return 0
	fi

	scribe-hook-helper --provider=system --event=env-delta \
		--added-json="$added_json" --removed-json="$removed_json" \
		</dev/null >/dev/null 2>&1 || true

	# Update the cache to the just-emitted state.
	__scribe_env_last=()
	for name in ${(k)__scribe_env_now[@]}; do
		__scribe_env_last[$name]="${__scribe_env_now[$name]}"
	done
}

# One-shot baseline emit at the tail (post-rc + post-restore).
__scribe_emit_env_baseline() {
	__scribe_snapshot_env __scribe_env_last
	local added_json
	added_json=$(__scribe_build_added_json __scribe_env_last)
	scribe-hook-helper --provider=system --event=env-delta \
		--added-json="$added_json" --removed-json='[]' --baseline-ready \
		</dev/null >/dev/null 2>&1 || true
}

__scribe_emit_env_baseline
add-zsh-hook precmd __scribe_emit_env_delta
