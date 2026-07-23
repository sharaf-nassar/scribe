# Research: Multi-Machine Collaborative Window Sharing

**Feature**: `015-multi-machine-sharing` | **Date**: 2026-07-22
**Spec**: [spec.md](spec.md) | **Plan**: [plan.md](plan.md)

Phase 0 output. Each design decision D1–D8 from `plan.md` is resolved here in
Decision / Rationale / Alternatives form, grounded in the actual server, protocol,
and client sources. Line anchors are approximate — the named types and functions
are authoritative.

## Revisits — what this feature supersedes from 013/014

This feature is the deliberate reversal of feature 013's single-controller
invariant, now that a shared-view use case exists.

- **Supersedes 013 FR-007** ("exactly one controller per window"). Spec 015
  FR-002 makes joining additive; the single-writer ownership model is replaced by
  a participant set. 013's takeover/reclaim path is retained only as the
  `Single controller` legacy mode.
- **Revisits 013 research D4** ("Takeover mechanics"), which explicitly chose to
  "reuse the existing single-writer ownership invariant (`Arc::ptr_eq` release
  guard) **instead of introducing multi-writer state**" and rejected multi-writer
  as out of scope. Feature 015 is the point where multi-writer state is
  introduced deliberately, behind a mode switch, so 013's default behavior is
  preserved byte-for-byte.
- **Reuses 013 research D5 / PR-004** (per-connection bounded queue, drop +
  resync) verbatim, now applied per participant rather than per controller (see
  D5 below).
- **Reuses 013 research D2/D3 and 014's LAN gates unchanged** — no new access
  path (FR-010). The protocol version handshake from 013 D3 is the vehicle for
  the 2 → 3 bump (D4 below).

## Open micro-decisions resolved here

Recorded so downstream phases do not re-litigate them:

- **Identity of the LOCAL owning client.** The machine that owns a window is
  modeled as a `Participant` whose `ControllerIdentity` is
  `ControllerIdentity::Local` (`crates/scribe-server/src/ipc_server.rs`,
  `ControllerIdentity` ~:157). It is **always implicitly a member** of its own
  windows' shares — it is never "joined" or "ejected", cannot be removed by any
  remote action, and is the fallback control holder and OSC-52 route (FR-007,
  FR-013). A share therefore always has ≥1 participant (the owner). Roster
  entries mark it `is_local = true`.
- **`Hello { takeover: true }` in each mode.** `takeover` keeps its exact 013
  meaning — *exclusive claim*. In every mode it ends any active share and gives
  the claimer sole control, sending every displaced participant
  `WindowTakenOver` (FR-003, the legacy displaced experience). It is never the
  join path for sharing; a normal additive join is `Hello { takeover: false }`.
  In `Single controller` mode `takeover:false` against a connected window still
  yields the 013 outcome (assign a different window locally, or `LostControl`
  remotely) — sharing never starts.
- **Reconnect semantics.** A participant that drops and auto-reconnects rejoins
  as a **viewer** (control unheld from its perspective) — reconnection MUST NOT
  silently seize single-typist control (spec Edge Cases; mirrors 013 research
  D6, which reserves `takeover:true` for explicit user actions). In
  `Collaborative free-for-all` mode every participant is a typist, so a
  reconnect rejoins as a typist — consistent with the mode, not a seizure of an
  exclusive role. Reconnect always re-runs the handshake and gets a fresh
  `SessionReplay`.

---

## D1 — Participant set replaces the single writer

