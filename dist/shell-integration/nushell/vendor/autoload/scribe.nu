# Scribe shell integration — Nushell

# This file is evaluated as a script from nushell's vendor autoload
# directory, and `return` is only legal inside a custom command or a
# closure — a bare top-level `return` aborts with "Return used outside
# of custom command or closure" every time a guard trips, which is
# exactly the common case (nu started outside Scribe). Gate once here
# and skip the side effects instead; the `def`s below are inert.
let scribe_active = (
    (($env.TERM_PROGRAM? | default '') == 'Scribe')
    and (($env.SCRIBE_SHELL_INTEGRATION? | default '1') != '0')
    and (not ($env._SCRIBE_INTEGRATION_SOURCED? | default false))
)

if $scribe_active {
    $env._SCRIBE_INTEGRATION_SOURCED = true
}

# `char esc` was dropped from nushell's character table, and `$'...'`
# interpolation does not process backslash escapes — the ST terminator
# has to come from a `$"..."` string so `\\` collapses to one backslash.
def __scribe-osc [payload: string] {
    print -n $"\u{1b}]($payload)\u{1b}\\"
}

def __scribe-sanitize-context [value: string] {
    $value
    | str replace -a "\n" ' '
    | str replace -a "\r" ' '
    | str replace -a ';' '_'
}

def __scribe-host-name [] {
    (__scribe-sanitize-context (hostname | str trim))
}

def __scribe-emit-context [] {
    let remote = if (
        (($env.SSH_CONNECTION? | default '') != '')
        or (($env.SSH_CLIENT? | default '') != '')
        or (($env.SSH_TTY? | default '') != '')
    ) {
        '1'
    } else {
        '0'
    }

    let host = (__scribe-host-name)
    mut payload = $"1337;ScribeContext;remote=($remote)"
    if not ($host | is-empty) {
        $payload = $"($payload);host=($host)"
    }

    if (($env.TMUX? | default '') != '') {
        let tmux_session = (
            try {
                __scribe-sanitize-context (tmux display-message -p '#S' | str trim)
            } catch {
                ''
            }
        )
        if not ($tmux_session | is-empty) {
            $payload = $"($payload);tmux=($tmux_session)"
        }
    }

    __scribe-osc $payload
}

def __scribe-pre-prompt [] {
    let host = (__scribe-host-name)
    let cwd = ($env.PWD | path expand | into string)
    let encoded_cwd = (
        $cwd
        | str replace -a '\' '/'
        | url encode
    )

    __scribe-osc $"133;D;($env.LAST_EXIT_CODE? | default 0)"
    __scribe-osc $"7;file://($host)($encoded_cwd)"
    __scribe-emit-context
    __scribe-osc '1337;CodexTaskLabelCleared'
    __scribe-osc $"2;(($env.PWD | path basename | into string))"
    __scribe-osc '133;A;click_events=1'
}

def __scribe-pre-exec [] {
    let command = (commandline)
    __scribe-osc '133;C'
    if not ($command | is-empty) {
        __scribe-osc $"2;((__scribe-sanitize-context $command))"

        # OSC 1337 ScribeAiLaunch — pre-arm Scribe's ED 3 filter when the
        # user runs an AI binary, so `<tool> --resume`'s pre-OSC-1337
        # \x1b[3J still hits the filter even after ai_provider was cleared
        # by the previous 133;A on shell-prompt return.
        let first_word = (
            $command
            | str trim
            | split row ' '
            | get 0?
            | default ''
            | path basename
        )
        match $first_word {
            'claude' => { __scribe-osc '1337;ScribeAiLaunch=claude_code' }
            'codex' => { __scribe-osc '1337;ScribeAiLaunch=codex_code' }
            _ => {}
        }
    }
}

def __scribe-normalize-hooks [hooks] {
    if $hooks == null {
        []
    } else if (($hooks | describe) == 'closure') {
        [$hooks]
    } else {
        $hooks
    }
}

