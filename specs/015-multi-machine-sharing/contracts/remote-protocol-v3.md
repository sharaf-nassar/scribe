# Contract: Remote Protocol v2 → v3 Delta

**Feature**: `015-multi-machine-sharing` | **Date**: 2026-07-22
**Source of truth**: `crates/scribe-common/src/protocol.rs`
**Related**: [research.md](../research.md) (D4), [data-model.md](../data-model.md)

Phase 1 output. The additive protocol delta from v2 to v3. All new messages and
fields ride the existing 4-byte-BE + named-MessagePack framing; every new struct
field carries `#[serde(default)]` per the established convention
(`WindowInfo.controller`, `Hello.takeover`). Line anchors are approximate.

## Version negotiation

- `REMOTE_PROTOCOL_VERSION`: **`2` → `3`** (`protocol.rs` ~:21).
- Negotiation is **exact match**, unchanged from 013: the preamble
  `ClientMessage::RemoteHandshake { remote_protocol_version, scribe_version,
  device_name }` (~:409, tailnet) and `ClientMessage::LanHello { device_name,
  remote_protocol_version }` (~:438, LAN) are checked before any window state is
  revealed. A mismatch is refused with the existing
  `RemoteRefusal::IncompatibleVersion` (~:1183) / `LanRefusal::IncompatibleVersion`
  (~:1219), both versions named. There is no partial-capability admission — a
  v2↔v3 pair never shares (D4).

---

## New `ClientMessage` variants

### `ControlClaim { window_id }`
- **Direction**: client → server.
- **Fields**: `window_id: WindowId`.
- **When**: a participant takes input control in `Shared view, single typist`
  mode with `control_acquisition = FreeClaim` (default), or the owning machine
  claims regardless of acquisition setting (FR-005, FR-007).
- **Server action**: if allowed, set `ControlState::SingleTypist.holder` to the
  claimer's `ParticipantId`, demote the previous holder to viewer (it stays
  live), broadcast `ShareRoster`. Ignored/no-op in `FreeForAll`; not applicable
  in `SingleController`.
- **Receives result via**: `ShareRoster` (new `holder`). No dedicated ack needed.
- **Back-compat**: additive variant; never sent by a v2 client or on the local
  Unix socket.

### `ControlRequest { window_id }`
- **Direction**: client → server.
- **Fields**: `window_id: WindowId`.
- **When**: a viewer asks for control under `control_acquisition = RequestAndGrant`.
- **Server action**: record `ControlState::SingleTypist.pending_request` and send
  `ControlRequested` to the current holder (or the owner if unheld). Cancelled on
  holder change or mode change (spec Edge Case).
- **Back-compat**: additive.

### `ControlGrant { window_id, participant_id, accept }`
- **Direction**: client → server (from the current holder or the owner).
- **Fields**: `window_id: WindowId`, `participant_id: ParticipantId` (`u64`,
  server-monotonic; the grant **target** — the requester), `accept: bool`.
- **When**: the holder/owner answers a pending `ControlRequest`.
- **Server action**: on `accept = true`, transfer `holder` to `participant_id`
  and broadcast `ShareRoster`; on `false`, clear `pending_request` and send
  `ControlDenied` to the requester. Only honored from the approver named by the
  request (FR-005).
- **Back-compat**: additive.

### `Resize` — reinterpreted as a per-participant viewport report (same wire shape)
- **Direction**: client → server (**unchanged shape**:
  `Resize { session_id: SessionId, size: TerminalSize }`, ~:236).
- **Semantics change (v3)**: in a share, `Resize` no longer sets the session grid
  directly. Each participant's `size` is stored as `Participant.viewport`, and the
  server sets the authoritative grid to `min(rows)` × `min(cols)` across attached
  participants, debounced 250 ms, via the existing `resize_term` +
  `TIOCSWINSZ` path (`handle_resize` ~:5158). See data-model `AuthoritativeGrid`
  and research D3.
- **When / who receives**: sent by every participant (including the owner) on its
  own window resize; the resulting grid change is applied server-side and reflected
  to all participants through normal `PtyOutput`/reflow.
- **Authorization (v3)**: in shared modes `Resize` is **exempt from control
  gating** — it is accepted from any attached participant (including viewers) as
  an informational viewport report, so a viewer's smaller window can drive
  smallest-wins. Only in `SingleController` mode does `Resize` stay
  controller-gated (legacy direct grid-set).
- **Back-compat**: **wire-identical** to v2, so a v2 peer's `Resize` is still a
  valid viewport report — but a v2 peer is never admitted to a share (exact-match
  refusal), so the reinterpretation only ever applies among v3 peers. In
  `SingleController` mode `Resize` retains its exact v2 meaning (single holder →
  direct grid set).

---

## New / changed `ServerMessage` variants

### `ShareRoster { window_id, participants, mode, holder }`
- **Direction**: server → all participants (full-state broadcast, D8).
- **Fields**:
  - `window_id: WindowId`
  - `participants: Vec<ParticipantInfo>` where
    `ParticipantInfo { participant_id: ParticipantId (u64), device_name: String,
    login_name: String, is_local: bool, is_holder: bool }`
  - `mode: SharingMode`
  - `holder: Option<ParticipantId>`
- **When**: on every join, leave, control transfer, ejection, and mode change
  (FR-008, SC-005). No deltas — always the complete current roster.
