# Phase 1 Contracts — Internal OSC 52 Pipeline

**Branch**: `010-osc52-clipboard-gating`
**Date**: 2026-05-22

Seven internal contracts span pty → server → client. None of these
are user-facing APIs; they are stable internal seams that the
implementation must preserve so the user stories' acceptance
scenarios remain testable independently.

## C1 — `SessionEvent::ClipboardStore` / `ClipboardLoad` preserved verbatim

**Producer**: `crates/scribe-pty/src/event_listener.rs#ScribeEventListener::send_event`
(`Event::ClipboardStore` arm at line 73; `Event::ClipboardLoad` arm at
line 77).
**Consumer**: `crates/scribe-server/src/ipc_server.rs` reader task
(the `SessionEvent::ClipboardStore` and `ClipboardLoad` arms,
currently at lines 2617-2627).

**Contract**: The producer emits the SAME `SessionEvent` variants
as today, with the same `(ClipboardType, String)` and
`(ClipboardType, ClipboardFormatter)` payload shapes. The change
in this feature is **entirely** on the consumer side. No new
event variants, no removed fields, no upstream re-routing.

**Why this matters**: alacritty_terminal's VTE Perform impl
already maps OSC 52 to these events; touching `scribe-pty`'s
listener would re-wire the parse layer for no gain. Preserving
C1 keeps the change confined to `ipc_server.rs` plus the
already-additive protocol surface in C2/C3.

## C2 — New ServerMessage variants (additive)

**Owner**: `crates/scribe-common/src/protocol.rs#ServerMessage`
(serde-tagged enum, currently at line 295).

```rust
ServerMessage::ClipboardPromptRequest {
    session_id: SessionId,
    request_id: PromptId,
    op: ClipboardOp,
    selection: ClipboardSelection,
    preview: Option<String>,   // Some(_) only when op == Write
}

ServerMessage::ClipboardBridgeWrite {
    session_id: SessionId,
    selection: ClipboardSelection,
    payload: String,
}

ServerMessage::ClipboardBridgeReadRequest {
    session_id: SessionId,
    request_id: PromptId,
    selection: ClipboardSelection,
}
```

**Contract**: All three variants land additively. Older clients
that cannot deserialize them MUST be detected at attach time by C7
and never receive them. The server emits these strictly in the
order: optional `ClipboardPromptRequest` → optional
`ClipboardBridgeWrite` or `ClipboardBridgeReadRequest`. No
out-of-order interleaving for a given session.

## C3 — New ClientMessage variants (additive)

**Owner**: `crates/scribe-common/src/protocol.rs#ClientMessage`
(serde-tagged enum, currently at line 134).

```rust
ClientMessage::ClipboardPromptResponse {
    request_id: PromptId,
    decision: ClipboardDecision,
}

ClientMessage::ClipboardBridgeReadReply {
    request_id: PromptId,
    payload: Result<String, BridgeError>,
}
```

`BridgeError` is a small enum: `Unavailable`, `Empty`. Errors
map onto a server-side empty-reply OSC 52 payload (per UX-002
and decision 7 in research.md).

**Contract**: Client always echoes the `request_id` from the
matching `ServerMessage::ClipboardPromptRequest` /
`ClipboardBridgeReadRequest`. The server tolerates duplicate
responses (idempotent — second reply is ignored). The server
times out a missing reply at 30 s (matches existing IPC reply
timeouts) and treats the timeout as a deny.

## C4 — Server-side gating in `ipc_server.rs`

**Owner**: `crates/scribe-server/src/ipc_server.rs` (replaces the
`SessionEvent::ClipboardStore` / `ClipboardLoad` arms at lines
2617-2627; removes the `ServerClipboard` struct at lines 329-356;
adds `ClipboardBurstState` to `PtyReaderState`).

**Contract**: The new arms MUST:

1. Resolve the policy axis (`policy.read_mode` for `ClipboardLoad`,
   `policy.write_mode` for `ClipboardStore`).
2. For writes: check `payload.len() <= policy.max_write_bytes`
   first; reject and drop on oversize (per FR-009 / FR-015).
3. Apply the burst-decision-reuse state machine from data-model.md
   E3 / decision 5.
4. For `Allow`: skip prompt; for writes, emit
   `ServerMessage::ClipboardBridgeWrite`; for reads, emit
   `ServerMessage::ClipboardBridgeReadRequest` and defer the
   PTY-side reply until the client responds.
