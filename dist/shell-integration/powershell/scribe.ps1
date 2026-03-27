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
        }
    }
    Set-PSReadLineKeyHandler -Chord Enter -Function ValidateAndAcceptLine
}
