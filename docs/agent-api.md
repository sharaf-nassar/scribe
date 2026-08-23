# Scribe Agent API

Scribe exposes a supported, local-only control surface for AI agents. An agent
with shell access — Claude Code, Codex, Pi, or anything else that can run a
command — talks to the Scribe server through the `scribe agent` command
family: one-shot requests over the server's existing Unix domain socket,
versioned JSON on stdout, diagnostics on stderr, typed error codes, and a
per-capability policy that denies everything until you opt in.

The CLI's JSON output is the stable contract. The raw IPC wire format
underneath it is not stable for third parties, and there is no network
listener, HTTP endpoint, or MCP server — the socket is
`/run/user/{uid}/scribe/server.sock` on Linux and the equivalent per-user
runtime path on macOS, reachable only by processes running as your user.

## Binaries and availability

| Install | CLI binary | Talks to |
|---|---|---|
| Debian package `scribe` (stable) | `/usr/bin/scribe` | the stable server |
| Debian package `scribe-dev` | `/usr/bin/scribe-dev-cli` | the dev server |
| macOS `Scribe.app` | `Scribe.app/Contents/MacOS/scribe` | the bundle's server |

The dev flavor is fully isolated: `scribe-dev-cli` resolves the `scribe-dev`
runtime directory, configuration, and socket, so a dev CLI never drives a
stable server (or vice versa).