if $scribe_active {
    let prompt_end = "\u{1b}]133;B\u{1b}\\"
    $env.PROMPT_INDICATOR = $"(($env.PROMPT_INDICATOR? | default ''))($prompt_end)"
    $env.PROMPT_INDICATOR_VI_INSERT = $"(($env.PROMPT_INDICATOR_VI_INSERT? | default ''))($prompt_end)"
    $env.PROMPT_INDICATOR_VI_NORMAL = $"(($env.PROMPT_INDICATOR_VI_NORMAL? | default ''))($prompt_end)"
    $env.PROMPT_MULTILINE_INDICATOR = $"(($env.PROMPT_MULTILINE_INDICATOR? | default ''))($prompt_end)"

    let pre_prompt_hooks = (__scribe-normalize-hooks ($env.config.hooks.pre_prompt? | default null))
    let pre_execution_hooks = (__scribe-normalize-hooks ($env.config.hooks.pre_execution? | default null))

    $env.config = (
        $env.config
        | upsert hooks.pre_prompt ($pre_prompt_hooks | append {|| __scribe-pre-prompt })
        | upsert hooks.pre_execution ($pre_execution_hooks | append {|| __scribe-pre-exec })
    )
}

# ── Env-delta capture (feature 006) ──────────────────────────────
# Three additions, in this order, per spec contract:
#   1. Best-effort apply of the restore-delta file the server staged.
#      Nushell's `source` is parse-time only, so we parse the POSIX
#      `export NAME=value` / `unset NAME` lines manually and apply
#      via `load-env` / `hide-env`. If the file format is unfamiliar
#      we skip (FR-010 graceful degradation).
#   2. Initialize the per-session "last emitted" snapshot.
#   3. One-shot baseline emit (--baseline-ready), then register a
#      pre_prompt hook that emits subsequent deltas.
#
# Both emits hand the JSON payload to the helper on stdin. argv is
# world-readable through /proc/<pid>/cmdline and one argument cannot
# exceed MAX_ARG_STRLEN (128 KiB), so the previous --added-json= form
# both exposed every exported value and silently dropped large
# environments.

# JSON-escape a single string. Returns the escaped form without
# surrounding quotes.
def __scribe-json-escape [value: string] {
    let short = (
        $value
        | str replace --all '\' '\\'
        | str replace --all '"' '\"'
        | str replace --all "\u{08}" '\b'
        | str replace --all "\u{0c}" '\f'
        | str replace --all (char nl) '\n'
        | str replace --all (char cr) '\r'
        | str replace --all (char tab) '\t'
    )
    # JSON forbids raw C0 controls, and the ones without a short escape
    # do occur here — Scribe appends OSC sequences to `PROMPT_INDICATOR`
    # above, so the baseline snapshot carries ESC. One malformed value
    # would make the server reject the whole payload, so spell the
    # remainder as `\u00XX`. Guarded because the scan is per character.
    if ($short =~ '[\x00-\x1f]') {
        $short
        | split chars
        | each {|ch|
            if ($ch =~ '[\x00-\x1f]') {
                $"\\u00($ch | into binary | encode hex)"
            } else {
                $ch
            }
        }
        | str join ''
    } else {
        $short
    }
}

# Render one env value the way a child process sees it. Nushell hands
# back lists for path-shaped vars (PATH, and anything in
# `ENV_CONVERSIONS`); `into string` maps over a list element-wise rather
# than joining, so join explicitly with the platform separator.
def __scribe-env-string [value] {
    let kind = ($value | describe)
    if $kind == 'string' {
        $value
    } else if ($kind | str starts-with 'list') {
        ($value | each {|item| $item | into string } | str join (char esep))
    } else {
        (try { $value | into string } catch { '' })
    }
}

# Build a JSON object literal `{"NAME":"value",...}` from a record.
def __scribe-build-object [rec: record] {
    let entries = (
        $rec
        | columns
        | each {|name|
            let val_str = (__scribe-env-string ($rec | get $name))
            $'"(__scribe-json-escape $name)":"(__scribe-json-escape $val_str)"'
        }
    )
    $"{($entries | str join ',')}"
}

# Build a JSON array literal `["NAME",...]` from a list of strings.
def __scribe-build-array [names: list<string>] {
    let entries = (
        $names | each {|name| $'"(__scribe-json-escape $name)"' }
    )
    $"[($entries | str join ',')]"
}

# Snapshot the current $env as a record of strings. Skips scribe-
# internal markers and the nushell-internal `config` record (which is
# not an exported env var). `PATH` is represented as a list in
# nushell; `__scribe-env-string` joins it back into the form other
# processes see on POSIX inheritance.
def __scribe-snapshot-env [] {
    let names = ($env | columns)
    $names
    | reduce --fold {} {|name, acc|
        if (($name | str starts-with '_SCRIBE_') or ($name | str starts-with '__scribe_') or ($name == 'config') or ($name == 'ENV_CONVERSIONS') or ($name == '__SCRIBE_ENV_LAST')) {
            $acc
        } else {
            # Best-effort string conversion; on any failure, treat as
            # empty rather than aborting the whole snapshot.
            let val_str = (try { __scribe-env-string ($env | get $name) } catch { '' })
            $acc | upsert $name $val_str
        }
    }
}

