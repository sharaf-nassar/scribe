# Contract: Wire Protocol

**Scope**: The bytes-on-the-wire contract between `scribe-hook-helper` (client) and `scribe-server` (server) for the AI hook channel. Frames the new `ClientMessage::HookEvent` variant on the existing server socket.

## Transport

- **Socket**: The existing `scribe-server` Unix domain socket (`server.sock`). No new socket.
- **Path discovery**: Helper reads it from `SCRIBE_HOOK_SOCK` env var (see [env-vars.md](./env-vars.md)).
- **Framing**: Length-prefixed msgpack via `scribe_common::framing` (`crates/scribe-common/src/framing.rs:1-73`). Unchanged from existing IPC: 4-byte big-endian `u32` length + msgpack-named payload, 64 MiB cap.
- **Connection model**: Transient. Helper opens a fresh connection per event, writes one `ClientMessage::HookEvent`, then closes. No `Hello` handshake. No `Welcome` reply. No window registration. Identical to the existing `ClientMessage::CheckForUpdates` / `ClientMessage::ListReleases` transient pattern (`ipc_server.rs:519-533`).

## Request shape

The new top-level variant on `ClientMessage`:

```rust
pub enum ClientMessage {
    // ... existing variants ...
    HookEvent(HookEvent),
}

pub struct HookEvent {
    pub session_id: SessionId,
    pub provider: AiProvider,
    pub kind: HookEventKind,
}

#[serde(rename_all = "snake_case", tag = "type")]
pub enum HookEventKind {
    StateChanged       { state: AiState, conversation_id: Option<String> },
    SessionStopped     { last_message: String, conversation_id: Option<String> },
    StateCleared,
    PromptReceived     { text: String, conversation_id: Option<String> },
    TaskLabelChanged   { label: String },
    TaskLabelCleared,
    ContextChanged     { fill_percent: u8 },
}
```

See [data-model.md](../data-model.md) for field semantics and caps.

## Server response policy

- **Default**: server reads, dispatches, writes no reply. Helper closes the connection after its single write.
- **Validation failures**: server-side validation errors (unknown `session_id`, unknown `provider`, payload over cap) are handled silently — truncate-or-drop on the server side, never propagated back as an error message. The helper has no reply to consume and has already exited.
- **Server unreachable** (connect failed, server crashed, socket file removed): helper's contract is exit 0 with no I/O. The event is lost; the next event lands fine.

## Server-side dispatch

The server's `run_client_message_loop` (`ipc_server.rs:611`) gets a new branch alongside the existing transient handlers:

```rust
ClientMessage::HookEvent(event) => {
    hook_ingress::handle(&mut ctx, event).await;
    break;  // transient: close after one event
}
```

The `hook_ingress::handle` function:

1. Looks up the live session for `event.session_id` in `LiveSessionRegistry`. If absent → drop silently.
2. Verifies the live session's `AiProvider` hint (if set) matches `event.provider`. Mismatch is **not** rejected — multiple providers can run sequentially in one PTY; the server accepts and routes by provider.
3. For `SessionStopped`, runs `stop_classifier::classify(&last_message)` to map to `AiState::IdlePrompt` or `AiState::WaitingForInput`. The resulting `MetadataEvent::AiStateChanged` enters the standard pipeline.
4. For all other `HookEventKind` variants, translates directly to the matching `MetadataEvent` per the table in [data-model.md](../data-model.md), then calls the existing `send_metadata_event` (`ipc_server.rs:2615-2657`).
5. Returns `()`. No reply on the wire.

## Backward compatibility

None. Per Spec Clarifications and FR-020 through FR-022, the new channel **replaces** the OSC-over-`/dev/tty` channel wholesale. There is no concurrent old path. Old Scribe builds connected by new helpers (or vice versa) error at deserialization, which is acceptable because Scribe ships its helper binary in the same package as the server and the user upgrades both together.

## Forward compatibility

- **New `HookEventKind` variants**: serde-msgpack with `#[serde(tag = "type")]` rejects unknown variants by default at deserialization. The server treats deserialization failures as a single dropped event, no panic.
- **New optional fields**: tolerated via `#[serde(default)]` on the field, following the pattern of `AiProcessState::provider` (`ai_state.rs:124`).
- **New `AiProvider` values**: the server drops events with unknown providers (FR-014). A future Scribe build adding another provider won't break older helpers; older Scribe builds connected to a helper emitting the new provider just drop those events.

## Error budget and rate limits

- **Max event size**: 64 MiB (existing framing cap). Practical cap is `LAST_MESSAGE_CAP_BYTES = 16 384` + payload overhead, well under.
- **Rate**: no explicit rate limit. The hook channel is implicitly rate-limited by hook subprocess fork latency (~10 ms) and the AI tool's hook firing cadence (typically <20 events/minute). The server's existing accept loop handles concurrent connections; under burst it will queue at the OS layer until the loop services them.
- **Connection budget**: one connection per event. No keepalive. The server's `LISTENER_BACKLOG` (current value in `ipc_server.rs`) is sufficient — no change required.

## Observability

For diagnostics, `hook_ingress::handle` emits structured `tracing::debug!` log entries on:

- Successful event dispatch (target: `scribe_server::hook_ingress`, fields: `session_id`, `provider`, `kind_variant`).
- Validation drop (unknown session, payload truncation).
- Classifier outcome (for `SessionStopped` events; logs the classification result).

No `tracing::error!` or higher — all hook failures are expected and silent. No metrics in v1.

## Test surface

Validated by the integration test in `crates/scribe-server/tests/hook_channel_roundtrip.rs`:

- Spawn an in-process server (using the `replay_roundtrip.rs` precedent).
- Connect a client via `crates/scribe-test/src/ipc.rs`.
- Send each `HookEventKind` variant.
- Assert the corresponding `ServerMessage` arrives on a subscribed pane.
- Assert no reply is sent on the helper's own connection.
