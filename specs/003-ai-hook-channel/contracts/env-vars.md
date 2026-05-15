# Contract: Environment Variables

**Scope**: The two environment variables Scribe injects into every PTY it spawns. These are the **sole** discovery mechanism for the hook channel.

## Variables

### `SCRIBE_HOOK_SOCK`

| Field | Value |
|---|---|
| Set by | `scribe-server`, at PTY spawn time, inside `build_pty_options` (`crates/scribe-server/src/session_manager.rs:538`) |
| Read by | `scribe-hook-helper` |
| Type | absolute filesystem path |
| Value | the absolute path of the running `scribe-server`'s Unix domain socket (the existing `server.sock`, e.g. `/run/user/1000/scribe/server.sock`) |
| Set in | base PTY env, propagates through user shell + AI tool to hook subprocess |
| Unset means | "not running under Scribe" → helper exits 0 silently (FR-003) |

### `SCRIBE_SESSION_ID`

| Field | Value |
|---|---|
| Set by | `scribe-server`, at PTY spawn time, alongside `SCRIBE_HOOK_SOCK` |
| Read by | `scribe-hook-helper` |
| Type | string — formatted UUID-v4 (the `SessionId` newtype, `crates/scribe-common/src/ids.rs:9-55`) |
| Value | the `SessionId` minted at `session_manager.rs:298` for this PTY |
| Set in | base PTY env, propagates through user shell + AI tool to hook subprocess |
| Unset means | helper exits 0 silently (same as `SCRIBE_HOOK_SOCK` unset) |
| Stability | survives AI tool restart inside the same pane. Does NOT survive pane destruction. |

## Propagation contract

Both variables must reach every hook subprocess. The chain is:

```
scribe-server  --[fork+exec]-->  user shell  --[fork+exec]-->  AI tool (claude/codex)  --[fork+exec]-->  hook subprocess
```

Each `fork+exec` inherits the env by default on POSIX. Shell integration scripts (`dist/shell-integration/*`) MUST NOT unset, rename, or filter these variables. The existing `dist/shell-integration/zsh/scribe.zsh:1-103`, `dist/shell-integration/bash/scribe.bash:1-20`, `dist/shell-integration/fish/vendor_conf.d/scribe.fish:106-125`, `dist/shell-integration/nushell/vendor/autoload/scribe.nu:86-101`, `dist/shell-integration/powershell/scribe.ps1:115-125` are unaffected because they only add to env, never strip.

AI tools likewise pass env through to their hook subprocesses; this is documented behavior per the Claude Code hooks reference (`docs.claude.com/en/docs/claude-code/hooks`) and matches Codex's existing behavior (the OSC hooks today already inherit env to call `printf > /dev/tty`).

## Absence semantics (FR-003, FR-004)

The helper checks both variables in order:

```text
if SCRIBE_HOOK_SOCK unset OR empty:
    exit 0 silently
if SCRIBE_SESSION_ID unset OR empty:
    exit 0 silently
if SCRIBE_SESSION_ID fails to parse as a UUID-v4:
    exit 0 silently  (treat as "not under Scribe")
attempt to emit; on any failure → exit 0 silently
```

**Critical**: silence here means **no stdout, no stderr, no /dev/tty write, no log file**. The exit code is the only signal, and it MUST be 0 in every path (FR-007 / FR-008 / FR-009 / FR-010).

## Cloud / non-Scribe environment behavior

Hooks that the user has installed via `dist/setup-{provider}-hooks.sh` are registered globally in the AI tool's settings file (e.g. `~/.claude/settings.json`). They follow the user to every surface that AI tool runs on:

- **Claude Code on the web** (Anthropic's cloud) — no Scribe present → both env vars unset → helper exits 0 silently.
- **AI tool over SSH on a different host** — no Scribe present on that host → both env vars unset → helper exits 0 silently.
- **AI tool inside a subagent context** — the subagent inherits env from the parent AI tool process. If the parent was under Scribe, the subagent's env contains both variables and the subagent's hook events route to the parent pane's `SessionId`. This is the correct behavior: subagents don't have their own Scribe pane.
- **CI / batch context** — no Scribe → exit 0 silently.

The contract above is what makes FR-025 hold: Scribe-installed hooks run safely everywhere.

## Why two variables, not one

Conceptually distinct concerns:

- `SCRIBE_HOOK_SOCK` answers "is there a Scribe to talk to, and where is it?" — purely a transport address.
- `SCRIBE_SESSION_ID` answers "which Scribe pane do I belong to?" — a routing key.

Bundling them into one variable would tempt clever parsing in shell adapters (split on a delimiter, error-prone) and makes the no-op decision (`SCRIBE_HOOK_SOCK` unset) harder to express cleanly. Two single-purpose variables are simpler.

## Compatibility with existing env vars

These two variables join the existing Scribe-set env, which currently is:

| Existing | Set at | Purpose |
|---|---|---|
| `TERM=alacritty` (or theme-derived) | `session_manager.rs:538` | terminfo identification |
| `COLORTERM=truecolor` | `session_manager.rs:538` | 24-bit color hint |
| `TERM_PROGRAM=Scribe` | `session_manager.rs:538` | terminal program identification |
| `TERM_PROGRAM_VERSION=<version>` | `session_manager.rs:538` | terminal program version |
| `SCRIBE_SHELL_INTEGRATION=1` | `shell_integration.rs:72` | gates shell-integration startup script |

The new variables follow the same prefix and the same site.

## Test surface

- Unit test in `crates/scribe-server/src/session_manager.rs` (or test module) asserts both variables are present in the env passed to `alacritty_terminal::tty::new`.
- Integration test in `crates/scribe-server/tests/hook_channel_roundtrip.rs` asserts a subprocess spawned under a real PTY sees both vars in its `env::vars()`.
- Offline regression in `tests/install/ipc-hook-regressions.sh` asserts the adapter scripts respect "unset env → exit 0 silently".