# Apply the server's restore-delta file, which nushell alone receives as
# JSON: `source` resolves its path at parse time and rejects a runtime
# value, so this script can never dot-source anything and has to read the
# delta as data. The previous hand-rolled POSIX-line parser silently lost
# the `'\''` quote idiom and every value that spanned more than one line;
# `from json` has no such blind spots.
def --env __scribe-apply-restore [path: string] {
    let payload = (try { open --raw $path | from json } catch { null })
    if (($payload | is-empty) or (not ($payload | describe | str starts-with 'record'))) {
        return
    }
    let adds = ($payload.added? | default {})
    if (($adds | describe | str starts-with 'record') and (($adds | columns | length) > 0)) {
        load-env $adds
    }
    let removes = ($payload.removed? | default [])
    if ($removes | describe | str starts-with 'list') {
        for name in $removes {
            try { hide-env $name } catch { }
        }
    }
}

if $scribe_active and ('SCRIBE_RESTORE_ENV_DELTA_FILE' in $env) {
    let restore_path = $env.SCRIBE_RESTORE_ENV_DELTA_FILE
    if ($restore_path | path exists) {
        try { __scribe-apply-restore $restore_path } catch { }
        try { rm -p $restore_path } catch { }
    }
    hide-env SCRIBE_RESTORE_ENV_DELTA_FILE
}

# Per-session "last emitted" snapshot, stored as a global record.
if $scribe_active {
    $env.__SCRIBE_ENV_LAST = (__scribe-snapshot-env)
}

# Per-prompt delta hook. Diffs the current $env against the cached
# snapshot, emits via the resolved hook helper only on non-empty change.
def --env __scribe-emit-env-delta [] {
    let now = (__scribe-snapshot-env)
    let prev = ($env.__SCRIBE_ENV_LAST? | default {})
    let now_names = ($now | columns)
    let prev_names = ($prev | columns)

    mut added = {}
    for name in $now_names {
        let cur_val = ($now | get $name)
        let prev_val = (if ($name in $prev_names) { $prev | get $name } else { null })
        if $prev_val == null or $prev_val != $cur_val {
            $added = ($added | upsert $name $cur_val)
        }
    }
    let removed = (
        $prev_names | where {|name| not ($name in $now_names) }
    )

    if (($added | columns | length) == 0) and (($removed | length) == 0) {
        return
    }

    let added_json = (__scribe-build-object $added)
    let removed_json = (__scribe-build-array $removed)
    # The payload goes over stdin, never argv: /proc/<pid>/cmdline is
    # world-readable, and a single argument is capped at MAX_ARG_STRLEN
    # (128 KiB), which turned a large delta into a silent E2BIG.
    let payload = $'{"added":($added_json),"removed":($removed_json)}'
    let helper = ($env.SCRIBE_HOOK_HELPER? | default "scribe-hook-helper")
    try {
        $payload | ^$helper --provider=system --event=env_delta --payload-stdin | complete | ignore
    } catch { }

    $env.__SCRIBE_ENV_LAST = $now
}

# One-shot baseline emit at the tail (post-rc + post-restore).
def --env __scribe-emit-env-baseline [] {
    let snapshot = (__scribe-snapshot-env)
    $env.__SCRIBE_ENV_LAST = $snapshot
    let added_json = (__scribe-build-object $snapshot)
    let payload = $'{"added":($added_json),"removed":[]}'
    let helper = ($env.SCRIBE_HOOK_HELPER? | default "scribe-hook-helper")
    try {
        $payload | ^$helper --provider=system --event=env_delta --payload-stdin --baseline-ready | complete | ignore
    } catch { }
}

if $scribe_active {
    __scribe-emit-env-baseline

    let env_delta_hooks = (__scribe-normalize-hooks ($env.config.hooks.pre_prompt? | default null))
    $env.config = (
        $env.config
        | upsert hooks.pre_prompt ($env_delta_hooks | append {|| __scribe-emit-env-delta })
    )
}
