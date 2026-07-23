# Data Model: Multi-Machine Collaborative Window Sharing

**Feature**: `015-multi-machine-sharing` | **Date**: 2026-07-22
**Spec**: [spec.md](spec.md) | **Research**: [research.md](research.md)

Phase 1 output. Entities, fields, relationships, validation, and state
transitions for the server-side share registry that replaces feature 013's
single-writer model. Line anchors into
`crates/scribe-server/src/ipc_server.rs` and
`crates/scribe-common/src/protocol.rs` are approximate; named types are
authoritative.

## Entity overview and relationships

```
IpcServerState
  └── window_shares: Arc<RwLock<HashMap<WindowId, WindowShare>>>   (D1 — replaces
        connected_clients + window_controllers + window_clipboard_gating)
             │  1
             │
             ▼  N
        WindowShare
          ├── participants: HashMap<ParticipantId, Participant>
          ├── control: ControlState
          ├── grid: AuthoritativeGrid
          └── settings snapshot (mode, control_acquisition, participant_limit)

        Participant  ──references──▶  SharedWriter (Arc<Mutex<ClientSink>>)
                                       └── ClientSink::Remote(RemoteSink)  (per-participant queue)
```

`SharingSettings` is global (one owning machine's Remote settings), snapshotted
into each `WindowShare` at mutation time so a live share reads a consistent mode.

---

## Entity: WindowShare

The full state of one window shared across machines — the single source of truth
that replaces the three per-window maps and their fixed lock order (D1).

| Field | Type | Notes |
|---|---|---|
| `window_id` | `WindowId` | map key |
| `participants` | `HashMap<ParticipantId, Participant>` | always ≥1 (the local owner is always present) |
| `control` | `ControlState` | see enum below; mode-dependent |
| `grid` | `AuthoritativeGrid` | min-of-viewports (FR-012) |
| `mode` | `SharingMode` | snapshot of `SharingSettings.sharing_mode` at last mutation |
| `control_acquisition` | `ControlAcquisition` | snapshot; only meaningful in single-typist |
| `participant_limit` | `Option<u32>` | snapshot; `None` = unlimited (FR-018) |

**Lock discipline (migration note).** Today three maps
(`ConnectedClients` ~:139, `WindowControllers` ~:213, `WindowClipboardGating`
~:148) move together under the `WindowOwnership<'a>` tri-lock (~:3942) in the
fixed order `connected → controllers → gating`, coordinated by
`resolve_and_register_claim` (~:3985). `WindowShare` folds all three into one
`RwLock`d map entry: every roster / control / gating / grid mutation is a
**single write-lock acquisition** on `window_shares`, so the tri-lock order and
its documented drift hazard (a losing takeover overwriting the winner's gating
bit) cease to exist. The `Arc::ptr_eq` identity-token invariant from
`connection_controls_window` (~:4126) and `release_window_if_owned` is preserved
inside a participant: a `Participant` is identified by its `SharedWriter` Arc, so
a stale disconnect still cannot evict a newer participant.

**Validation**
- `participants` non-empty (owner invariant). Removing the last remote leaves the
  local owner; the share is torn down only when the window itself closes.
- `participant_limit`, when `Some(n)`, bounds only **remote** joins; the local
  owner is exempt (FR-007, FR-018).
- `grid` rows/cols ≥ 1 (reuses `TerminalSize::has_grid()`).

---

## Entity: Participant

One attached machine's membership in a share (spec "Participant"). Absorbs the
per-connection state 013 spread across the three maps plus the existing
`RemoteSink` queue state.

| Field | Type | Source / notes |
|---|---|---|
| `id` | `ParticipantId` (`u64`, server-monotonic per share registry) | stable for a connection's lifetime; roster + grant target |
| `writer` | `SharedWriter` (`Arc<Mutex<ClientSink>>` ~:119) | identity token (`Arc::ptr_eq`); fan-out sink |
| `identity` | `ControllerIdentity` (~:157) | `Local` (owner) or `Remote { device_name, login_name }` |
| `transport` | `Transport` enum | Local / Tailnet / Lan (drives audit + eject-on-revoke) |
| `viewport` | `TerminalSize` | last `Resize` report (D3); feeds min-grid |
| `clipboard_gating` | `bool` | per-participant OSC-52 capability (moved from `WindowClipboardGating`, D7) |
| `joined_at` | `Instant` / timestamp | roster ordering + audit |
| `queue_state` | via existing `RemoteSink` (~:826) → `RemoteOutputShared` | replay-dirty set, `queued_pty_bytes`, `closed` — unchanged 013 flow control (D5) |
| `role` | derived, not stored | viewer vs holder computed from `ControlState` + `mode` |

**Local owner participant.** Exactly one participant per share has
`identity = ControllerIdentity::Local` and `transport = Local`. It is created
with the window, never joins/leaves via the remote path, is exempt from
`participant_limit`, and is the fallback control holder and OSC-52 route (FR-007,
FR-013). Its `writer` is `ClientSink::Local` (inline write, no `RemoteSink`).

**Validation**
- A remote participant must have passed the 013 WhoIs / 014 device-approval gate
  before a `Participant` is constructed (FR-010) — sharing adds no new access
  path.
- `role` is never "holder" for a participant absent during reconnect grace (D3
  exclusion) — a reconnecting participant re-enters as viewer (research
  reconnect semantics).

---

## Entity: ControlState (enum)

Who may type, per mode (D2). Replaces the implicit single-writer holder that
`connection_controls_window` (~:4126) derives from `ConnectedClients`.

```
enum ControlState {
    /// Single controller mode — legacy 013 exclusive ownership. The writer Arc
    /// IS the holder (Arc::ptr_eq), identical to today.
    LegacyExclusive { writer: SharedWriter },

    /// Shared view, single typist. At most one holder; None = unheld/claimable.
    SingleTypist {
        holder: Option<ParticipantId>,
        pending_request: Option<PendingRequest>,   // request-and-grant only
    },

    /// Collaborative free-for-all. Every attached participant may type; there is
    /// no distinguished holder.
    FreeForAll,
}

struct PendingRequest {
    requester: ParticipantId,
    // approver = current holder, or owner if unheld (FR-005)
}
```

**Validation / invariants**
- `SingleTypist.holder`, when `Some`, must name a live participant; on that
  participant's detach/eject it becomes `None` (FR-016 — no silent inheritance).
- `pending_request` is only ever `Some` under `control_acquisition = RequestAndGrant`
  and is cleared on grant, deny, holder change, or mode change (FR-005, spec Edge
  Case: mode flip cancels a pending request and informs the requester).
- `FreeForAll` has no holder field — authorization is pure membership (D2).
- The owning machine can always transition to holder regardless of mode or
  `control_acquisition` (FR-007).

---

## Entity: SharingSettings

The owning machine's Remote settings governing joins and input authorization
(spec "Sharing Mode", FR-004/005/018). Real config location:
**`crates/scribe-common/src/config.rs`**, added to `RemoteConfig` (~:1937) — NOT
`scribe-server/src/config.rs`, which only imports `RemoteConfig`. Applied live
over `ConfigReloaded` (D6); no restart.

| Config field | Type | Default | FR |
|---|---|---|---|
| `sharing_mode` | `SharingMode` = `SingleController \| SharedSingleTypist \| FreeForAll` | `SingleController` | FR-004 |
| `control_acquisition` | `ControlAcquisition` = `FreeClaim \| RequestAndGrant` | `FreeClaim` | FR-005 |
| `participant_limit` | `Option<u32>` | `None` (unlimited) | FR-018 |

All `#[serde(default)]` so an existing config file loads with legacy behavior
(FR-014, SC-006). A change to any field triggers immediate application to active
shares (FR-017 transitions below).

---

## Entity: AuthoritativeGrid

The one terminal grid the session's PTY runs at (FR-012), sized smallest-wins.

| Field | Type | Notes |
|---|---|---|
| `rows` | `u16` | `min(participant.viewport.rows)` over attached, non-reconnecting participants |
| `cols` | `u16` | `min(participant.viewport.cols)` |
| `debounce` | 250 ms coalesce window (D3) | one `resize_term` + `TIOCSWINSZ` per settled change |

**Computation.** On any `Resize` report (D3) or membership change, recompute
`min` over the current viewport snapshot after the debounce settles, then drive
the existing `handle_resize` primitives (`resize_term` + `set_pty_winsize`,
~:5158). Order-independent (pure function of the participant set) → no flapping
(spec Edge Case). Regrow: a participant detaching removes its viewport from the
min, so the grid grows back to the next-smallest.

---

## Entity: PresenceEvent / ShareRoster payload

The full-state roster broadcast on every membership/control/mode change (D8,
FR-008). Wire shape in `contracts/remote-protocol-v3.md`
(`ServerMessage::ShareRoster`). Reuses `ControllerInfo { device_name, login_name }`
(~:128) per entry plus `is_local` / `is_holder`.

| Field | Type |
|---|---|
| `window_id` | `WindowId` |
| `participants` | `Vec<ParticipantInfo { participant_id, device_name, login_name, is_local, is_holder }>` |
| `mode` | `SharingMode` |
| `holder` | `Option<ParticipantId>` |

Every join, leave, control transfer, ejection, and share-end is also written to
the existing 013 remote-audit surface (FR-015, SC-007).

---

## State transitions

### Participant lifecycle

| From | Event | To | Notes |
|---|---|---|---|
| — | remote passes trust gate, mode permits sharing, under limit | `joining` | else refused (see limit row) |
| `joining` | writer registered in `WindowShare.participants`, replay sent | `attached (viewer)` | additive; no existing participant disturbed (FR-002) |
| `attached (viewer)` | claims/granted control (mode-dependent) | `attached (holder)` | FR-005 |
| `attached (holder)` | another claims (free-claim) or grants elsewhere | `attached (viewer)` | previous holder stays live (FR-005, US2) |
| `attached (*)` | clean leave / disconnect | `detached` | roster broadcast; if holder, control → unheld (FR-016) |
| `attached (*)` | device revoked / transport severed | `ejected` | immediate, that participant only (FR-011) |
| `detached` | auto-reconnect (013 D6) | `attached (viewer)` | rejoins as viewer; free-for-all → typist (research reconnect) |
| `joining` | `participant_limit` reached | `refused (Busy)` | `RemoteRefusal::Busy` / `LanRefusal::Busy`; share undisturbed (FR-018) |

### Control transitions per mode

**Single controller (`LegacyExclusive`)** — unchanged 013 behavior:

| Event | Result |
|---|---|
| additive join attempt | not applicable — mode does not share; `Hello{takeover:false}` gets 013 assign/`LostControl` |
| `Hello { takeover: true }` | writer swapped under lock; displaced gets `WindowTakenOver` (legacy) |

**Shared view, single typist (`SingleTypist`)**:

| From holder | Event | Acquisition = FreeClaim | Acquisition = RequestAndGrant |
|---|---|---|---|
| `None` (unheld) | participant claims (`ControlClaim`) | becomes holder instantly | becomes holder (nothing to approve when unheld) or owner may claim |
| `Some(X)` | participant Y requests | Y takes control instantly | `pending_request = Y`; holder X (or owner) approves via `ControlGrant` |
| `Some(X)` | owner claims | owner becomes holder (always allowed, FR-007) | owner becomes holder (always allowed) |
| `Some(X)` | X detaches/ejected (**FR-016**) | `holder → None` (unheld); roster informs all; anyone may claim | same; `pending_request` cleared |
| any | mode flip → SingleTypist (**FR-017**) | all demoted to viewers, `holder = None` | same |
| any | `Hello { takeover: true }` (**FR-003**) | share ends: **every** attached participant is detached and sent `WindowTakenOver` (legacy displaced notice); claimer becomes sole controller (`LegacyExclusive`) — not just a sole-writer swap | same |

**Collaborative free-for-all (`FreeForAll`)**:

| Event | Result |
|---|---|
| any participant types (`KeyInput`) | delivered, interleaved in arrival order (US4, FR-004) |
| `Resize` viewport report | accepted from any attached participant (ungated in shared modes, D3); feeds min-grid |
| lifecycle / focus / search action | authorized for the owning machine only — `CloseSession`, `CloseWindow`, `FocusChanged`, `SearchRequest` fall to the `Local` participant since no control holder exists in this mode (spec Assumptions, D2) |
| control claim/request | no-op (everyone already types); roster still shows no single holder |
| `Hello { takeover: true }` (**FR-003**) | share ends: **every** attached participant is detached and sent `WindowTakenOver` (legacy displaced notice); claimer becomes sole controller (`LegacyExclusive`) |

### Mode-change application (FR-017, immediate)

| New mode | Effect on active share |
|---|---|
| `SharedSingleTypist` | demote all participants to viewers, `control = SingleTypist { holder: None, pending_request: None }`; broadcast roster; every participant informed |
| `SingleController` | detach all **remote** participants with `WindowTakenOver` (legacy displaced notice); owner retains sole control (`LegacyExclusive`) |
| `FreeForAll` | `control = FreeForAll`; all participants become typists; broadcast roster |

A pending `ControlRequest` is cancelled on any mode change and the requester
informed (spec Edge Case).

---

## FR → entity/transition traceability

| FR | Satisfied by |
|---|---|
| FR-001 | `WindowShare.participants` (multi); D1 fan-out via each `Participant.writer` |
| FR-002 | Participant lifecycle `joining → attached` is additive; no writer swap (supersedes 013 FR-007) |
| FR-003 | `ControlState::LegacyExclusive` + `Hello{takeover:true}` path; detach broadcasts `WindowTakenOver` |
| FR-004 | `SharingSettings.sharing_mode` → `WindowShare.mode`; `ControlState` three arms |
| FR-005 | `ControlState::SingleTypist` transitions; `SharingSettings.control_acquisition`; owner-always-claim invariant |
| FR-006 | `connection_may_type == false` drops input; roster (`holder`) drives the claim/request affordance |
| FR-007 | Local owner participant always present + always-claim + can end share for all (mode flip / takeover); ending for a single participant is via device-revoke / transport-sever (FR-011 `ejected` transition), no separate eject affordance in v1 |
| FR-008 | `ShareRoster` full-state broadcast (D8) on every membership/control change |
| FR-009 | Per-participant `RemoteSink` queue + `drop_pty_backlog`/`send_resync_replay` (D5) |
| FR-010 | `Participant` only constructed after 013/014 trust gate; `transport` recorded; no new path |
| FR-011 | Participant `ejected` transition — that participant only |
| FR-012 | `AuthoritativeGrid` min-of-viewports + regrow; clients render responsively |
| FR-013 | D7 routing: acting-machine paste/link; OSC-52 → holder, owner fallback (`is_local`) |
| FR-014 | serde-default config + exact-match `REMOTE_PROTOCOL_VERSION` bump → `IncompatibleVersion` |
| FR-015 | membership/control changes on existing 013 remote-audit surface |
| FR-016 | holder-detach → `SingleTypist.holder = None`; roster informs; claimable |
| FR-017 | Mode-change application table (immediate demote/detach) |
| FR-018 | `participant_limit` snapshot; over-limit join → `refused (Busy)` |
