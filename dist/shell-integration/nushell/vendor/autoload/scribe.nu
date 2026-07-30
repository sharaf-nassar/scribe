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

# Nushell found this file because the server prepended the
# shell-integration root to `XDG_DATA_DIRS`, and autoload is over by the
# time the script runs. Take that entry back out: left in place it is
# inherited by every child the session spawns, and the env baseline below
# records it as if the user had exported it, so the server would restore
# a server-private path into later sessions on a shell that never wanted
# it. Matched on the trailing `/shell-integration` component, the way
# `scribe.fish` matches it — all four install layouts end the scripts
# directory with that name.
def --env __scribe-strip-injected-data-dirs [] {
    if not ('XDG_DATA_DIRS' in $env) {
        return
    }
    # `ENV_CONVERSIONS` can turn any variable into a list on the nushell
    # side, so only split when the value is still the raw string a child
    # process inherits.
    let raw = $env.XDG_DATA_DIRS
    let entries = if (($raw | describe) | str starts-with 'list') {
        ($raw | each {|entry| $entry | into string })
    } else {
        ($raw | into string | split row (char esep))
    }
    let kept = ($entries | where {|entry| not ($entry | str ends-with '/shell-integration') })
    if ($kept | length) == ($entries | length) {
        return
    }
    if ($kept | is-empty) {
        hide-env XDG_DATA_DIRS
    } else {
        $env.XDG_DATA_DIRS = ($kept | str join (char esep))
    }
}

if $scribe_active {
    $env._SCRIBE_INTEGRATION_SOURCED = true
    __scribe-strip-injected-data-dirs
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

# Build a JSON object literal `{"NAME":"value",...}` from a table of
# already-stringified `{name, value}` pairs.
def __scribe-build-object [pairs: list] {
    let entries = (
        $pairs
        | each {|pair|
            $'"(__scribe-json-escape $pair.name)":"(__scribe-json-escape $pair.value)"'
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

# Snapshot the current $env as a table of `{name, value}` pairs. Skips
# scribe-internal markers and the nushell-internal `config` record
# (which is not an exported env var). `PATH` is represented as a list in
# nushell; `__scribe-env-string` joins it back into the form other
# processes see on POSIX inheritance.
#
# `items` walks the record once and hands each value straight to the
# closure. The accumulating `reduce`/`upsert` this replaced rebuilt the
# whole record per variable and read each value back out by name, so the
# snapshot alone was O(N^2) before the diff even started.
def __scribe-snapshot-env [] {
    $env
    | items {|name, value|
        if (($name | str starts-with '_SCRIBE_') or ($name | str starts-with '__scribe_') or ($name == 'config') or ($name == 'ENV_CONVERSIONS') or ($name == '__SCRIBE_ENV_LAST')) {
            null
        } else {
            # Best-effort string conversion; on any failure, treat as
            # empty rather than aborting the whole snapshot.
            {name: $name, value: (try { __scribe-env-string $value } catch { '' })}
        }
    }
    | compact
}

# Diff two snapshots into `{added: table<name, value>, removed: list}`.
#
# Both sides are bucketed by name in one `group-by`, which hashes, so the
# whole diff is O(N). Testing `$name in $prev_names` and then reading
# `$prev | get $name` per variable — nushell scans a list and a record
# linearly for both — made this O(N^2) instead. The `side` column is what
# tells a lone row apart: 0 means the name only exists in the previous
# snapshot (removed), 1 that it only exists in the current one (added).
def __scribe-diff-env [prev: list, now: list] {
    let paired = (
        (($prev | insert side 0) ++ ($now | insert side 1))
        | group-by name
    )
    mut added = []
    mut removed = []
    for bucket in ($paired | items {|name, rows| {name: $name, rows: $rows} }) {
        let rows = $bucket.rows
        if ($rows | length) == 1 {
            let row = ($rows | first)
            if $row.side == 1 {
                $added = ($added | append {name: $bucket.name, value: $row.value})
            } else {
                $removed = ($removed | append $bucket.name)
            }
        } else if ($rows | first | get value) != ($rows | last | get value) {
            let current = ($rows | where side == 1 | first)
            $added = ($added | append {name: $bucket.name, value: $current.value})
        }
    }
    {added: $added, removed: $removed}
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

# Spawn-time persistence gate. The server exports SCRIBE_ENV_PERSIST=0 when
# `terminal.env_persistence.enabled` is off, and drops every EnvChanged it
# receives in that state — so the snapshot, the diff and the helper fork
# below would all be built for nothing. A bare `return` is illegal at
# this level (see the header), so gate the side effects instead; the `def`s
# stay inert. Absence means a server that predates the gate: keep emitting.
let scribe_env_persist = (
    $scribe_active and (($env.SCRIBE_ENV_PERSIST? | default '1') != '0')
)

# Per-prompt delta hook. Diffs the current $env against the cached
# snapshot, emits via the resolved hook helper only on non-empty change.
def --env __scribe-emit-env-delta [] {
    let now = (__scribe-snapshot-env)
    let prev = ($env.__SCRIBE_ENV_LAST? | default [])
    let delta = (__scribe-diff-env $prev $now)

    if (($delta.added | is-empty) and ($delta.removed | is-empty)) {
        return
    }

    let added_json = (__scribe-build-object $delta.added)
    let removed_json = (__scribe-build-array $delta.removed)
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

if $scribe_env_persist {
    __scribe-emit-env-baseline

    let env_delta_hooks = (__scribe-normalize-hooks ($env.config.hooks.pre_prompt? | default null))
    $env.config = (
        $env.config
        | upsert hooks.pre_prompt ($env_delta_hooks | append {|| __scribe-emit-env-delta })
    )
}