- **Decision**: Consolidate the three per-window maps
  (`ConnectedClients = Arc<RwLock<HashMap<WindowId, SharedWriter>>>` ~:139,
  `WindowControllers = Arc<RwLock<HashMap<WindowId, ControllerIdentity>>>` ~:213,
  `WindowClipboardGating = Arc<RwLock<HashMap<WindowId, bool>>>` ~:148) into a
  single `WindowShare` map — `Arc<RwLock<HashMap<WindowId, WindowShare>>>` — where
  `WindowShare` owns a `Vec`/`HashMap` of `Participant`, a `ControlState`, the
  per-participant clipboard-gating bits, and the authoritative grid. Each
  `Participant` keeps its own `SharedWriter` (`Arc<Mutex<ClientSink>>` ~:119;
  `ClientSink::Local | ClientSink::Remote(RemoteSink)` ~:110). The per-session
  single sink slot `LiveSession.client_writer: ClientWriter`
  (`Arc<Mutex<Option<SharedWriter>>>` ~:124, field at ipc_server.rs:396) becomes a
  set of attached participant sinks, and the `send_to_client` chokepoint (~:6008,
  called by `send_pty_output` ~:6393) becomes a **fan-out** that enqueues to every
  participant's sink — each remote via its existing `RemoteSink` (never awaiting
  the socket), each local inline as today.
