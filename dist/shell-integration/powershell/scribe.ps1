# Scribe shell integration — PowerShell
# Loaded with `pwsh -NoExit -File` or `powershell -NoExit -File`.

if ($env:TERM_PROGRAM -ne 'Scribe') {
    return
}
if (($env:SCRIBE_SHELL_INTEGRATION ?? '1') -eq '0') {
    return
}
if ($global:_SCRIBE_INTEGRATION_SOURCED) {
    return
}
$global:_SCRIBE_INTEGRATION_SOURCED = $true

$script:ScribeEsc = [string][char]0x1b
$script:ScribeSt = "$($script:ScribeEsc)\"
$script:ScribePromptEnd = "$($script:ScribeEsc)]133;B$($script:ScribeSt)"
$script:ScribeOriginalPrompt = if (Test-Path Function:\prompt) {
    (Get-Item Function:\prompt).ScriptBlock
} else {
    $null
}

function global:__Scribe-Osc {
    param([string]$Payload)

    Write-Host -NoNewline "$($script:ScribeEsc)]$Payload$($script:ScribeSt)"
}

function global:__Scribe-SanitizeContext {
    param([AllowNull()][string]$Value)

    if ([string]::IsNullOrEmpty($Value)) {
        return ''
    }

    return $Value.Replace("`r", ' ').Replace("`n", ' ').Replace(';', '_')
}

