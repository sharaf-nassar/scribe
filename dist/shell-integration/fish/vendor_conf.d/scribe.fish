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
#
# Both emits hand the JSON payload to the helper on stdin. argv is
# world-readable through /proc/<pid>/cmdline and one argument cannot
# exceed MAX_ARG_STRLEN (128 KiB), so the previous --added-json= form
# both exposed every exported value and silently dropped large
# environments.

# Source restore-delta file (FR-008: applied AFTER rc has run).
# The file contains `set -gx NAME 'value'` / `set -e NAME` lines that
# the server wrote as a fish-compatible apply script.
if set -q SCRIBE_RESTORE_ENV_DELTA_FILE
    and test -f "$SCRIBE_RESTORE_ENV_DELTA_FILE"
    builtin source "$SCRIBE_RESTORE_ENV_DELTA_FILE"
    rm -f "$SCRIBE_RESTORE_ENV_DELTA_FILE" 2>/dev/null
    set -e SCRIBE_RESTORE_ENV_DELTA_FILE
end

# Spawn-time persistence gate. The server exports SCRIBE_ENV_PERSIST=0 when
# `terminal.env_persistence.enabled` is off, and drops every EnvChanged it
# receives in that state — so the snapshot, the diff and the helper fork
# below would all be built for nothing. Everything past this point belongs
# to the env-delta feature, so return rather than branch. Absence means a
# server that predates the gate: keep emitting.
if set -q SCRIBE_ENV_PERSIST; and test "$SCRIBE_ENV_PERSIST" = "0"
    return 0
end

# Absolute path to the hook helper, injected by the server for whichever
# install layout is live. Packaged installs never put it on PATH, so the
# bare name is only a dev-shell fallback.
if set -q SCRIBE_HOOK_HELPER; and test -n "$SCRIBE_HOOK_HELPER"
    set -g __scribe_hook_helper $SCRIBE_HOOK_HELPER
else
    set -g __scribe_hook_helper scribe-hook-helper
end

# Per-session "last emitted" snapshot. Fish has no associative arrays,
# but its own variable table is a hash, so the cache lives in
# dynamically named globals: `__scribe_envm_<NAME>` holds the value last
# emitted for NAME. Every lookup is then O(1), which is what keeps the
# per-prompt diff O(N) — parallel name/value lists needed a `contains`
# scan of the whole cache per variable, i.e. O(N^2) per prompt.
#
# `__scribe_env_last_names` is the list of names the map currently holds;
# the removal sweep walks it, and the two are always updated together.
set -g __scribe_env_last_names

# JSON-escape a single string for embedding in a JSON object/array
# literal. Echoes the escaped form (no surrounding quotes). Only
# handles the canonical escapes \\, \", \b, \f, \n, \r, \t plus the
# common controls (whitespace family). Rare 0x00–0x1F codepoints in
# env values are extremely uncommon; if one slips through and the
# resulting JSON fails to parse server-side, the helper exits 0
# silently (FR-009 fail-open).
#
# Every stage funnels through `string collect` because fish command
# substitution otherwise yields ZERO elements for an empty string and
# one element PER LINE for a multi-line one. Callers concatenate the
# result, and concatenating a zero-element list is a cartesian product
# that collapses the whole accumulator, so the helper must always echo
# exactly one element.
function __scribe_json_escape
    # A `.` guard rides along through every stage: `string collect`
    # trims the newline `string replace` prints after each result, which
    # would otherwise eat a newline the value itself ended with. No
    # escape stage rewrites a period, so dropping the final character
    # restores the value exactly.
    #
    # Order matters: backslash first to avoid double-escaping the
    # replacements that follow.
    set -l s (string replace -a '\\' '\\\\' -- "$argv[1]." | string collect)
    set s (string replace -a '"' '\\"' -- "$s" | string collect)
    set s (string replace -a \b '\\b' -- "$s" | string collect)
    set s (string replace -a \f '\\f' -- "$s" | string collect)
    set s (string replace -a \n '\\n' -- "$s" | string collect)
    set s (string replace -a \r '\\r' -- "$s" | string collect)
    set s (string replace -a \t '\\t' -- "$s" | string collect)
    printf '%s' (string sub -s 1 -e -1 -- "$s") | string collect --allow-empty
end