- **Rationale**: `send_to_client` currently locks one `ClientWriter` holding at
  most one `SharedWriter`; that single slot is the structural reason a takeover
  freezes the displaced client. Making it a fan-out over a participant set is the
  minimum change that lets N machines receive the same live output. Folding the
  three maps into one entry under one `RwLock` **removes** the tri-lock ordering
  hazard that `WindowOwnership` (~:3942) exists to manage: today
  `resolve_and_register_claim` (~:3985) must take
  `connected → controllers → gating` in a fixed order to avoid deadlock, and the
  doc-comment warns a losing takeover once overwrote the winner's gating bit. One
  `WindowShare` under one lock makes every roster/control/gating mutation a single
  atomic acquisition (the plan's "all per-window share state mutates under a single
  lock acquisition" constraint), eliminating drift by construction rather than by
  discipline.
- **Alternatives considered**:
  - *Keep the three maps, add a fourth participants map* — reintroduces and
    worsens the lock-ordering hazard the 013 code already flagged; four maps to
    keep in lock-step is strictly harder than one. Rejected.
  - *Broadcast channel per window instead of per-participant sinks* — loses the
    per-participant bounded-queue + drop-and-resync isolation that D5 requires; a
    single slow subscriber on a shared channel reintroduces head-of-line blocking.
    Rejected.
  - *Server-side render/diff fan-out (WezTerm model)* — the existing protocol is a
    thin raw-`PtyOutput` proxy with client-side VTE (013 research "State-sync
    model"); switching to server-parsed deltas is a protocol rewrite with no
    payoff for LAN-class links. Rejected.

## D2 — Mode-driven input authorization

- **Decision**: Replace the `Arc::ptr_eq` single-writer guard
  `connection_controls_window` (~:4126) with a mode-aware predicate,
  `connection_may_type(share, participant, mode)`, consulted by
  `requires_window_control` (~:4140) callers before applying a gated message.
  Three arms:
  - `Single controller` → the legacy `Arc::ptr_eq` check against the sole holder
    (byte-identical behavior).
  - `Shared view, single typist` → the participant equals the current
    `ControlState::SingleTypist { holder }`.
  - `Collaborative free-for-all` → any attached participant passes.
  The gated message set is unchanged from 013 — `requires_window_control`
  matches `KeyInput`, `Resize`, `CloseSession`, `CloseWindow`, `FocusChanged`,
  `SearchRequest` — but `Resize` is handled specially in shared modes (see the
  `Resize` carve-out below). In `Shared view, single typist` the
  session-mutating messages `KeyInput`, `CloseSession`, `CloseWindow`,
  `FocusChanged`, and `SearchRequest` follow the control holder (not just
  keystrokes). In free-for-all `KeyInput` is allowed for every attached
  participant, while the lifecycle actions `CloseSession` and `CloseWindow` plus
  `FocusChanged` and `SearchRequest` — which have no control holder to follow in
  that mode — fall to the owning machine (the always-present
  `ControllerIdentity::Local` participant), per spec Assumptions.

  **`Resize` carve-out.** In shared modes `Resize` is **exempt from control
  gating**: it is an informational per-participant viewport report (D3) accepted
  from any attached participant — including viewers — so smallest-wins grid can
  see every viewport. Only in `Single controller` mode does `Resize` retain its
  legacy meaning as a controller-gated direct grid-set.
- **Rationale**: `requires_window_control` already enumerates exactly the
  session-mutating messages; keeping that set and swapping only the predicate is
  the smallest change that satisfies FR-005/FR-006 without touching dispatch. A
  viewer's discarded input (FR-006) is a `connection_may_type == false` result
  that drops the message safely and leaves the client to show the "who holds
  control" affordance — identical drop semantics to today's post-takeover barred
  connection.
- **Alternatives considered**:
  - *Per-message capability tokens* — finer-grained than the two roles the spec
    allows (Assumptions: viewer + holder only), more surface for no v1 value.
    Rejected.
  - *Client-side input suppression only* — a compromised/older client could still
    send input; authorization must be server-side (FR-006, FR-010). Rejected as
    the sole mechanism (clients still suppress for UX, but the server is
    authoritative).

## D3 — Smallest-wins authoritative grid, responsive render

- **Decision**: Every participant reports its terminal viewport by **reusing the
  existing `ClientMessage::Resize { session_id, size: TerminalSize }`** (~:236) —
  no new message. The server, instead of applying the last resize wholesale in
  `handle_resize` (~:5158), stores each participant's reported `size` in its
  `Participant.viewport` and computes the authoritative grid as
  `min(rows)` × `min(cols)` across all attached participants (including the local
  owner's window), then drives the existing `resize_term` +
  `set_pty_winsize`/`TIOCSWINSZ` path once with that minimum. Clients render the
  authoritative grid responsively, centered/padded inside their own (independent
  pixel) window. Regrow on detach: when the smallest participant leaves, the
  min is recomputed and the grid grows back.
  - **Debounce**: coalesce viewport reports over a **250 ms** window before
    applying a grid change, so two near-simultaneous resizes (spec Edge Case) and
    a live drag produce one deterministic `TIOCSWINSZ`, not a flapping stream.
    The min is computed over the debounced snapshot, making the result
    order-independent.
  - **Reconnect-grace interaction**: a participant that is dropping/reconnecting
    (within the D6/013 reconnect grace) is **excluded** from the min while
    absent, so a laggy link cannot pin the whole session to a tiny grid; on
    rejoin its viewport re-enters the min after the same debounce.
- **Rationale**: `TerminalSize` already carries `rows`, `cols`, `cell_width`,
  `cell_height` and `has_grid()`, and `Resize` is already a control-gated,
  per-session message — reusing it means viewport reporting rides the existing
  validated path with no wire addition (D4's additive-only goal). `min` is the
  only choice that guarantees every participant can display the whole grid
  (FR-012); a smaller participant seeing a truncated screen would break shared
  view. Debounce + min-over-snapshot is inherently flap-free because the output
  is a pure function of the current participant set, independent of arrival order.
- **Alternatives considered**:
  - *New dedicated `ViewportReport` message* — duplicates `Resize`'s shape and
    semantics; a viewer's `Resize` already means "this is my grid". Rejected in
    favor of reinterpretation (documented in the v3 contract).
  - *max-of-viewports or owner-fixed grid* — a max forces smaller participants to
    scroll/clip a screen full-screen apps assume is visible; owner-fixed defeats
    the point of letting the laptop drive. Rejected.
  - *Independent per-client grids with server reflow* — the session has one PTY
    and one authoritative `Term`; multiple grids would require per-client
    emulators server-side (mosh/WezTerm territory), contradicting D1's thin-proxy
    reuse. Rejected.

## D4 — Protocol v3 (additive, exact-match negotiation)

- **Decision**: Bump `REMOTE_PROTOCOL_VERSION` from `2` to `3`
  (`crates/scribe-common/src/protocol.rs` ~:21). All new messages are additive
  (`ShareRoster`, `ControlClaim`, `ControlRequest`, `ControlGrant`, and the
  `WindowInfo`/roster field additions — see `contracts/remote-protocol-v3.md`),
  and every new struct field carries `#[serde(default)]` following the existing
  convention (`WindowInfo.controller` ~:150, `Hello.takeover` ~:342,
  `WindowInfo.workspace_names`). Version negotiation stays **exact match** on the
  013 preamble (`RemoteHandshake` ~:409 / `LanHello` ~:438): a v2 (older) remote
  handshaking a v3 server, or the reverse, fails the match and is refused with
  the existing `RemoteRefusal::IncompatibleVersion` (~:1183) /
  `LanRefusal::IncompatibleVersion` (~:1219), both versions named — never a
  half-joined share (FR-014, spec Edge Case).
- **Rationale**: 013 research D3 already chose exact-match versioning precisely so
  a skewed pair gets "a clean refusal path before any window data flows"; sharing
  changes the *meaning* of an accepted session (multi-writer state) enough that a
  v2 peer must not be admitted to it, so a version bump with the existing refusal
  is exactly the intended consequence. serde-default additive fields preserve the
  local Unix-socket path and let a v3 server keep decoding an old on-disk
  `WindowInfo` cleanly.
- **Consequence spelled out**: because negotiation is exact-match, there is **no
  partial-capability mode** — a v2↔v3 pair never shares; it either falls back to
  nothing (refused) per the compatibility matrix in the v3 contract. This is the
  intended, explicit outcome for FR-014, not a limitation to work around.
- **Alternatives considered**:
  - *Capability-negotiated version window* (accept v2 as view-only) — more states
    to test, and a v2 client has no roster/control UI, so it would present a
    broken experience; the spec asks for an explicit outcome, not a degraded one.
    Rejected (consistent with 013 D3's rejection of a semver window).
  - *Feature flag inside `Hello`* — would run after accept on the shared dispatch
    and complicate the local-path guarantees, exactly what 013 D3 rejected.
    Rejected.

## D5 — Fan-out reuses 013's per-connection flow control

- **Decision**: Reuse the 013 bounded-queue machinery **per participant,
  unchanged**: each remote participant keeps its own `RemoteSink` (~:826) over a
  `RemoteOutputShared` queue bounded at `REMOTE_OUTPUT_QUEUE_BYTES = 4 MiB` (~:688)
  for droppable `PtyOutput`, with `REMOTE_OUTPUT_QUEUE_TOTAL_BYTES` (16 MiB) and
  `REMOTE_OUTPUT_QUEUE_MAX_FRAMES` (8192) ceilings. When one participant's link
  stalls, `drop_pty_backlog` sheds its `PtyOutput` backlog, marks its sessions
  replay-dirty, and `send_resync_replay` catches *only that participant* up with a
  fresh `SessionReplay` when the link drains; `enforce_queue_ceiling` closes a
  hopelessly stalled connection. The D1 fan-out enqueues to each participant's
  queue independently and never awaits any socket, so the PTY reader and the
  authoritative `Term` never back-pressure (SC-004, FR-009).
- **Rationale**: This is verbatim 013 research D5 / PR-004, and it already solves
  the exact isolation requirement the spec restates for the multi-participant case
  ("a lagging participant is brought current via an individual resync rather than
  by slowing anyone else down"). Because each participant owns a separate queue
  and drain task, one stalled sink cannot head-of-line-block the others — the
  property is inherited free from the per-connection design.
- **Alternatives considered**:
  - *Shared per-window output buffer* — a single slow participant would gate the
    buffer and re-introduce the WezTerm #2048 flood-stall. Rejected (and would
    undo 013's guarantee).
  - *Unbounded per-participant buffering* — unbounded host memory under a
    persistent stall. Rejected, as in 013 D5.

## D6 — Settings: three live-applied Remote controls

- **Decision**: Add three fields to the remote settings config. The real struct is
  `RemoteConfig` in **`crates/scribe-common/src/config.rs`** (~:1937), NOT
  `scribe-server/src/config.rs` (which only re-exports it via
  `use scribe_common::config::RemoteConfig`). New fields, all
  `#[serde(default)]` so an old config file loads unchanged:
  - `sharing_mode: SharingMode` — `default = single-controller` (legacy).
  - `control_acquisition: ControlAcquisition` — `default = free-claim`.
  - `participant_limit: Option<u32>` (or a sentinel) — `default = None`
    (unlimited).
  They apply live over the existing `ConfigReloaded` → `RemoteControl` reconcile
  path (the same live-apply mechanism `apply_tailnet` ~:1449 / `apply_lan` ~:1505
  use); **no server restart** (plan Operational safety). A mode change takes
  effect immediately on active shares per FR-017 (see data-model state table):
  switching to `Shared view, single typist` demotes all participants to viewers
  with control unheld; switching to `Single controller` detaches remote
  participants with `WindowTakenOver` (legacy displaced notice).
- **Rationale**: Identical shape and live-apply path to every existing remote
  setting (013 D8), so the settings webview and reload plumbing are reused, and
  the default keeps every upgraded server behaving exactly as today until the user
  opts in (FR-014, SC-006).
- **Alternatives considered**:
  - *Per-window mode* — the spec fixes mode as an owner-machine Remote setting
    (FR-004, Assumptions), not per-window; per-window adds UI surface with no
    requirement behind it. Rejected.
  - *Restart-to-apply* — prohibited by repo policy and the plan's operational
    constraint. Rejected.

## D7 — Host-privileged action routing

- **Decision**: Client-initiated privileged actions (paste confirmation, link
  opening) confirm and act on the **acting machine**, per participant, exactly as
  today — this is already client-side behavior and needs no server routing change
  (FR-013). Session-initiated OSC 52 clipboard requests (spec 010) route to the
  **current control holder**; when control is unheld or the mode is
  `Collaborative free-for-all` (no single holder), they fall back to the **owning
  machine** (`ControllerIdentity::Local` participant). The per-participant
  clipboard-gating bit that today lives in `WindowClipboardGating` (~:148) moves
  into each `Participant` under `WindowShare` (D1), so the gating capability is
  tracked per participant rather than per window.
- **Rationale**: Default-safe: an unattended viewer never gets a surprise
  clipboard prompt; the owner is the guaranteed-present fallback (it is always a
  member, per the micro-decision above). Reuses the existing gating bit semantics,
  just relocated to the participant.
- **Alternatives considered**:
  - *Broadcast OSC 52 prompt to all viewers* — leaks session clipboard intent to
    passive watchers and produces N prompts for one event. Rejected.
  - *Route to whoever last typed* — ambiguous in free-for-all and racy. The
    holder-then-owner rule is deterministic. Rejected.

## D8 — Presence and UX (full-state roster broadcast)

- **Decision**: On every membership or control change (join, leave, control
  transfer, ejection, mode change), the server broadcasts a **full-state**
  `ShareRoster { window_id, participants, mode, holder }` to every participant —
  no deltas. Roster entries reuse the `ControllerInfo { device_name, login_name }`
  shape (~:128) plus `is_local`/`is_holder` flags. Clients render a live viewer
  state (output live, input suppressed except the claim/request affordance,
  FR-006); the legacy frozen `LostControlState`
  (`crates/scribe-client/src/lost_control.rs`, held in
  `crates/scribe-client/src/main.rs` `window_taken_over` ~:885, driven by
  `handle_window_taken_over` ~:6860, reclaim via `reclaim_window` ~:7178) remains
  **only** for `Single controller`-mode displacement. Membership changes are
  recorded on the existing 013 remote-audit surface (FR-015, SC-007).
- **Rationale**: Full-state broadcast is idempotent and self-healing — a
  participant that missed a delta during a stall gets the correct roster on the
  next event and on its resync, matching the spec's "rosters on all machines
  reflect a join or leave within 1 second" (SC-005) without delta-reconciliation
  bugs. Reusing `ControllerInfo` keeps the identity surface from drifting (the
  same rationale the type's doc-comment already states).
- **Alternatives considered**:
  - *Incremental delta events* — smaller payloads, but the roster is tiny (2–5
    participants, Assumptions) so full-state costs nothing and removes an entire
    class of ordering/dropped-delta bugs. Rejected.
  - *Poll-on-demand roster* — would miss the "notice appears promptly on all
    machines" requirement (FR-008, SC-005). Rejected.