function global:__Scribe-EncodePath {
    param([string]$Path)

    $normalized = $Path.Replace('\', '/')
    if ($normalized -match '^[A-Za-z]:/') {
        $normalized = "/$normalized"
    }

    return [System.Uri]::EscapeDataString($normalized).Replace('%2F', '/')
}

function global:__Scribe-HostName {
    if (-not [string]::IsNullOrWhiteSpace($env:HOSTNAME)) {
        return (__Scribe-SanitizeContext $env:HOSTNAME)
    }
    if (-not [string]::IsNullOrWhiteSpace($env:COMPUTERNAME)) {
        return (__Scribe-SanitizeContext $env:COMPUTERNAME)
    }
    return (__Scribe-SanitizeContext ([System.Net.Dns]::GetHostName()))
}

function global:__Scribe-EmitContext {
    $remote = if ($env:SSH_CONNECTION -or $env:SSH_CLIENT -or $env:SSH_TTY) { 1 } else { 0 }
    $host = __Scribe-HostName
    $tmuxSession = ''

    if ($env:TMUX -and (Get-Command tmux -ErrorAction SilentlyContinue)) {
        $tmuxSession = __Scribe-SanitizeContext (tmux display-message -p '#S' 2>$null)
    }

    $payload = "1337;ScribeContext;remote=$remote"
    if (-not [string]::IsNullOrWhiteSpace($host)) {
        $payload += ";host=$host"
    }
    if (-not [string]::IsNullOrWhiteSpace($tmuxSession)) {
        $payload += ";tmux=$tmuxSession"
    }

    __Scribe-Osc $payload
}

function global:prompt {
    $exitCode = if ($LASTEXITCODE -is [int]) { [int]$LASTEXITCODE } elseif ($?) { 0 } else { 1 }
    $cwd = (Get-Location).Path
    $title = Split-Path -Leaf $cwd

    __Scribe-Osc "133;D;$exitCode"
    __Scribe-Osc "7;file://$(__Scribe-HostName)$(__Scribe-EncodePath $cwd)"
    __Scribe-EmitContext
    __Scribe-Osc '1337;CodexTaskLabelCleared'
    __Scribe-Osc "2;$(__Scribe-SanitizeContext $title)"
    __Scribe-Osc '133;A;click_events=1'

    $promptText = if ($script:ScribeOriginalPrompt) { & $script:ScribeOriginalPrompt } else { "PS $(Get-Location)> " }
    if ($null -eq $promptText) {
        $promptText = ''
    }

    return "$promptText$($script:ScribePromptEnd)"
}

if (Get-Command Set-PSReadLineOption -ErrorAction SilentlyContinue) {
    Set-PSReadLineOption -CommandValidationHandler {
        param([System.Management.Automation.Language.CommandAst]$CommandAst)

        if ($null -eq $CommandAst) {
            return
        }

        __Scribe-Osc '133;C'
        $commandText = [string]$CommandAst.Extent.Text
        if (-not [string]::IsNullOrWhiteSpace($commandText)) {
            __Scribe-Osc "2;$(__Scribe-SanitizeContext $commandText)"

            # OSC 1337 ScribeAiLaunch — pre-arm Scribe's ED 3 filter when
            # the user runs an AI binary, so `<tool> --resume`'s
            # pre-OSC-1337 ESC [3J still hits the filter even after
            # ai_provider was cleared by the previous 133;A.
            $firstWord = ($commandText.Trim() -split '\s+', 2)[0]
            if (-not [string]::IsNullOrWhiteSpace($firstWord)) {
                $firstWord = [System.IO.Path]::GetFileName($firstWord)
                switch ($firstWord) {
                    'claude' { __Scribe-Osc '1337;ScribeAiLaunch=claude_code' }
                    'codex' { __Scribe-Osc '1337;ScribeAiLaunch=codex_code' }
                }
            }
        }
    }
    Set-PSReadLineKeyHandler -Chord Enter -Function ValidateAndAcceptLine
}

# ── Env-delta capture (feature 006) ──────────────────────────────
# Three additions, in this order, per spec contract:
#   1. Source the restore-delta file if the server staged one (post-
#      profile, so user-set values from the previous session beat any
#      profile-driven defaults). The file is dot-sourced so its
#      `$env:NAME = 'value'` / `Remove-Item env:NAME` statements
#      affect the current process environment.
#   2. Initialize the per-session "last emitted" snapshot.
#   3. One-shot baseline emit (--baseline-ready), then re-wrap the
#      `prompt` function to emit per-prompt deltas.

# Source restore-delta file (FR-008: applied AFTER profile has run).
if ($env:SCRIBE_RESTORE_ENV_DELTA_FILE -and (Test-Path $env:SCRIBE_RESTORE_ENV_DELTA_FILE)) {
    try {
        . $env:SCRIBE_RESTORE_ENV_DELTA_FILE
    } catch {
        # Fail open — keep the terminal usable even if the file format
        # was unexpected.
    }
    try {
        Remove-Item -Path $env:SCRIBE_RESTORE_ENV_DELTA_FILE -ErrorAction SilentlyContinue
    } catch { }
    Remove-Item env:SCRIBE_RESTORE_ENV_DELTA_FILE -ErrorAction SilentlyContinue
}

# Per-session "last emitted" snapshot as a hashtable (global so the
# globally-scoped emit functions defined below can read and update it
# uniformly across invocations from the wrapped `prompt` function).
$global:ScribeEnvLast = @{}

function global:__Scribe-SnapshotEnv {
    $snap = @{}
    foreach ($entry in [Environment]::GetEnvironmentVariables('Process').GetEnumerator()) {
        $name = [string]$entry.Key
        if ([string]::IsNullOrEmpty($name)) { continue }
        # Skip scribe-internal markers.
        if ($name.StartsWith('_SCRIBE_') -or $name.StartsWith('__scribe_')) { continue }
        $snap[$name] = [string]$entry.Value
    }
    return $snap
}

function global:__Scribe-HashToJson {
    param([hashtable]$Table)

    if ($Table.Count -eq 0) {
        return '{}'
    }
    # PowerShell's ConvertTo-Json handles all JSON-escaping (backslash,
    # quote, control chars) and produces a compact single-line object
    # when -Compress is given. Use -Depth 2 since values are strings
    # (depth 1) and the wrapper is the object (depth 0).
    return ($Table | ConvertTo-Json -Compress -Depth 2)
}

function global:__Scribe-ArrayToJson {
    param([string[]]$Names)

    if ($null -eq $Names -or $Names.Count -eq 0) {
        return '[]'
    }
    # Wrap in @() so a single-element array stays an array post-
    # serialization (ConvertTo-Json unwraps scalars otherwise).
    return (,$Names | ConvertTo-Json -Compress -Depth 1)
}

function global:__Scribe-EmitEnvDelta {
    $now = __Scribe-SnapshotEnv
    $prev = $global:ScribeEnvLast

    $added = @{}
    foreach ($entry in $now.GetEnumerator()) {
        $name = [string]$entry.Key
        $value = [string]$entry.Value
        if ($prev.ContainsKey($name)) {
            if ($prev[$name] -ceq $value) { continue }
        }
        $added[$name] = $value
    }
    $removedList = New-Object System.Collections.Generic.List[string]
    foreach ($name in $prev.Keys) {
        if (-not $now.ContainsKey([string]$name)) {
            $removedList.Add([string]$name)
        }
    }

    if ($added.Count -eq 0 -and $removedList.Count -eq 0) {
        return
    }

    $addedJson = __Scribe-HashToJson $added
    $removedJson = __Scribe-ArrayToJson $removedList.ToArray()

    try {
        & scribe-hook-helper --provider=system --event=env-delta `
            "--added-json=$addedJson" "--removed-json=$removedJson" `
            2>$null | Out-Null
    } catch {
        # Helper missing or any other failure: stay silent.
    }

    $global:ScribeEnvLast = $now
}

function global:__Scribe-EmitEnvBaseline {
    $snap = __Scribe-SnapshotEnv
    $global:ScribeEnvLast = $snap
    $addedJson = __Scribe-HashToJson $snap
    try {
        & scribe-hook-helper --provider=system --event=env-delta `
            "--added-json=$addedJson" --removed-json='[]' --baseline-ready `
            2>$null | Out-Null
    } catch { }
}

__Scribe-EmitEnvBaseline

# Wrap the existing prompt function so env-delta is emitted on each
# prompt return, immediately after the OSC marks. The original prompt
# function was installed above; capture it once, then redefine.
$script:ScribePromptInner = (Get-Item Function:\prompt).ScriptBlock
function global:prompt {
    $promptText = & $script:ScribePromptInner
    try {
        __Scribe-EmitEnvDelta
    } catch {
        # Never let the env-delta machinery break the prompt.
    }
    return $promptText
}