On macOS, Scribe prepends the app bundle's executable directory to `PATH`
inside every Scribe pane, so `scribe` resolves with no extra setup. On Linux
the binary is in `/usr/bin`. The commands work from any same-user shell — a
Scribe pane is not required — but inside a pane the `SCRIBE_SESSION_ID`
environment variable supplies caller orientation (see
[Invocation](#invocation)).

## Invocation

```bash
scribe agent [--agent NAME] [--model MODEL] <command> [args]
```

- `--agent NAME` — caller-supplied agent name, shown to the user in
  confirmation prompts and recorded in the audit log. Defaults to
  `scribe-cli`. Must be non-empty.
- `--model MODEL` — optional caller-supplied model name. When set, the
  displayed label becomes `NAME [MODEL]`.
- The composed label is capped at 64 characters. The label is disclosure, not
  authentication — see [Trust model](#trust-model-and-limitations).
- `--json` — accepted for symmetry; data commands emit JSON regardless.

Environment:

- `SCRIBE_SESSION_ID` — set automatically inside every Scribe pane. When set,
  it must be the caller's full session UUID; the CLI forwards it so the server
  can mark the caller's own entry (`is_caller`) and resolve `siblings`. An
  unset or empty variable is fine for every command except `siblings`. An
  invalid value is a usage error.

Every data command is one request and one reply: the CLI connects, sends the
request, prints exactly one JSON envelope as a single line on stdout, and
exits. Diagnostics never go to stdout. If the server does not answer within
3 seconds — for example, a server built before the agent API — the CLI reports
`unsupported` and exits 3.

Use `scribe agent <command> --help` for per-command argument details.

## Response envelope

Success:

```json
{"v": 1, "ok": true, "data": { "type": "..." }}
```

Failure:

```json
{"v": 1, "ok": false, "error": {"code": "...", "message": "..."}}
```

`v` is the envelope version, currently `1`. It changes only for incompatible
envelope changes; additive fields inside `data` do not bump it. `message` is
human-readable and not stable; `code` is stable and machine-matchable.

## Exit codes

| Exit | Meaning |
|---|---|
| `0` | Success — the envelope has `ok: true`. |
| `1` | Typed API error — the envelope has `ok: false` (denied, not found, too large, …). |
| `2` | Usage error — malformed command line, or invalid `--agent`, `--model`, or `SCRIBE_SESSION_ID`. |
| `3` | Server unreachable, or the connected server does not support the agent API. |

Malformed command lines print clap diagnostics without a JSON envelope and
exit 2. Invalid `--agent`/`--model`/`SCRIBE_SESSION_ID` values print a JSON
envelope with code `usage` and exit 2.

## Error codes

Returned by the server (stable, in the envelope's `error.code`):

| Code | Meaning |
|---|---|
| `denied` | The capability is denied by policy, or the user declined the confirmation prompt. All capabilities are denied by default. |
| `prompt_timeout` | A `prompt`-mode confirmation was raised but not answered in time. |
| `not_found` | The named session or window does not exist. Also returned by `siblings` without a valid origin session. |
| `ambiguous_target` | An action omitted `--window` while more than one window is connected. |
| `unsupported` | This build does not support the request. Exits 3. |
| `too_large` | A request exceeds its bound — for example, `write` text over `max_input_bytes`. Checked before any prompt is raised. |
| `busy` | The concurrent-request limit is reached; retry later. |
| `version_mismatch` | The CLI and server disagree on the contract version. |
| `action_failed` | An accepted action failed to complete (for example, the client disconnected before executing it). |
| `internal` | Unexpected server-side failure. |

Produced by the CLI itself:

| Code | Meaning | Exit |
|---|---|---|
| `usage` | Invalid invocation values. | 2 |
| `unreachable` | The server socket could not be connected. | 3 |
| `unsupported` | The connected server predates the agent API or returned no valid reply within 3 seconds. | 3 |
| `internal` | The CLI could not serialize or correlate the response. | 1 |

Policy is evaluated before target lookup, so a call denied by policy returns
`denied` without disclosing whether the target exists.

Window ids are snapshot facts, not permanent workspace identities. Moving a
workspace to a new window mints a fresh `window_id`; `world` and `siblings`
show the ownership change atomically, so agents should refresh a snapshot
rather than cache a session's or workspace's window id across operations.

## Commands

Session, window, and workspace ids are full UUIDs, exactly as returned by
`world` and `siblings`.

### `scribe agent world`

Capability: **read-metadata**. Returns every window, workspace, and session
the server owns, as one internally consistent snapshot (`snapshot_id` ties
the entries together). Sessions carry AI provider, state, task label, and
context fill where the server has them; absent facts are omitted, never
faked. When a valid origin session is supplied, exactly one session carries
`is_caller: true`.

```bash
scribe agent --agent my-agent --model my-model world
```

```json
{
  "v": 1,
  "ok": true,
  "data": {
    "type": "world",
    "snapshot": {
      "windows": [
        {
          "window_id": "8f0f3c1e-5f0a-4f2e-9b3d-2a6c4e8d1b57",
          "workspace_names": ["scribe"],
          "session_count": 2,
          "connected": true,
          "sharing_mode": "single_controller",
          "participant_count": 0
        }
      ],
      "workspaces": [
        {
          "workspace_id": "3d9b2a7c-1e4f-4a6b-8c5d-7e2f9a0b4c6e",
          "name": "scribe",
          "window_id": "8f0f3c1e-5f0a-4f2e-9b3d-2a6c4e8d1b57",
          "session_ids": [
            "6b0f7f0a-4b8e-4e0e-9c47-4f2f6d3a9b1c",
            "a4c2e8d0-9b1f-4c3a-8e5d-6f7a2b4c9e0d"
          ]
        }
      ],
      "sessions": [
        {
          "session_id": "a4c2e8d0-9b1f-4c3a-8e5d-6f7a2b4c9e0d",
          "window_id": "8f0f3c1e-5f0a-4f2e-9b3d-2a6c4e8d1b57",
          "workspace_id": "3d9b2a7c-1e4f-4a6b-8c5d-7e2f9a0b4c6e",
          "title": "cargo test",
          "cwd": "/home/user/work/scribe",
          "provider": "claude_code",
          "ai_state": "processing",
          "task_label": "Fixing tests",
          "context_fill_percent": 42,
          "is_caller": false
        }
      ],
      "snapshot_id": 12,
      "captured_at": 1766188800
    }
  }
}
```

`provider` is one of `claude_code`, `codex_code`, `pi`, `system`. `ai_state`
is one of `idle_prompt`, `processing`, `waiting_for_input`,
`permission_prompt`, `error`. `sharing_mode` is one of `single_controller`,
`shared_single_typist`, `free_for_all`. `captured_at` is the server's capture
timestamp. Deliberately absent: prompt text, conversation ids, launch ids,
and other participants' identities. The response never carries terminal
content — that is `read`'s job, under its own capability.

### `scribe agent siblings`

Capability: **read-metadata**. The same snapshot shape as `world`, filtered
to the caller's own window — "what is next to me" in one call, no id
plumbing. Requires a valid `SCRIBE_SESSION_ID`; without one it returns
`not_found`. The payload `type` is `siblings`.

```bash
scribe agent --agent my-agent siblings
```

### `scribe agent read <session-id> [--scrollback N]`

Capability: **read-content**. Returns the named session's current screen as
text, plus up to `N` scrollback lines (clamped to `max_scrollback_lines`).
The response identifies the pane by `title` and `cwd` so two panes cannot be
confused. Text is normalized for machine consumption: soft-wrapped rows are
joined, hard line breaks preserved, trailing blanks trimmed, styles and
colors dropped, terminal images appear as `[image omitted]`, and OSC 8
hyperlinks keep their visible text but drop the URI. A response that would
exceed `max_response_bytes` is cut and flagged `truncated`.

```bash
scribe agent --agent my-agent read a4c2e8d0-9b1f-4c3a-8e5d-6f7a2b4c9e0d --scrollback 200
```

```json
{
  "v": 1,
  "ok": true,
  "data": {
    "type": "read_screen",
    "screen": {
      "session_id": "a4c2e8d0-9b1f-4c3a-8e5d-6f7a2b4c9e0d",
      "title": "cargo test",
      "cwd": "/home/user/work/scribe",
      "text": "running 128 tests\n...\ntest result: ok",
      "lines": 3,
      "truncated": false,
      "captured_at": 1766188800,
      "snapshot_id": 42
    }
  }
}
```

### `scribe agent action <action> [--window <window-id>]`

Capability: **dispatch-action**, or **dispatch-destructive-action** for the
actions marked below — destructive actions cannot ride a benign grant. With
exactly one connected window `--window` may be omitted; with several, an
ambiguous action returns `ambiguous_target` rather than guessing. The reply
reports real completion, not queuing: session-creating actions return the
new `created_session_id`, and a failure surfaces as `action_failed`.

| CLI action | JSON `action.type` | Capability |
|---|---|---|
| `open-settings` | `open_settings` | dispatch-action |
| `open-find` | `open_find` | dispatch-action |
| `new-tab` | `new_tab` | dispatch-action |
| `new-claude-tab` | `new_claude_tab` | dispatch-action |
| `resume-claude-tab` | `new_claude_resume_tab` | dispatch-action |
| `new-codex-tab` | `new_codex_tab` | dispatch-action |
| `resume-codex-tab` | `new_codex_resume_tab` | dispatch-action |
| `new-ai-tab` (alias of `new-claude-tab`) | `new_claude_tab` | dispatch-action |
| `resume-ai-tab` (alias of `resume-claude-tab`) | `new_claude_resume_tab` | dispatch-action |
| `split-vertical` | `split_vertical` | dispatch-action |
| `split-horizontal` | `split_horizontal` | dispatch-action |
| `new-window` | `new_window` | dispatch-action |
| `switch-profile <name>` | `switch_profile` | dispatch-action |
| `focus-session <session-id>` | `focus_session` | dispatch-action |
| `close-pane` | `close_pane` | dispatch-destructive-action |
| `close-tab` | `close_tab` | dispatch-destructive-action |
| `open-update-dialog` | `open_update_dialog` | dispatch-destructive-action |

```bash
scribe agent --agent my-agent action new-tab
```

```json
{
  "v": 1,
  "ok": true,
  "data": {
    "type": "dispatch_action",
    "result": {
      "action": { "type": "new_tab" },
      "outcome": "completed",
      "created_session_id": "5e2d8c4a-7b0f-4e1a-9d3c-8a6b2f4e0c1d"
    }
  }
}
```

### `scribe agent write <session-id> --text <text> [--submit]`

Capability: **write-input**. Writes UTF-8 text into the named session's
input, as if typed. With `--submit`, the input is submitted (Enter) after
the text. The payload is bounded by `max_input_bytes`; an over-cap payload
returns `too_large` before any confirmation prompt is raised. The success
reply is acknowledged only after the write lands; a PTY failure returns
`action_failed` instead of being silently dropped.

```bash
scribe agent --agent my-agent write a4c2e8d0-9b1f-4c3a-8e5d-6f7a2b4c9e0d --text "just test" --submit
```

```json
{"v": 1, "ok": true, "data": {"type": "write_input"}}
```

### `scribe agent capabilities`

Requires no capability grant. Reports the surface version and every supported
capability with its current live policy mode, so an agent can probe before
spending calls. An operation the build does not know fails with `unsupported`
rather than hanging.

```json
{
  "v": 1,
  "ok": true,
  "data": {
    "type": "capabilities",
    "version": 1,
    "capabilities": [
      {"capability": "read_metadata", "mode": "deny"},
      {"capability": "read_content", "mode": "allow"},
      {"capability": "dispatch_action", "mode": "prompt"},
      {"capability": "dispatch_destructive_action", "mode": "deny"},
      {"capability": "write_input", "mode": "deny"}
    ]
  }
}
```

### `scribe agent skill`

Not a data command. Prints provider guidance as markdown on stdout, rendered
from this binary's actual command tree and the live policy — never
hand-authored, so it cannot drift from the contract it documents. A
capability you have not granted renders as unavailable together with the
settings key that would enable it, instead of inviting the agent to burn a
turn on a refusal. The guidance tells agents to no-op when
`SCRIBE_SESSION_ID` is unset, so an agent outside Scribe is never told to
call a command that cannot help it.

It reads the local configuration and does not contact the server; exit 0 on
success, 1 if the configuration cannot be loaded. Save its output wherever
your agent discovers instructions (for example, a provider skill file), and
regenerate it after a policy change or upgrade.

## Policy and configuration

Five independent capabilities gate the surface. Each is `allow`, `deny`, or
`prompt`; **everything defaults to `deny`**, and all-deny is the off state —
there is no separate master switch, and a disabled surface runs no extra
listener, task, or timer.

| Capability | Config key | Gates |
|---|---|---|
| Read metadata | `agent_api.read_metadata` | `world`, `siblings` |
| Read content | `agent_api.read_content` | `read` |
| Dispatch actions | `agent_api.dispatch_action` | non-destructive `action` commands |
| Dispatch destructive actions | `agent_api.dispatch_destructive_action` | `close-pane`, `close-tab`, `open-update-dialog` |
| Write input | `agent_api.write_input` | `write` |

Configure them in the settings window (`Ctrl+,` → **Agent API**) or in
`~/.config/scribe/config.toml`:

```toml
[agent_api]
read_metadata = "allow"
read_content = "prompt"
dispatch_action = "prompt"
dispatch_destructive_action = "deny"
write_input = "prompt"
```

Changes apply live — no server restart — and a policy refresh cancels
prompts that are still pending.

### Prompt mode

A `prompt` capability raises a Scribe-owned confirmation dialog naming the
caller-supplied agent label and the requested capability. The decision
defaults to Deny, Escape denies, and choosing "Always" persists that
capability's mode as `allow`. The call is denied when no Scribe window that
understands the prompt is attached (headless), or when `prompt_timeout_ms`
elapses unanswered. An approval is reused for repeated calls with the same
agent label, capability, and target within `burst_window_ms`, so one
confirmation covers a tight burst instead of interrogating you per call.

### Limits

All numeric keys live in the same `[agent_api]` table. Values above a
ceiling are clamped on load, not rejected; unknown keys are tolerated.

| Key | Default | Ceiling | Meaning |
|---|---|---|---|
| `max_response_bytes` | 262144 (256 KiB) | 262144 | Hard cap on one response; an oversized `read` is cut and flagged `truncated`. |
| `max_scrollback_lines` | 1000 | 10000 | Most scrollback lines one `read` may request. |
| `max_input_bytes` | 4096 | 65536 | Most UTF-8 bytes one `write` may carry; checked before any prompt. |
| `prompt_timeout_ms` | 60000 | 300000 | How long a `prompt` confirmation waits before denying. |
| `burst_window_ms` | 500 | 5000 | How long an approval is reused for the same agent, capability, and target. |
| `activity_dwell_ms` | 1500 | 10000 | How long the tab's agent indicator lingers after the last call. |

## Visibility and audit

Agent access is never silent:

- While an agent is using the API against a session, that session's tab
  shows an agent indicator at the start of the tab label, coexisting with
  the AI-state indicator. It clears `activity_dwell_ms` after the last call
  finishes; overlapping calls cannot clear each other's indicator early.
- Every call — allowed or refused — emits a metadata-only audit record as a
  structured log event (target `scribe::agent_api`, event `agent_call`)
  carrying the agent label, capability, target kind and id, decision, and
  response byte count. The audit never contains terminal content, and no
  separate audit file is written.

## Trust model and limitations

**Same-user trust, cooperative policy.** Socket admission authenticates only
the connecting process's UID — any process running as your user can connect.
The capability policy constrains cooperative callers of this supported
surface; it is **not** a sandbox against arbitrary same-UID processes, which
retain the ordinary raw IPC paths that predate the agent API. Treat the
policy as consent management and observability for well-behaved agents, not
containment of hostile code already running as you.

**Caller-supplied identity.** The `--agent`/`--model` label is self-asserted.
Prompts and the audit log present it as caller-supplied text; a hostile
caller can claim any name, which is the same class of trust as same-UID
access. Likewise `SCRIBE_SESSION_ID` is orientation, not authorization: it
only marks `is_caller` and resolves `siblings`, and no capability decision
depends on it.

## Data locality and egress

Scribe itself opens no network connection and initiates no outbound transfer
as part of this API. Requests ride the local Unix socket only; nothing binds
a TCP port, and nothing is transmitted anywhere by Scribe.

Granting a capability authorizes disclosure **to the local agent that calls
it** — and what that agent does next is outside Scribe's control. A
cloud-backed agent (Claude Code, Codex, and most others) will typically place
what it reads into model context and transmit it to its model provider.
Granting `read_content` — or any capability whose responses carry your
terminal data — to such an agent is therefore your explicit opt-in to that
egress. If that is not acceptable, grant capabilities only to agents backed
by on-device models, or leave the policy denied.
