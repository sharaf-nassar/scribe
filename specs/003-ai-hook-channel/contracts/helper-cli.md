# Contract: `scribe-hook-helper` CLI

**Scope**: The command-line interface of the shared emitter binary that provider adapter scripts call to send a `HookEvent` to `scribe-server`.

## Invocation form

```text
scribe-hook-helper
  --provider=<claude_code|codex_code|auggie>
  --event=<state_changed|session_stopped|state_cleared|prompt_received|task_label_changed|task_label_cleared|context_changed>
  [event-specific flags …]
```

The helper is invoked once per event. It always exits with status 0. It never writes to stdout or stderr.

## Common flags

| Flag | Required | Notes |
|---|---|---|
| `--provider=<id>` | yes | One of `AiProvider::id()` values: `claude_code`, `codex_code`, `auggie`. Mapped to `AiProvider` via `AiProvider::from_id` (`ai_state.rs:39`). Unknown value → exit 0 silently. |
| `--event=<id>` | yes | Determines which `HookEventKind` variant to build. See per-event flags below. Unknown value → exit 0 silently. |

## Per-event flags

### `--event=state_changed`

```text
--state=<idle_prompt|processing|waiting_for_input|permission_prompt|error>
[--conversation-id=<string>]
```

| Flag | Required | Notes |
|---|---|---|
| `--state` | yes | Must parse to `AiState`. Unknown → exit 0 silently. |
| `--conversation-id` | no | Opaque string from the AI tool. Truncated at 256 bytes on the server. |

### `--event=session_stopped`

```text
--last-message-file=<path>
[--conversation-id=<string>]
```

| Flag | Required | Notes |
|---|---|---|
| `--last-message-file` | yes | Path to a file containing the assistant's last-message text. The helper reads up to `LAST_MESSAGE_CAP_BYTES` (16 384) from it and discards the rest. File missing → exit 0 silently. |
| `--conversation-id` | no | Same as above. |

**Why a file path instead of a string arg**: the assistant's last message can be many KiB. Stdin is reserved for the hook event JSON (the AI tool writes to it; the adapter passes it through). Passing multi-KiB text via `--last-message=…` would hit `ARG_MAX` on long messages. The adapter writes the extracted text to a temp file (`mktemp`) and passes the path. The helper unlinks the file after reading (best-effort; missing-unlink is silent).

### `--event=state_cleared`

No event-specific flags.

### `--event=prompt_received`

```text
--text=<string>
[--conversation-id=<string>]
```

| Flag | Required | Notes |
|---|---|---|
| `--text` | yes | The user's submitted prompt text. Truncated at `PROMPT_TEXT_CAP_BYTES` (256). Empty → exit 0 silently. |

### `--event=task_label_changed`

```text
--label=<string>
```

| Flag | Required | Notes |
|---|---|---|
| `--label` | yes | The sanitized task label. Truncated at `TASK_LABEL_CAP_BYTES` (256). Empty → exit 0 silently. |

### `--event=task_label_cleared`

No event-specific flags.

### `--event=context_changed`

```text
--fill-percent=<0..100>
```

| Flag | Required | Notes |
|---|---|---|
| `--fill-percent` | yes | Integer percentage. Out-of-range or non-integer → exit 0 silently. |

## Discovery and gating

Before doing any work:

1. Read `SCRIBE_HOOK_SOCK` env var → if unset/empty, **exit 0 silently**.
2. Read `SCRIBE_SESSION_ID` env var → if unset/empty or not a valid UUID-v4, **exit 0 silently**.
3. Parse CLI args → if invalid, **exit 0 silently**.
4. Build `HookEvent` → connect to socket → frame and write → close → **exit 0**.

## I/O policy (HARD CONSTRAINTS — FR-007 through FR-011)

- **stdout**: zero bytes. Never written. (Some AI tool hook events forward stdout to the model context.)
- **stderr**: zero bytes. Never written. (Some AI tool hook events surface stderr to the user.)
- **/dev/tty**: never opened. (FR-010.)
- **exit code**: always 0. (FR-007.)
- **panic / abort**: the binary uses `panic = "abort"` in `Cargo.toml` and a custom panic hook (`std::panic::set_hook`) that swallows panic messages without printing. Worst case: SIGABRT-induced exit 134, never anything that reaches the AI tool's stderr.

## Timeout (FR-012)

- **Total budget**: 100 ms wall-clock, covering connect + write + close.
- **Enforcement**: the binary sets a `tokio::time::timeout` around the full I/O sequence. Expiry → drop in-progress operations, exit 0.
- **Rationale**: see [research.md](../research.md) Decision 8.

## Retries

None. One attempt per invocation. Failed events are lost. The next hook event will retry naturally; tab-indicator UI is eventually-consistent under load.

## Concurrency

Multiple helper processes may run simultaneously (e.g. two hooks fire in quick succession). Each is independent: separate process, separate connection, separate frame. The server-side accept loop handles concurrency.

## Adapter script invocation pattern

Each `dist/ai-hook-<provider>.sh` adapter follows this template:

```sh
#!/bin/sh
# Read stdin JSON, extract fields with python3, exec helper.
# Exit 0 unconditionally; never write to stdout/stderr.

set +e
PAYLOAD="$(python3 -c '<extract field>' 2>/dev/null)" || PAYLOAD=""
# … one or more extracts …

exec /usr/share/scribe/scribe-hook-helper \
    --provider=<provider_id> \
    --event=<event_id> \
    --<field>="$PAYLOAD" \
    2>/dev/null

# Unreachable; exec replaces this process. But just in case:
exit 0
```

The `2>/dev/null` on the `exec` is a belt-and-braces guard. The Rust helper never writes to stderr in normal operation; the redirect handles the kernel's own error if `exec` itself fails (e.g. binary missing) and is one of the few cases where the helper isn't yet running.

## Versioning

The helper binary version follows the workspace version (set at build time via `env!("CARGO_PKG_VERSION")`). The helper does NOT advertise its version on the wire because the wire is msgpack-named and naturally schema-tolerant; clients and server upgrade together via Scribe's normal package flow.

## Failure modes (exhaustive)

| Failure | Helper response |
|---|---|
| `SCRIBE_HOOK_SOCK` unset | exit 0 silently |
| `SCRIBE_SESSION_ID` unset / unparseable | exit 0 silently |
| `--provider` unknown | exit 0 silently |
| `--event` unknown | exit 0 silently |
| Missing required flag | exit 0 silently |
| `--state` / `--fill-percent` malformed | exit 0 silently |
| `--last-message-file` path missing | exit 0 silently |
| Socket connect failed | exit 0 silently |
| Socket write failed mid-frame | exit 0 silently |
| Timeout (100 ms exceeded) | exit 0 silently |
| Server crashed mid-write | exit 0 silently |
| Helper itself panics | abort via custom hook, no stderr |

Every failure mode produces the same observable behavior: exit 0, zero I/O on the standard streams. This is what makes the AI tool's view of "is Scribe installed?" indistinguishable from "is the channel reachable right now?", which in turn makes FR-025 hold uniformly.

## Test surface

- Unit tests in `crates/scribe-hook-helper/src/main.rs` cover: arg parsing, env-var gating, message construction. No I/O in unit tests.
- Integration test in `crates/scribe-server/tests/hook_channel_roundtrip.rs` exec's the real helper binary (cargo bin lookup) against an in-process server.
- Offline regression in `tests/install/ipc-hook-regressions.sh` replaces the real helper with a `/bin/sh` mock that echoes its args; asserts each adapter calls the helper with the expected flags.
