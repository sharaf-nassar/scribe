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

    # Clear any stale Codex task label once control returns to the shell.
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
end

# ── Command end (OSC 133;D) ─────────────────────────────────────
function __scribe_fish_postexec --on-event fish_postexec
    set -g __scribe_last_status $status
end
