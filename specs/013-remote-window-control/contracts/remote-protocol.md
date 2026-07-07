# Contract: Remote Wire Protocol

**Feature**: `013-remote-window-control` | **Date**: 2026-07-03
**Scope**: Everything that crosses the TCP link, plus the protocol deltas
visible to local clients. Grounded in research.md D1–D6.

## Transport

- Listener: TCP, bound ONLY to the machine's Tailscale addresses (v4/v6),
  port `remote.port` (default 46061). Exists only while `remote.enabled`.
- Framing: identical to local — 4-byte big-endian u32 length + named
  MessagePack body; 64 MiB max frame. `framing.rs` reused unchanged.
- Connection cap: 8 concurrent remote connections; excess connections are
  refused with a typed `Busy` reply after the preamble is read.

## Connection establishment sequence (owning side)

```text
1. accept()                    — TCP connection on tailnet address
2. Read RemoteHandshake        — first frame MUST be this preamble
                                 (bounded read; carries no window data)
3. WhoIs(peer ip:port)         — LocalAPI; ANY failure ⇒ IdentityUnavailable
4. Authorize                   — peer.user_id == self.user_id, not tagged
                                 (failure ⇒ Unauthorized)
5. Version gate                — exact REMOTE_PROTOCOL_VERSION match
                                 (failure ⇒ IncompatibleVersion)
6. Write RemoteHandshakeReply  — accepted, or typed refusal + close
7. Normal protocol begins      — Hello/Welcome and the existing catalogue
```

The preamble is read FIRST so every refusal — `Disabled` (race with
disable), `Unauthorized`, `IdentityUnavailable`, `IncompatibleVersion`,
`Busy` — reaches the caller as a typed `RemoteHandshakeReply`, which is
what makes the distinct FR-004/UX-002 copy possible. FR-003 is preserved:
the preamble reveals no window or session information, and no other frame
is processed until identity, authorization, and version all pass. Only a
malformed or non-`RemoteHandshake` first frame closes bare (nothing
protocol-aware to reply to). Every outcome emits an AuditEvent.

## New wire messages (scribe-common/src/protocol.rs)

```rust
/// Remote-only preamble. NEVER sent on the local Unix socket.
ClientMessage::RemoteHandshake {
    remote_protocol_version: u32,   // REMOTE_PROTOCOL_VERSION of the dialer
    scribe_version: String,         // human version for error copy
    device_name: String,            // dialer's short name, display only
}

ServerMessage::RemoteHandshakeReply {
    accepted: bool,
    refusal: Option<RemoteRefusal>, // present iff !accepted
    server_remote_protocol_version: u32,
    server_scribe_version: String,
}

enum RemoteRefusal {            // typed; maps 1:1 to UX-002 copy
    Disabled,                   // remote access off (races with disable)
    Unauthorized,               // wrong account / tagged / unknown identity
    IdentityUnavailable,        // tailscaled/WhoIs down (fail closed)
    IncompatibleVersion,        // exact-match failure (both versions in reply)
    Busy,                       // remote connection cap reached
}

/// Sent to a client whose window was claimed by another controller.
ServerMessage::WindowTakenOver {
    device_name: String,        // new controller's device (or "this machine")
    login_name: String,         // new controller's account display name
}

/// Best-effort final frame before the server closes a remote connection
/// for a policy reason (bounded, non-blocking send; the close follows
/// regardless of delivery). v1 reason: Disabled (remote access turned off).
ServerMessage::RemoteDisconnect {
    reason: RemoteRefusal,
}
```

`REMOTE_PROTOCOL_VERSION: u32` starts at `1`; bump on ANY change to
remote-visible message semantics. v1 policy: exact match (research D3).

## Changed wire messages

```rust
ClientMessage::Hello {
    window_id: Option<WindowId>,
    clipboard_gating: bool,        // existing
    #[serde(default)]
    takeover: bool,                // NEW: claim a currently-connected window
}
```

- `takeover = false` on the LOCAL socket (and all existing local clients,
  via serde default): today's behavior exactly — a connected window is
  never displaced; the claimant gets a different/new window.
- `takeover = false` on the REMOTE transport targeting a CONNECTED window:
  the server does NOT displace and does NOT silently assign a different
  window — it completes `Welcome` for the requested window id and
  immediately sends `WindowTakenOver` naming the current controller, with
  no sessions attached. The client renders the standard lost-control state.
  This is the auto-reconnect path: a dropped remote client reconnects with
  `takeover = false`, resumes normally when its window is unconnected (the
  common case), and lands in the lost-control state — never a silent
  seizure — when someone else took the window mid-outage (FR-011).
