# Phase 1 Data Model — OSC 52 Clipboard Gating

**Branch**: `010-osc52-clipboard-gating`
**Date**: 2026-05-22
**Scope**: Multi-crate (`scribe-common` config + protocol;
`scribe-server` per-session state; `scribe-client` dialog state).

Following research.md, five entities cross the crate boundaries.
Two are config / protocol types living in `scribe-common`, two are
per-session / per-window runtime state, and one is a thin facade
over the existing `arboard::Clipboard` handle on the client.

## E1 — ClipboardPolicyConfig (new, scribe-common)

**Owner**: `crates/scribe-common/src/config.rs`
**Lifetime**: One per installation. Read at server attach time and
on each `ConfigReloaded` event. Hung off `TerminalConfig` as
`clipboard: ClipboardPolicyConfig`. Serialized to TOML at the
config file root; webview-edited via `scribe-settings`.

| Field | Type | Default | Notes |
|---|---|---|---|
| `read_mode` | `ClipboardMode` | `Prompt` | One of `Deny`, `Allow`, `Prompt`. Maps to FR-001. Applies to both clipboard and primary selection per FR-004. |
| `write_mode` | `ClipboardMode` | `Allow` | One of `Deny`, `Allow`, `Prompt`. Maps to FR-002. Applies to both clipboard and primary selection. |
| `max_write_bytes` | `u64` | `16_777_216` | 16 MiB default per Q2; user-settable up to `512 * 1024 * 1024` (512 MiB) per FR-009. Values above the cap are clamped at deserialize time with a warning. |
| `focus_gate_writes` | `bool` | `false` | FR-019 opt-in. Off by default; on means "client must be focused for writes to land in the host clipboard". |
| `burst_window_ms` | `u64` | `500` | FR-017 burst-decision-reuse window. Plan-phase tunable; not exposed in the settings UI for v1. |

```rust
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ClipboardMode {
    Deny,
    Allow,
    Prompt,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default)]
pub struct ClipboardPolicyConfig {
    pub read_mode: ClipboardMode,
    pub write_mode: ClipboardMode,
    pub max_write_bytes: u64,
    pub focus_gate_writes: bool,
    pub burst_window_ms: u64,
}
```

**Invariants**:

- `ClipboardMode` is exhaustively matched in the policy
  evaluator (decision 5); adding a variant requires touching the
  evaluator.
- `max_write_bytes` is clamped to `[0, 512 MiB]` on deserialize.
  Zero means "writes effectively disabled at the size layer";
  the explicit `Deny` mode is the canonical way to disable writes
  via the policy axis.
- `burst_window_ms` is bounded `[0, 10_000]` on deserialize; zero
  disables burst-decision-reuse (every fresh request prompts).

## E2 — ClipboardOp / ClipboardDecision (new, scribe-common)

**Owner**: `crates/scribe-common/src/protocol.rs`
**Lifetime**: Per IPC message. Carried inside the new
`ClipboardPromptRequest` / `ClipboardPromptResponse` variants.

```rust
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ClipboardOp {
    Read,
    Write,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ClipboardSelection {
    Clipboard,
    Primary,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ClipboardDecision {
    AllowOnce,
    DenyOnce,
    AlwaysAllow,
    AlwaysDeny,
}
```

**Invariants**:

- `ClipboardSelection::Primary` on non-X11 platforms resolves to
  the system clipboard at the `arboard` layer (decision 3); the
  policy still evaluates it independently for symmetry.