# Names of the exported variables the delta tracks, in `set -nx` order.
# Scribe's own markers are dropped, and so is any name fish cannot spell
# as an identifier: indirect expansion stops at the first character
# outside `[A-Za-z0-9_]`, so a `BASH_FUNC_x%%` inherited from bash reads
# back as `%%` rather than as its value, and `set` refuses the same name
# as a cache key outright. One `string match` call filters the whole
# list, so the pass stays O(N) with a single builtin invocation.
function __scribe_env_names
    string match -r '^(?!__scribe_|_SCRIBE_)[A-Za-z0-9_]+$' -- (set -nx)
end

# Env-delta emit. One pass over the exported environment both diffs
# against the cached map and refreshes it, then a removal sweep runs
# only when the counts prove something is gone; the helper is skipped
# when the diff comes out empty. Called with `--baseline` the map is
# still empty, so the whole environment comes out as added and the emit
# carries `--baseline-ready` even if that set is empty. Both entry
# points share this one body, so the two payloads cannot drift apart in
# escaping or quoting.
#
# Every interpolation is double-quoted so a stray zero- or multi-element
# list can never turn a concatenation into a cartesian product that
# drops or duplicates the payload.
function __scribe_emit_env_delta --on-event fish_prompt
    set -l baseline 0
    if test "$argv[1]" = --baseline
        set baseline 1
    end

    set -l names (__scribe_env_names)
    set -l fresh_names
    set -l added
    set -l removed

    for name in $names
        # Indirect read, double-quoted. A list-valued export otherwise
        # expands to one element per component and an empty export to
        # none; quoting collapses either to exactly one element. The
        # quoted form is also the only one that reproduces the export
        # separator: fish joins a quoted list on the variable's own
        # delimiter — a colon for a path variable, a space for anything
        # else — which is exactly what it hands a child process, so
        # `PATH` is recorded as `a:b:c` and never as `a b c`.
        set -l value "$$name"
        set -l key __scribe_envm_$name
        if set -q $key
            if test "$$key" = "$value"
                continue
            end
        else
            set -a fresh_names $name
        end
        # `--unpath` because fish makes any variable whose name ends in
        # PATH a path variable, and the cache key inherits the tracked
        # variable's name: without it `__scribe_envm_PATH` would re-split
        # the recorded `a:b:c` into a list on every write.
        set -g --unpath $key "$value"
        set -l esc_name (__scribe_json_escape "$name")
        set -l esc_value (__scribe_json_escape "$value")
        set -a added "\"$esc_name\":\"$esc_value\""
    end

    # The map holds an entry for exactly `__scribe_env_last_names`, so
    # the names this pass found already in it — everything it saw minus
    # the fresh ones — equals the cached count precisely when nothing
    # was removed. An unchanged prompt therefore never walks the cached
    # list at all, and the sweep itself tests membership in the variable
    # table rather than scanning a list.
    if test (math (count $names) - (count $fresh_names)) -lt (count $__scribe_env_last_names)
        for name in $__scribe_env_last_names
            if not set -q -x $name
                set -l esc_name (__scribe_json_escape "$name")
                set -a removed "\"$esc_name\""
                set -e __scribe_envm_$name
            end
        end
    end

    set -g __scribe_env_last_names $names

    if test $baseline -eq 0
        and test (count $added) -eq 0
        and test (count $removed) -eq 0
        return 0
    end

    set -l added_json (string join ',' -- $added)
    set -l removed_json (string join ',' -- $removed)
    set -l baseline_flag
    if test $baseline -eq 1
        set baseline_flag --baseline-ready
    end

    # The payload goes over stdin, never argv: /proc/<pid>/cmdline is
    # world-readable, and a single argument is capped at MAX_ARG_STRLEN
    # (128 KiB), which turned a large delta into a silent E2BIG. `%s`
    # copies each argument verbatim, so backslashes and percent signs
    # inside the JSON survive untouched.
    printf '{"added":{%s},"removed":[%s]}' "$added_json" "$removed_json" \
        | $__scribe_hook_helper --provider=system --event=env_delta \
            --payload-stdin $baseline_flag >/dev/null 2>&1
    or true
end

# One-shot baseline emit at the tail (post-rc + post-restore).
__scribe_emit_env_delta --baseline
