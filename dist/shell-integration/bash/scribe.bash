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

	# Clear any stale provider task label once control returns to the shell.
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

# OSC 1337 ScribeAiLaunch — pre-arm Scribe's ED 3 filter when the user runs an
# AI binary so `<tool> --resume`'s pre-OSC-1337 \x1b[3J still hits the filter
# even after ai_provider was cleared by the previous 133;A. DEBUG trap fires
# for each top-level command bash is about to execute; subshell expansions
# during PROMPT_COMMAND/PS1 substitution are skipped via BASH_SUBSHELL.
#
# $_ preservation: a DEBUG trap action runs as a command *before* every
# interactive command, so bash sets $_ to the trap's own last word — the
# user's next `echo $_` would otherwise see `__scribe_emit_ai_launch`
# instead of their previous command's last argument. We capture $_ in the
# trap string (it still holds the previous command's last arg at trap-fire
# time — the canonical bash-preexec technique) and restore it as the
# function's final command. $? is unaffected: bash preserves the exit
# status across DEBUG traps automatically.
__scribe_emit_ai_launch() {
	local __scribe_underscore="$1"
	if [[ "${BASH_SUBSHELL:-0}" -eq 0 ]]; then
		local first_word="${BASH_COMMAND%% *}"
		first_word="${first_word##*/}"
		case "$first_word" in
			claude) printf '\e]1337;ScribeAiLaunch=claude_code\e\\' ;;
			codex) printf '\e]1337;ScribeAiLaunch=codex_code\e\\' ;;
		esac
	fi
	# Restore $_ so interactive `$_` keeps the user's previous last argument.
	: "$__scribe_underscore"
}
trap '__scribe_emit_ai_launch "$_"' DEBUG

# ---------------------------------------------------------------------------
# Env-delta capture (feature 006: persist & restore terminal environment)
#
# Three additions, in this order, per spec contract:
#   1. Source the restore-delta file if the server staged one (post-rc, so
#      user-set values from the previous session beat any rc-driven defaults).
#   2. Initialize the per-session "last emitted" snapshot AFTER the restore
#      source and BEFORE the baseline emit.
#   3. One-shot baseline emit (--baseline-ready), then register a
#      PROMPT_COMMAND-driven hook that emits subsequent deltas.
#
# All helper invocations fail open: stdout/stderr redirected, exit code
# ignored. If scribe-hook-helper is missing or the env-store feature is off
# server-side, the terminal stays fully usable.
# ---------------------------------------------------------------------------

# Source restore-delta file (FR-008: applied AFTER rc has run).
if [[ -n "${SCRIBE_RESTORE_ENV_DELTA_FILE:-}" && -f "${SCRIBE_RESTORE_ENV_DELTA_FILE}" ]]; then
	# shellcheck disable=SC1090
	source "${SCRIBE_RESTORE_ENV_DELTA_FILE}"
	rm -f "${SCRIBE_RESTORE_ENV_DELTA_FILE}" 2>/dev/null || true
	unset SCRIBE_RESTORE_ENV_DELTA_FILE
fi

# Per-session "last emitted" snapshot keyed by env var name.
# Requires bash 4+ (associative arrays). On bash 3, the env-delta feature
# silently disables itself — every read of the cache returns empty, every
# write is a no-op.
if ((BASH_VERSINFO[0] >= 4)); then
	declare -gA __scribe_env_last=()
	__SCRIBE_ENV_DELTA_ENABLED=1
else
	__SCRIBE_ENV_DELTA_ENABLED=0
fi

# JSON-escape a single string for embedding in a JSON object/array literal.
# Handles backslash, double-quote, and ASCII control chars (0x00–0x1F).
# Writes the escaped form (without surrounding quotes) to stdout.
__scribe_json_escape() {
	local s="$1" out="" i c hex
	local len=${#s}
	for ((i = 0; i < len; i++)); do
		c="${s:i:1}"
		case "$c" in
			'\') out+='\\' ;;
			'"') out+='\"' ;;
			$'\b') out+='\b' ;;
			$'\f') out+='\f' ;;
			$'\n') out+='\n' ;;
			$'\r') out+='\r' ;;
			$'\t') out+='\t' ;;
			*)
				# Control chars 0x00–0x1F (excluding the ones above) need \u00XX.
				printf -v hex '%d' "'$c"
				if ((hex < 0x20)); then
					printf -v out '%s\\u%04x' "$out" "$hex"
				else
					out+="$c"
				fi
				;;
		esac
	done
	printf '%s' "$out"
}