- `ClipboardDecision::AlwaysAllow` / `AlwaysDeny` trigger a
  persisted policy update on the originating axis (read or write
  per the request's `op`). The client sends an out-of-band
  policy-write through the existing settings webview channel; the
  server picks up the new policy on the next `ConfigReloaded`.

## E3 — ClipboardBurstState (new, scribe-server)

**Owner**: `crates/scribe-server/src/ipc_server.rs` (held inside
`PtyReaderState`).
**Lifetime**: One per session. Lives for the duration of the
PTY reader task; dropped on session exit; re-initialized
(empty) on cold-restart handoff. Per-session = per-pane (each
session has its own state per the Scribe data model).

| Field | Type | Notes |
|---|---|---|
| `outstanding_prompt` | `Option<PromptId>` | `Some` when a `ClipboardPromptRequest` has been emitted to the client and no response received. FR-016 defer hook. |
| `pending_for_prompt` | `Vec<DeferredRequest>` | Requests that arrived while `outstanding_prompt.is_some()` and matched the open prompt's op. Each defers its reply until the prompt resolves. Bounded length (research decision 5; max 64 — anything beyond is treated as cap-exceeded and rejected). |
| `last_decision` | `Option<(ClipboardOp, ClipboardDecision, Instant)>` | Most recent resolved decision. FR-017 reuse source. |
| `policy` | `ClipboardPolicyConfig` | Snapshot at attach time; refreshed on `ConfigReloaded`. |

```rust
struct DeferredRequest {
    request_id: PromptId,
    op: ClipboardOp,
    selection: ClipboardSelection,
    payload_for_write: Option<String>,
    read_formatter: Option<ClipboardFormatter>,
}
```

**Invariants**:

- `outstanding_prompt` and `pending_for_prompt` are reset
  atomically when a `ClipboardPromptResponse` arrives.
- `last_decision` is updated on every prompt resolution
  (including `DenyOnce` and `AlwaysDeny`).
- The reuse check (FR-017) compares
  `now.duration_since(last_decision.timestamp)` against
  `policy.burst_window_ms`. Times outside the window invalidate
  the cached decision.

**State transitions** (per FR-016/017/018):

```text
Idle  ──[OSC 52 event, mode=prompt, no reuse]──>  Awaiting
                                                     │
                                                     ▼
Awaiting  ──[ClipboardPromptResponse]──>  Burst
                                            │
                                            ▼
Burst  ──[burst_window_ms idle]──>  Idle
Burst  ──[OSC 52 event matching op]──>  Burst (reuse decision)
Burst  ──[OSC 52 event different op]──>  Awaiting (new prompt)
Awaiting  ──[OSC 52 event matching op]──>  Awaiting (defer to pending)
```

## E4 — ClipboardDialog (new, scribe-client)

**Owner**: `crates/scribe-client/src/clipboard_dialog.rs`
**Lifetime**: At most one per client window. Replaces / dismisses
the previous instance when a new `ClipboardPromptRequest` arrives
for the same window.

| Field | Type | Notes |
|---|---|---|
| `request_id` | `PromptId` | Echoed back in `ClipboardPromptResponse` so the server can match. |
| `session_id` | `SessionId` | Which pane the request belongs to (anchors the dialog in the pane per UX-001 and the spec's multi-pane edge case). |
| `op` | `ClipboardOp` | Drives the title copy ("Allow paste from clipboard?" vs. "Allow copy to clipboard?"). |
| `selection` | `ClipboardSelection` | Mentioned in body text. |
| `preview` | `Option<String>` | Write-only; truncated head-and-tail per FR-006. For reads, `None`. |
| `show_always_buttons` | `bool` | Default `true`. Hides the "Always" pair if the user-config disables them (future-proof; v1 always shows them). |
| `focused_button` | `ButtonIndex` | Defaults to `DenyOnce`. Tab cycles; Esc activates `DenyOnce`. |

```rust
pub enum ClipboardDialogAction {
    AllowOnce,
    DenyOnce,
    AlwaysAllow,
    AlwaysDeny,
}
```

**Invariants**:

- The dialog is dismissed immediately on action; the
  `ClipboardPromptResponse` is sent on dismiss.
- A second `ClipboardPromptRequest` arriving while the dialog is
  open replaces the dialog only if it belongs to a different
  session. Same-session requests do not happen — the server's
  `ClipboardBurstState::pending_for_prompt` defers them (FR-016).
- `preview` rendering uses the same head-tail truncation helper as
  the spec 009 disallowed-scheme dialog body text.

## E5 — HostClipboardBridge (thin facade, scribe-client)

**Owner**: `crates/scribe-client/src/main.rs` (no new module — a
small set of functions on `App` keyed by the existing
`clipboard: Option<arboard::Clipboard>` field at line 346).
**Lifetime**: As long as the `arboard::Clipboard` itself.

**Operations**:

- `bridge_write(selection: ClipboardSelection, payload: &str) ->
  Result<(), BridgeError>`:
  1. If `policy.focus_gate_writes` and `!self.window_focused`:
     silent no-op, return Ok (per research decision 6).
  2. Call `arboard::Clipboard::set_text` (or the X11 primary
     variant for `ClipboardSelection::Primary`).
  3. Any arboard error maps to `BridgeError::Unavailable` and is
     observable to the calling code only via the return value
     (no PTY-side surface — UX-002).
- `bridge_read(selection: ClipboardSelection) ->
  Result<String, BridgeError>`:
  1. Call `arboard::Clipboard::get_text` (or X11 primary
     variant).
  2. Empty string is a valid value (matches the host clipboard
     state).
  3. Errors return `BridgeError::Unavailable`; the server treats
     this identically to a deny and replies an empty OSC 52 payload.

**Invariants**:

- The bridge holds no state of its own beyond the existing
  `arboard::Clipboard` and `App::window_focused`. No second
  clipboard buffer (per research decision 1).
- The bridge does NOT consult `policy.read_mode` /
  `policy.write_mode` itself — that decision is owned server-side
  per research decision 6. The bridge only enforces the
  client-local `focus_gate_writes` check.

---

## Cross-entity flow (read example, prompt mode, no burst reuse)

```text
PTY program → vte parses OSC 52 read → alacritty Term fires
  Event::ClipboardLoad → ScribeEventListener emits
  SessionEvent::ClipboardLoad → ipc_server reader sees mode=prompt:
    - check ClipboardBurstState.outstanding_prompt → None
    - check ClipboardBurstState.last_decision → expired
    - assign request_id, set outstanding_prompt = Some(id)
    - send ServerMessage::ClipboardPromptRequest to client
  Client renders ClipboardDialog → user picks AllowOnce
    - send ClientMessage::ClipboardPromptResponse { request_id,
      decision: AllowOnce } to server
  Server matches request_id → updates ClipboardBurstState
    - last_decision = (Read, AllowOnce, now())
    - drains pending_for_prompt (none in this example)
    - sends ServerMessage::ClipboardBridgeReadRequest to client
  Client calls arboard.get_text → sends
    ClientMessage::ClipboardBridgeReadReply { payload }
  Server formats OSC 52 reply, calls write_term_response →
    PTY program sees clipboard contents.
```

The write path is symmetric: the server forwards
`ClipboardBridgeWrite` with the size-capped payload; the client
honors `focus_gate_writes` and calls `arboard.set_text`; no reply
needed (OSC 52 has no write-ack semantic).
