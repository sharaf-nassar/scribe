# Scribe shell integration — Nushell

if (($env.TERM_PROGRAM? | default '') != 'Scribe') {
    return
}
if (($env.SCRIBE_SHELL_INTEGRATION? | default '1') == '0') {
    return
}
if ($env._SCRIBE_INTEGRATION_SOURCED? | default false) {
    return
}
$env._SCRIBE_INTEGRATION_SOURCED = true

def __scribe-osc [payload: string] {
    print -n $'((char esc))]($payload)((char esc))\\'
}

def __scribe-sanitize-context [value: string] {
    $value
    | str replace -a "\n" ' '
    | str replace -a "\r" ' '
    | str replace -a ';' '_'
}

def __scribe-host-name [] {
    (hostname | str trim | __scribe-sanitize-context)
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
                tmux display-message -p '#S' | str trim | __scribe-sanitize-context
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

let prompt_end = $'((char esc))]133;B((char esc))\\'
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

# JSON-escape a single string. Returns the escaped form without
# surrounding quotes.
def __scribe-json-escape [value: string] {
    $value
    | str replace --all '\' '\\'
    | str replace --all '"' '\"'
    | str replace --all (char bs) '\b'
    | str replace --all (char ff) '\f'
    | str replace --all (char nl) '\n'
    | str replace --all (char cr) '\r'
    | str replace --all (char tab) '\t'
}

# Build a JSON object literal `{"NAME":"value",...}` from a record.
def __scribe-build-object [rec: record] {
    let entries = (
        $rec
        | columns
        | each {|name|
            let value = ($rec | get $name)
            let val_str = (if (($value | describe) == 'string') {
                $value
            } else {
                ($value | into string)
            })
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
# nushell — `into string` produces the joined form, which matches
# what other processes see on POSIX inheritance.
def __scribe-snapshot-env [] {
    let names = ($env | columns)
    $names
    | reduce --fold {} {|name, acc|
        if (($name | str starts-with '_SCRIBE_') or ($name | str starts-with '__scribe_') or ($name == 'config') or ($name == 'ENV_CONVERSIONS') or ($name == '__SCRIBE_ENV_LAST')) {
            $acc
        } else {
            let value = ($env | get $name)
            # Nushell may surface env vars as records or lists (when
            # `ENV_CONVERSIONS` is configured, e.g. PATH → list). Best-
            # effort string conversion; on any failure, treat as empty.
            let val_str = (try { $value | into string } catch { '' })
            $acc | upsert $name $val_str
        }
    }
}

# Best-effort apply of the server's POSIX-format restore-delta file.
# Recognizes `export NAME=value`, `export NAME='value'`, `export
# NAME="value"`, and `unset NAME` lines; ignores everything else.
def --env __scribe-apply-restore [path: string] {
    let lines = (try { open --raw $path | lines } catch { [] })
    mut adds = {}
    mut removes = []
    for line in $lines {
        let trimmed = ($line | str trim)
        if ($trimmed | str starts-with 'export ') {
            let body = ($trimmed | str substring 7..)
            let eq_idx = ($body | str index-of '=')
            if $eq_idx > 0 {
                let name = ($body | str substring 0..$eq_idx)
                let raw_value = ($body | str substring ($eq_idx + 1)..)
                # Strip a single layer of matching surrounding quotes
                # (single or double); leave others verbatim.
                let value = if (($raw_value | str starts-with "'") and ($raw_value | str ends-with "'") and (($raw_value | str length) >= 2)) {
                    $raw_value | str substring 1..(-1)
                } else if (($raw_value | str starts-with '"') and ($raw_value | str ends-with '"') and (($raw_value | str length) >= 2)) {
                    $raw_value | str substring 1..(-1)
                } else {
                    $raw_value
                }
                $adds = ($adds | upsert $name $value)
            }
        } else if ($trimmed | str starts-with 'unset ') {
            let name = ($trimmed | str substring 6.. | str trim)
            if not ($name | is-empty) {
                $removes = ($removes | append $name)
            }
        }
    }
    if (($adds | columns | length) > 0) {
        load-env $adds
    }
    for name in $removes {
        try { hide-env $name } catch { }
    }
}

if ('SCRIBE_RESTORE_ENV_DELTA_FILE' in $env) {
    let restore_path = $env.SCRIBE_RESTORE_ENV_DELTA_FILE
    if ($restore_path | path exists) {
        try { __scribe-apply-restore $restore_path } catch { }
        try { rm -p $restore_path } catch { }
    }
    hide-env SCRIBE_RESTORE_ENV_DELTA_FILE
}

# Per-session "last emitted" snapshot, stored as a global record.
$env.__SCRIBE_ENV_LAST = (__scribe-snapshot-env)

# Per-prompt delta hook. Diffs the current $env against the cached
# snapshot, emits via scribe-hook-helper only on non-empty change.
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
    let added_arg = $"--added-json=($added_json)"
    let removed_arg = $"--removed-json=($removed_json)"
    try {
        ^scribe-hook-helper --provider=system --event=env-delta $added_arg $removed_arg | complete | ignore
    } catch { }

    $env.__SCRIBE_ENV_LAST = $now
}

# One-shot baseline emit at the tail (post-rc + post-restore).
def --env __scribe-emit-env-baseline [] {
    let snapshot = (__scribe-snapshot-env)
    $env.__SCRIBE_ENV_LAST = $snapshot
    let added_json = (__scribe-build-object $snapshot)
    let added_arg = $"--added-json=($added_json)"
    try {
        ^scribe-hook-helper --provider=system --event=env-delta $added_arg --removed-json='[]' --baseline-ready | complete | ignore
    } catch { }
}

__scribe-emit-env-baseline

let env_delta_hooks = (__scribe-normalize-hooks ($env.config.hooks.pre_prompt? | default null))
$env.config = (
    $env.config
    | upsert hooks.pre_prompt ($env_delta_hooks | append {|| __scribe-emit-env-delta })
)
