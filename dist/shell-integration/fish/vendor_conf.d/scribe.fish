# Scribe shell integration — fish
# Auto-loaded via XDG_DATA_DIRS/fish/vendor_conf.d/

# Guards
if not set -q TERM_PROGRAM; or test "$TERM_PROGRAM" != "Scribe"
    return 0
end
if set -q SCRIBE_SHELL_INTEGRATION; and test "$SCRIBE_SHELL_INTEGRATION" = "0"
    return 0
end
if set -q _SCRIBE_INTEGRATION_SOURCED
    return 0
end
set -g _SCRIBE_INTEGRATION_SOURCED 1

# ── Clean up XDG_DATA_DIRS ───────────────────────────────────────
# Remove the Scribe-prepended entry so child processes don't inherit it.
# The server prepended the shell-integration root directory.
if set -q XDG_DATA_DIRS
    set -l cleaned
    for dir in (string split ':' -- $XDG_DATA_DIRS)
        if not string match -q '*/shell-integration' -- "$dir"
            set -a cleaned $dir
        end
    end
    if test (count $cleaned) -gt 0
        set -gx XDG_DATA_DIRS (string join ':' -- $cleaned)
    else
        set -e XDG_DATA_DIRS
    end
end

# ── URL-encode helper ────────────────────────────────────────────
function __scribe_urlencode
    string escape --style=url -- $argv[1]
end

function __scribe_sanitize_context
    set -l value $argv[1]
    set value (string replace -a \n ' ' -- $value)
    set value (string replace -a \r ' ' -- $value)
    string replace -a ';' '_' -- $value
end

function __scribe_emit_context
    set -l remote 0
    if set -q SSH_CONNECTION; or set -q SSH_CLIENT; or set -q SSH_TTY
        set remote 1
    end

    set -l host (__scribe_sanitize_context (hostname 2>/dev/null))
    set -l tmux_session
    if set -q TMUX; and type -q tmux
        set tmux_session (__scribe_sanitize_context (tmux display-message -p '#S' 2>/dev/null))
    end

    printf '\e]1337;ScribeContext;remote=%s' $remote
    if test -n "$host"
        printf ';host=%s' $host
    end
    if test -n "$tmux_session"
        printf ';tmux=%s' $tmux_session
    end
    printf '\e\\'
end

# ── OSC sequence helpers ─────────────────────────────────────────
# Note: Fish uses \e for ESC and \\ for literal backslash.
# ST (String Terminator) = ESC \ = \e\\

# ── Prompt start (OSC 133;A + click_events=1) ───────────────────
function __scribe_fish_prompt --on-event fish_prompt
    # OSC 133;D — end of previous command (fish tracks $status internally)
    # Note: $__scribe_last_status is set by fish_postexec
    if set -q __scribe_last_status
        printf '\e]133;D;%d\e\\' $__scribe_last_status
    end

    # OSC 7 — report CWD
    printf '\e]7;file://%s%s\e\\' (hostname) (__scribe_urlencode "$PWD")

    # OSC 1337 — report remote host/tmux context
    __scribe_emit_context

    # Clear any stale provider task label once control returns to the shell.
    printf '\e]1337;CodexTaskLabelCleared\e\\'

    # OSC 2 — window title (basename of CWD)
    printf '\e]2;%s\e\\' (basename "$PWD")

    # OSC 133;A — prompt start
    printf '\e]133;A;click_events=1\e\\'
end

# ── Prompt end (OSC 133;B) ───────────────────────────────────────
# Fish doesn't have a direct "after prompt, before input" hook.
# We emit B at the end of fish_prompt since the prompt function's
# output IS the prompt text. After it returns, the cursor is at
# the input position.
function __scribe_fish_prompt_end --on-event fish_prompt
    printf '\e]133;B\e\\'
end