- **Who receives**: every attached participant for that window.
- **Back-compat**: additive; a v2 client would never negotiate into a share to
  receive it. `ParticipantInfo` reuses the `device_name`/`login_name` pair of
  `ControllerInfo` (~:128) so the identity surface never drifts.

### `ControlRequested { window_id, from }`
- **Direction**: server → the current holder (or owner if unheld).
- **Fields**: `window_id: WindowId`, `from: ParticipantInfo` (the requester).
- **When**: a `ControlRequest` arrives under `RequestAndGrant`.
- **Who receives**: only the approver.
- **Back-compat**: additive.

### `ControlDenied { window_id }`
- **Direction**: server → the requester.
- **Fields**: `window_id: WindowId` (optionally a reason string).
- **When**: a `ControlGrant { accept: false }`, or the request was cancelled by a
  holder/mode change (spec Edge Case).
- **Back-compat**: additive.

### `ShareEnded { window_id, reason }`
- **Direction**: server → affected participants.
- **Fields**: `window_id: WindowId`, `reason: ShareEndReason` (e.g.
  `OwnerClosed`, `WindowClosed`, `ModeChangedToSingleController`).
- **When**: the owning machine closes the window/session or flips to
  `SingleController` (FR-017); for the mode flip, remote participants also receive
  the legacy `WindowTakenOver` for the frozen displaced UI.
- **Who receives**: every remote participant of the ending share.
- **Back-compat**: additive. (For a `SingleController` flip the retained
  `WindowTakenOver` is what the existing client already knows how to render;
  `ShareEnded` is the mode-neutral notice for the roster/notice surface.)

### `WindowTakenOver { device_name, login_name }` — retained, legacy only
- **Unchanged** (~:792). Used ONLY for exclusive-takeover displacement and the
  `SingleController` mode flip (FR-003, FR-017) — the frozen dimmed-frame
  experience driven by `LostControlState`
  (`crates/scribe-client/src/main.rs` `handle_window_taken_over` ~:6860). Never
  sent for an additive share join or a single-typist control pass (those keep the
  displaced machine live and use `ShareRoster`).

### `Welcome { …, participant_id }` — additive field
- **Changed**: `Welcome` gains `participant_id: Option<u64>`, `#[serde(default)]`.
- **Meaning**: the connection's own server-assigned `ParticipantId` in the
  window's share, populated on every claim/join that registers a participant so a
  remote client can match itself in a subsequent `ShareRoster` **exactly** (its
  own `is_holder`) rather than comparing device names. `None` from an older
  server, or when the claim registered no participant (a lost-control landing).
- **Compatibility**: additive `#[serde(default)]` — local and legacy flows
  deserialize unchanged.

---

## Changed struct: `WindowInfo`

`WindowInfo` (~:136) gains sharing fields, all `#[serde(default)]`:

| New field | Type | Meaning |
|---|---|---|
| `participants` | `Vec<ControllerInfo>` | attached remote participants (empty from an older server or a locally-controlled/unconnected window) |
| `mode` | `Option<SharingMode>` | the window's sharing mode; `None` decodes from older servers |
| `participant_count` | `usize` (optional) | picker shows share occupancy instead of a binary in-use flag |

The existing `controller: Option<ControllerInfo>` (~:150) is retained: in
`SingleController` mode it still names the sole holder; in shared modes it names
the current `holder` (or `None` when unheld). The connect picker uses
`participants`/`participant_count` to show "N attached" instead of 013's binary
in-use.

---

## Unchanged messages (and why)

- **`Hello { window_id, clipboard_gating, takeover }`** (~:329) — **no new
  fields**. Join semantics are decided **server-side by the owning machine's
  mode**, not by the joiner: `takeover: false` is an additive share join when the
  mode permits, or the 013 assign/`LostControl` outcome in `SingleController`;
  `takeover: true` keeps its exact 013 meaning — an *exclusive claim* that ends any
  share (FR-003). A joiner needs no new field to express "I want to share" because
  sharing is the owner's mode, not the joiner's request (research micro-decision).
- **`RemoteHandshake` / `LanHello`** — only the constant they carry changes
  (2 → 3); shapes unchanged.
- **`PtyOutput`, `SessionReplay`, `KeyInput`, `CloseSession`, `CloseWindow`,
  `FocusChanged`, `SearchRequest`** — unchanged shapes; their *authorization*
  changes server-side (mode-aware `connection_may_type`, research D2) but the wire
  format does not.
- **`RemoteRefusal` / `LanRefusal`** — no new variant; participant-limit refusal
  **reuses `Busy`** (~:1191 / ~:1229), matching the spec's "existing busy-style
  refusal" (FR-018).

---

## Compatibility matrix

| Dialer | Listener | Outcome |
|---|---|---|
| v3 client | v3 server | full sharing per the owner's mode |
| v2 client | v3 server | exact-match fails → `IncompatibleVersion` refusal (both versions named); no half-join (FR-014) |
| v3 client | v2 server | v2 server advertises version 2 → dialer's exact-match fails → `IncompatibleVersion`; or, if the v2 server has no listener, connection refused → "unreachable/disabled" UX (013 D3) |
| local Unix-socket client | local server | **completely unchanged** — the preamble and all v3 remote messages are never sent on the local socket; `Hello`/`Resize`/`KeyInput` behave exactly as v2 (SC-006) |
