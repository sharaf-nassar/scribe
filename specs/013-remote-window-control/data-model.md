# Data Model: Remote Window Control over Tailscale

**Feature**: `013-remote-window-control` | **Date**: 2026-07-03
**Sources**: [spec.md](./spec.md) Key Entities, [research.md](./research.md) D1–D9

Runtime-only unless noted; the only persisted artifact is the `[remote]`
config table. No database, no new files on disk besides config and log
lines.

## RemoteConfig (persisted, TOML `[remote]`)

| Field | Type | Default | Rules |
|---|---|---|---|
| `enabled` | bool | `false` | FR-001: listener exists only while true; disable severs live connections (FR-016) |
| `port` | u16 | `46061` | Bind port on tailnet addresses only (FR-002); rebind on change via live reload |

Lives in `scribe-common/src/config.rs` types, applied by the server on
`ConfigReloaded` (D8). Future extension point: authorization list for
cross-user sharing (out of v1 scope; do not add fields now).

## TailnetIdentity (runtime, derived per connection)

Resolved by `tailnet.rs` from LocalAPI (D2); never persisted.

| Field | Type | Rules |
|---|---|---|
| `node_name` | String | MagicDNS short name; shown in banner/indicators (FR-009) |
| `user_id` | u64/opaque id | Stable tailnet account id; authorization compares this, not display names |
| `login_name` | String | Display only (banner, audit) |
| `is_tagged` | bool | Tagged ⇒ refuse (FR-003) |

**Validation**: connection proceeds only if `peer.user_id ==
self.user_id`, peer is not tagged, and lookup succeeded; any LocalAPI
failure ⇒ refuse (FR-015, fail closed).

## RemotePeer (runtime, connecting side)

Picker entries from LocalAPI `status` (D7).

| Field | Type | Rules |
|---|---|---|
| `name` | String | MagicDNS short name; also manual-entry target (FR-004) |
| `addr` | IP | Tailnet address to dial |
| `same_account` | bool | Picker lists only `true`; manual entry may still be refused server-side |
| `online` | bool | Offline peers shown greyed or omitted |

## RemoteConnection (runtime, owning side)

One per accepted TCP connection; parallels the existing per-client
connection state. Together with [WindowControl](#windowcontrol-runtime-owning-side--extension-of-existing-ownership)
this realizes the spec's "Remote Attachment" entity: RemoteConnection is
the link lifecycle, WindowControl the ownership relationship.

| Field | Type | Rules |
|---|---|---|
| `identity` | TailnetIdentity | Set before any protocol traffic is processed |
| `negotiated_version` | u32 | Equals `REMOTE_PROTOCOL_VERSION` (exact match, D3) |
| `window_id` | Option<WindowId> | Set after successful `Hello` claim |
| `output_queue` | bounded queue | Overflow ⇒ drop backlog + mark replay-dirty (D5) |
| `replay_dirty` | bool | True ⇒ send fresh `SessionReplay` when writable |

**State transitions**:

```text
Accepted → IdentityVerified → VersionNegotiated → Ready(Hello/Welcome)
        ↘ Refused(reason)  ↘ Refused(version)   → Attached(window)
Attached → Detached (disconnect/error)  — sessions unaffected (FR-010)
Attached → Displaced (another controller took over, FR-007)
Any     → Severed (remote access disabled, FR-016)
```

Cap: 8 concurrent remote connections (separate from the 32 local cap).

## WindowControl (runtime, owning side — extension of existing ownership)

Extends today's `connected_clients: WindowId → writer` single-writer model.

| Field | Type | Rules |
|---|---|---|
| `controller` | Local(uds) \| Remote(TailnetIdentity) | Exactly one at a time (FR-007); controller identity is exposed to window-listing/status surfaces (FR-009b, SC-006) |
| `displaced` | Option<ControllerInfo> | Who was displaced; cleared on next claim |
| per-connection capability state | e.g. `clipboard_gating` | Follows the CURRENT controller's `Hello`; re-bound atomically at takeover (FR-014) |

**Transitions** (all under the existing claim lock, atomic):

```text
Unconnected --claim--> Controlled(X)
Controlled(X) --claim{takeover} by Y--> Controlled(Y) + WindowTakenOver→X
Controlled(X) --X disconnects--> Unconnected (window reattachable)
```

Reclaim is the same `claim{takeover}` edge in the opposite direction (D4).
A claim without `takeover` against a Controlled window keeps today's
behavior: a different/new window is assigned.

## LostControlState (runtime, displaced client side)

What a client renders after `WindowTakenOver` (clarification #2).

| Field | Type | Rules |
|---|---|---|
| `frozen_frame` | last rendered grid | Content as of transfer; dimmed; never updated (no live fan-out in v1) |
| `controller` | device + account strings | Shown in banner (FR-009) |
| `reclaim` | action | One action ⇒ reconnect with `takeover = true` |

## ReconnectState (runtime, remote client side)

| State | Behavior |
|---|---|
| `Active` | Normal operation |
| `Reconnecting { attempt, next_delay }` | Auto-retry with capped exponential backoff; visible, cancelable (FR-011) |
| `Disconnected` | After cancel; one-action reconnect |
| `Refused { reason }` | Terminal until user acts; distinct copy per UX-002 |

On successful reconnect: preamble → `Hello { takeover: false }` for the
same window. Window unconnected (common case) ⇒ normal claim + fresh
replay rebuilds all panes (convergence, FR-011). Window held by another
controller ⇒ `Welcome` + immediate `WindowTakenOver`, client enters
LostControlState — automatic reconnection never seizes control (FR-011).

## AuditEvent (server log, owning side)

Structured log records (D9), FR-017.

| Field | Type |
|---|---|
| `timestamp` | log-native |
| `kind` | `accepted` \| `refused` \| `disconnected` \| `severed` |
| `peer` | node name + login name (when known) |
| `reason` | typed refusal reason for `refused`, mirroring the wire `RemoteRefusal` exactly (disabled / unauthorized / identity-unavailable / version / busy), plus optional `detail=tagged` qualifier |
| `window` | Option<WindowId> |

## Unchanged existing entities (referenced)

- **Window / WindowId** — unit of attach; server tracks connected and
  unconnected windows with their workspace trees.
- **Session / SessionId** — server-owned PTY; lifetime independent of any
  client (FR-010 rests on this).
- **SessionReplay** — zstd ANSI full-state stream; reused verbatim for
  remote attach and resync.
- **ClipboardPolicy / capability bit** — applies unchanged with the
  controlling client as bridge endpoint (FR-014).