5. For `Deny`: skip prompt; for writes, drop; for reads, write an
   empty-payload OSC 52 reply via `write_term_response`.
6. For `Prompt`: state-machine in E3 decides whether to open a
   new prompt, reuse a recent decision, or defer onto an open
   prompt.

The arm MUST NOT do any host clipboard work directly; the bridge
is C5 (client-side).

## C5 — Client-side bridge in `main.rs`

**Owner**: `crates/scribe-client/src/main.rs` (extends the
existing IPC handler that processes `ServerMessage` variants).

**Contract**: On receiving:

- `ServerMessage::ClipboardPromptRequest`: instantiate a
  `ClipboardDialog` per data-model E4; route the dialog
  resolution into `ClientMessage::ClipboardPromptResponse`. If
  `decision` is `AlwaysAllow` or `AlwaysDeny`, ALSO send a
  settings-update message via the existing settings webview
  channel to persist the policy change.
- `ServerMessage::ClipboardBridgeWrite`: call
  `HostClipboardBridge::bridge_write` (data-model E5). Apply
  the `focus_gate_writes` check inside the bridge (decision 6).
  No reply to the server.
- `ServerMessage::ClipboardBridgeReadRequest`: call
  `HostClipboardBridge::bridge_read`; send
  `ClientMessage::ClipboardBridgeReadReply` with the result
  (or `BridgeError` on arboard failure).

The client MUST keep the dialog scoped to the originating pane
(per UX-001 and the spec edge case). Multiple windows can show
independent dialogs.

## C6 — `arboard::Clipboard` reuse

**Owner**: `crates/scribe-client/src/main.rs#clipboard` field
(`Option<arboard::Clipboard>`, lines 346 + 866).

**Contract**: The same handle services user-driven copy/paste
(existing path) AND the OSC 52 bridge (new path). No second
handle is created. The handle is wrapped in the existing error
tolerance — if arboard initialization failed at startup
(`clipboard: None`), the bridge silently no-ops, matching the
existing user-driven copy/paste behavior.

Primary selection on X11 goes through arboard's
`GetExtLinux::primary` / `SetExtLinux::primary` (already used by
the existing code paths at lines 9048 / 9117). Per spec
Assumptions, non-X11 platforms map primary to system clipboard
inside arboard.

## C7 — Attach-time clipboard-gating feature negotiation

**Owner**: `crates/scribe-server/src/ipc_server.rs` (attach
handler) + `crates/scribe-client/src/main.rs` (attach sender).

**Contract**: Both peers advertise a `clipboard_gating: bool`
capability flag in their existing attach-time message
(`ServerMessage::Hello` and `ClientMessage::Attach` or
equivalents — the exact field name follows the existing
`kitty_keyboard` precedent established in spec 008).

Cross-version behavior:

- **Server supports, client doesn't**: Server treats the
  session as headless for OSC 52 prompt purposes (decision 7).
  No new ServerMessage variants are sent. The audit's silent-
  exfiltration risk remains for that one mixed-version session
  until the user upgrades the client. Documented as expected
  during upgrade windows; quickstart includes a manual
  verification of this case.
- **Server doesn't support, client does**: Client silently
  no-ops any unexpected ServerMessage clipboard variants
  (defensive — shouldn't happen). User-driven paste path
  unchanged.
- **Both support**: Full feature active.

This negotiation is the same shape as the established
`kitty_keyboard` feature flag — see `lat.md/client.md#Key
Translation Priority` for the canonical pattern.

---

## Non-contracts (explicit)

These are deliberately NOT contracts; they are implementation
freedom for the plan phase to refine:

- The exact format of `BridgeError` (one enum, multiple
  variants, or one boolean) — internal client/server detail.
- The exact wire layout of the `PromptId` (a `u64`, a `Ulid`,
  etc.) — must be `Eq + Hash + Copy + Serialize` and that's it.
- The dialog body text and button labels — UX-001 says
  match the spec 009 dialog model, but the exact strings are
  drafted in the settings-UI copy pass.
- The 500 ms `burst_window_ms` default — plan-phase decision;
  may be tuned by quickstart verification feedback.
- The `pending_for_prompt` bound of 64 — implementation
  detail; the contract is "bounded".