# Snapshot the current exported environment as `name<TAB>value` lines into
# the associative array referenced by name-ref $1. Bash 4.3+ supports
# `declare -n`; we already gated on bash 4+, so 4.0–4.2 callers would fail
# here — but those are vanishingly rare; if encountered the feature simply
# never emits.
__scribe_snapshot_env() {
	local -n __dest="$1"
	__dest=()
	local name
	while IFS= read -r name; do
		# Skip empty names and our own machinery.
		[[ -z "$name" || "$name" == _SCRIBE_* || "$name" == __scribe_* ]] && continue
		# Indirect expansion; ${!name} returns the value even under nounset.
		__dest["$name"]="${!name-}"
	done < <(compgen -e 2>/dev/null)
}

# Build a JSON object literal `{"NAME":"value",...}` from the current
# exported environment, writing to stdout. Skips names containing chars
# that JSON can't represent (none in practice — env names are POSIX-safe).
__scribe_build_added_json() {
	local -n __src="$1"
	local first=1 name esc_name esc_value
	printf '{'
	for name in "${!__src[@]}"; do
		esc_name=$(__scribe_json_escape "$name")
		esc_value=$(__scribe_json_escape "${__src[$name]}")
		if ((first)); then
			first=0
		else
			printf ','
		fi
		printf '"%s":"%s"' "$esc_name" "$esc_value"
	done
	printf '}'
}

# Compute the added/changed (object) and removed (array) JSON literals
# between the current snapshot ($1 nameref) and the cached snapshot ($2
# nameref), writing two NUL-separated payloads to stdout: added JSON,
# then NUL, then removed JSON.
__scribe_diff_env() {
	local -n __cur="$1"
	local -n __prev="$2"
	local first=1 name esc_name esc_value

	printf '{'
	for name in "${!__cur[@]}"; do
		# Only include if new or changed.
		if [[ -z "${__prev[$name]+x}" ]] || [[ "${__prev[$name]}" != "${__cur[$name]}" ]]; then
			esc_name=$(__scribe_json_escape "$name")
			esc_value=$(__scribe_json_escape "${__cur[$name]}")
			if ((first)); then
				first=0
			else
				printf ','
			fi
			printf '"%s":"%s"' "$esc_name" "$esc_value"
		fi
	done
	printf '}\0['

	first=1
	for name in "${!__prev[@]}"; do
		if [[ -z "${__cur[$name]+x}" ]]; then
			esc_name=$(__scribe_json_escape "$name")
			if ((first)); then
				first=0
			else
				printf ','
			fi
			printf '"%s"' "$esc_name"
		fi
	done
	printf ']'
}

# Per-prompt env-delta emit. Skips the helper invocation entirely when the
# diff is empty to save the ~15ms hook-helper cold-start cost.
__scribe_emit_env_delta() {
	((__SCRIBE_ENV_DELTA_ENABLED)) || return 0
	local -A __scribe_env_now=()
	__scribe_snapshot_env __scribe_env_now

	local payload added removed
	payload=$(__scribe_diff_env __scribe_env_now __scribe_env_last)
	added="${payload%%$'\0'*}"
	removed="${payload#*$'\0'}"

	# Skip the helper if both sides are empty literals.
	if [[ "$added" == '{}' && "$removed" == '[]' ]]; then
		return 0
	fi

	scribe-hook-helper --provider=system --event=env-delta \
		--added-json="$added" --removed-json="$removed" \
		</dev/null >/dev/null 2>&1 || true

	# Update the cache to the just-emitted state.
	__scribe_env_last=()
	for name in "${!__scribe_env_now[@]}"; do
		__scribe_env_last["$name"]="${__scribe_env_now[$name]}"
	done
}

# One-shot baseline emit: snapshot the current (post-rc, post-restore)
# exported env and tell the server "this is the StartupBaseline".
__scribe_emit_env_baseline() {
	((__SCRIBE_ENV_DELTA_ENABLED)) || return 0
	__scribe_snapshot_env __scribe_env_last
	local added
	added=$(__scribe_build_added_json __scribe_env_last)
	scribe-hook-helper --provider=system --event=env-delta \
		--added-json="$added" --removed-json='[]' --baseline-ready \
		</dev/null >/dev/null 2>&1 || true
}

__scribe_emit_env_baseline

# Register the per-prompt delta hook after the prompt's existing
# OSC-emitting __scribe_prompt_command — order keeps the OSC marks
# correct relative to the prompt text.
if ((__SCRIBE_ENV_DELTA_ENABLED)); then
	if [[ -z "${PROMPT_COMMAND:-}" ]]; then
		PROMPT_COMMAND="__scribe_emit_env_delta"
	elif [[ "${PROMPT_COMMAND}" != *"__scribe_emit_env_delta"* ]]; then
		PROMPT_COMMAND="${PROMPT_COMMAND};__scribe_emit_env_delta"
	fi
fi