# ── Command start (OSC 133;C) ───────────────────────────────────
function __scribe_fish_preexec --on-event fish_preexec
    # OSC 133;C — command execution start
    printf '\e]133;C\e\\'

    # OSC 2 — update title with running command
    printf '\e]2;%s\e\\' $argv[1]

    # OSC 1337 ScribeAiLaunch — pre-arm Scribe's ED 3 filter when the user
    # runs an AI binary, so `<tool> --resume`'s pre-OSC-1337 \x1b[3J still
    # hits the filter even after ai_provider was cleared by the previous
    # 133;A on shell-prompt return.
    set -l __scribe_first_word (string split ' ' -- $argv[1])[1]
    set __scribe_first_word (string replace -r '.*/' '' -- $__scribe_first_word)
    switch $__scribe_first_word
        case claude
            printf '\e]1337;ScribeAiLaunch=claude_code\e\\'
        case codex
            printf '\e]1337;ScribeAiLaunch=codex_code\e\\'
    end
end

# ── Command end (OSC 133;D) ─────────────────────────────────────
function __scribe_fish_postexec --on-event fish_postexec
    set -g __scribe_last_status $status
end

# ── Env-delta capture (feature 006) ──────────────────────────────
# Three additions, in this order, per spec contract:
#   1. Source the restore-delta file if the server staged one (post-rc).
#   2. Initialize the per-session "last emitted" snapshot.
#   3. One-shot baseline emit (--baseline-ready), then register a
#      fish_prompt event handler that emits subsequent deltas.
#
# Helper invocations fail open: stdout/stderr discarded, exit code
# ignored via `or true`.

# Source restore-delta file (FR-008: applied AFTER rc has run).
# The file contains `set -gx NAME 'value'` / `set -e NAME` lines that
# the server wrote as a fish-compatible apply script.
if set -q SCRIBE_RESTORE_ENV_DELTA_FILE
    and test -f "$SCRIBE_RESTORE_ENV_DELTA_FILE"
    builtin source "$SCRIBE_RESTORE_ENV_DELTA_FILE"
    rm -f "$SCRIBE_RESTORE_ENV_DELTA_FILE" 2>/dev/null
    set -e SCRIBE_RESTORE_ENV_DELTA_FILE
end

# Per-session "last emitted" snapshot stored as two parallel lists.
# Fish has no associative arrays, so we use name/value lists indexed
# in lockstep.
set -g __scribe_env_last_names
set -g __scribe_env_last_values

# JSON-escape a single string for embedding in a JSON object/array
# literal. Echoes the escaped form (no surrounding quotes). Only
# handles the canonical escapes \\, \", \b, \f, \n, \r, \t plus the
# common controls (whitespace family). Rare 0x00–0x1F codepoints in
# env values are extremely uncommon; if one slips through and the
# resulting JSON fails to parse server-side, the helper exits 0
# silently (FR-009 fail-open).
function __scribe_json_escape
    set -l s $argv[1]
    # Order matters: backslash first to avoid double-escaping the
    # replacements that follow.
    set s (string replace -a '\\' '\\\\' -- $s)
    set s (string replace -a '"' '\\"' -- $s)
    set s (string replace -a \b '\\b' -- $s)
    set s (string replace -a \f '\\f' -- $s)
    set s (string replace -a \n '\\n' -- $s)
    set s (string replace -a \r '\\r' -- $s)
    set s (string replace -a \t '\\t' -- $s)
    printf '%s' $s
end

# Snapshot the current exported environment into two global lists:
# __scribe_env_snap_names and __scribe_env_snap_values (parallel).
# `set -nx` lists names of exported vars; `$$name` indirects the value.
function __scribe_snapshot_env
    set -g __scribe_env_snap_names
    set -g __scribe_env_snap_values
    for name in (set -nx)
        # Skip empty names and scribe-internal markers.
        switch $name
            case '' '__scribe_*' '_SCRIBE_*'
                continue
        end
        # Indirect read; fish lists are joined with space when expanded
        # into a string context, so use `string join` with NUL? No —
        # env values are strings; embed any list elements joined by
        # space (which matches POSIX shell semantics for arrays).
        set -l value (string collect -- $$name)
        if test -z "$value"
            set value ''
        end
        set -ga __scribe_env_snap_names $name
        set -ga __scribe_env_snap_values $value
    end
