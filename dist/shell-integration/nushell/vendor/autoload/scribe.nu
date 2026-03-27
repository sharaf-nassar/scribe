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