- `takeover = true`: explicit user action only (first attach from the
  picker, reclaim from the lost-control banner). If the window is
  connected, the server atomically swaps the writer, sends
  `WindowTakenOver` to the displaced client, and continues the normal
  attach flow for the claimant. Works identically for local and remote
  claimants (reclaim = same message from the other side). All
  per-connection state — including the `clipboard_gating` capability bit
  and clipboard-bridge routing — follows the NEW controller's `Hello` from
  the moment of the swap; no stale capability or policy state may survive
  a takeover.
- `Hello { window_id: None }` over remote: creates a fresh window
  (clarification #3 — remote create).

## Semantics over the remote link (unchanged messages)

- `Welcome`, `AttachSessions`, `Subscribe`, `RequestSnapshot`,
  `SessionReplay`, `PtyOutput`, `KeyInput` (4 KiB cap), `Resize`,
  scroll/search, workspace and notes messages: byte-identical semantics.
- Flow control (research D5): each remote connection has a bounded output
  queue. On overflow the server drops that connection's queued `PtyOutput`,
  marks affected sessions replay-dirty, and sends a fresh `SessionReplay`
  when the link drains. Clients MUST treat any `SessionReplay` as full
  pane-state replacement (already true for reattach).
- Clipboard: `ClipboardPromptRequest/Response`, `ClipboardBridgeWrite`,
  `ClipboardBridgeReadRequest/Reply` route to the CURRENT controller —
  i.e. the controlling machine's clipboard is the policy endpoint (FR-014).
  The `clipboard_gating` capability bit is advertised per-connection as
  today.
- Transient no-Hello connections (update checks, hook events) remain
  LOCAL-ONLY: over TCP, any first frame other than `RemoteHandshake` closes
  the connection.

## Local helper messages (Unix socket only)

The connecting machine's picker gets peer data from ITS OWN local server
(the LocalAPI client lives in `scribe-server/src/tailnet.rs`; the GUI
client never talks to tailscaled directly):

```rust
ClientMessage::ListRemotePeers                     // local-only request
ServerMessage::RemotePeerList {
    peers: Vec<RemotePeerInfo>,                    // same-account, with
}                                                  //   name/addr/online
```

Over TCP these are refused like any other pre-`RemoteHandshake` frame; a
remote peer has no business enumerating a third machine's tailnet view.

## Displaced-client obligations (both directions)

On `WindowTakenOver` the client MUST: stop sending input for that window,
render the last frame dimmed under a banner naming
`device_name`/`login_name`, and offer one-action reclaim (fresh connection,
`Hello { takeover: true }`). It MUST NOT expect further `PtyOutput` (no
fan-out in v1).

## Disable semantics

On `remote.enabled → false` (live reload), within 2 seconds (FR-016):
stop accepting, send each remote connection a best-effort
`RemoteDisconnect { reason: Disabled }` final frame, close every remote
connection, and close the listener. The delivered notice is what lets the
remote client state "remote access was disabled on <peer>" as fact rather
than inference. If the notice is lost (crash, dead link), the client falls
back to the reconnect path, where the vanished listener yields the
combined connection-failure copy (offline / not running / disabled —
FR-004): a disabled machine is deliberately indistinguishable from an
unreachable one on a cold connect, because FR-001 forbids leaving anything
listening. Owning-side sessions are untouched (FR-016, FR-010).

## Compatibility statement (Constitution: Engineering Constraints)

- Local Unix-socket clients: zero observable change (`takeover` defaults
  false; new messages never appear locally except `WindowTakenOver`, which
  only follows an explicit takeover claim — old clients that predate it
  tolerate unknown-variant decode failure ONLY by upgrade pairing, so the
  client and server of one install upgrade together as today).
- Remote pairs: gated by exact `REMOTE_PROTOCOL_VERSION` match with typed
  refusal; old servers have no listener, yielding the distinct
  "unreachable or disabled" outcome.
- Server↔server handoff: `HANDOFF_VERSION` bumps to carry the
  remote-listener flag + active remote connection metadata is NOT attempted
  — remote connections drop on upgrade and auto-reconnect (D6); only the
  enabled-state re-derives from config after handoff.