end

# Build a JSON object literal `{"NAME":"value",...}` from the two
# parallel lists $argv[1] (names) and $argv[2] (values). Fish can't
# pass lists by reference, so we use indirect variable names.
function __scribe_build_added_json
    set -l names_var $argv[1]
    set -l values_var $argv[2]
    set -l names $$names_var
    set -l values $$values_var
    set -l count (count $names)
    set -l out '{'
    set -l first 1
    for i in (seq 1 $count)
        set -l name $names[$i]
        set -l value $values[$i]
        set -l esc_name (__scribe_json_escape $name)
        set -l esc_value (__scribe_json_escape $value)
        if test $first -eq 1
            set first 0
        else
            set out $out','
        end
        set out $out'"'$esc_name'":"'$esc_value'"'
    end
    set out $out'}'
    printf '%s' $out
end

# Per-prompt env-delta emit. Skips the helper invocation when the diff
# is empty.
function __scribe_emit_env_delta --on-event fish_prompt
    __scribe_snapshot_env

    # Build added (object) and removed (array) JSON literals by
    # diffing the current snapshot against the cached last-emitted.
    set -l added '{'
    set -l removed '['
    set -l first_added 1
    set -l first_removed 1
    set -l now_count (count $__scribe_env_snap_names)
    set -l last_count (count $__scribe_env_last_names)

    for i in (seq 1 $now_count)
        set -l name $__scribe_env_snap_names[$i]
        set -l value $__scribe_env_snap_values[$i]
        set -l prev_idx (contains -i -- $name $__scribe_env_last_names)
        set -l changed 1
        if test -n "$prev_idx"
            if test "$__scribe_env_last_values[$prev_idx]" = "$value"
                set changed 0
            end
        end
        if test $changed -eq 1
            set -l esc_name (__scribe_json_escape $name)
            set -l esc_value (__scribe_json_escape $value)
            if test $first_added -eq 1
                set first_added 0
            else
                set added $added','
            end
            set added $added'"'$esc_name'":"'$esc_value'"'
        end
    end
    set added $added'}'

    if test $last_count -gt 0
        for i in (seq 1 $last_count)
            set -l name $__scribe_env_last_names[$i]
            if not contains -- $name $__scribe_env_snap_names
                set -l esc_name (__scribe_json_escape $name)
                if test $first_removed -eq 1
                    set first_removed 0
                else
                    set removed $removed','
                end
                set removed $removed'"'$esc_name'"'
            end
        end
    end
    set removed $removed']'

    if test "$added" = '{}'
        and test "$removed" = '[]'
        return 0
    end

    scribe-hook-helper --provider=system --event=env-delta \
        --added-json=$added --removed-json=$removed \
        </dev/null >/dev/null 2>&1
    or true

    # Update the cache to the just-emitted state.
    set -g __scribe_env_last_names $__scribe_env_snap_names
    set -g __scribe_env_last_values $__scribe_env_snap_values
end

# One-shot baseline emit at the tail (post-rc + post-restore).
function __scribe_emit_env_baseline
    __scribe_snapshot_env
    set -g __scribe_env_last_names $__scribe_env_snap_names
    set -g __scribe_env_last_values $__scribe_env_snap_values
    set -l added (__scribe_build_added_json __scribe_env_last_names __scribe_env_last_values)
    scribe-hook-helper --provider=system --event=env-delta \
        --added-json=$added --removed-json='[]' --baseline-ready \
        </dev/null >/dev/null 2>&1
    or true
end

__scribe_emit_env_baseline
