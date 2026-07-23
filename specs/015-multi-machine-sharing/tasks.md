---
description: "Task list for feature 015 — Multi-Machine Collaborative Window Sharing"
---

# Tasks: Multi-Machine Collaborative Window Sharing

**Input**: Design documents from `/specs/015-multi-machine-sharing/`

**Prerequisites**: plan.md (D1–D8), spec.md (US1–US4, FR-001..FR-018),
research.md, data-model.md, contracts/remote-protocol-v3.md, quickstart.md

**Tests**: No new test files (repo rule — tests only when explicitly requested;
not requested here). Existing `crates/scribe-server` unit tests that cover the
changed claim/authorization/ownership behavior ARE updated where the
`WindowShare` consolidation breaks them (T009). Every user story ends in a
manual verification task against the numbered `quickstart.md` scenarios
(constitution Principle II).

## Format: `[ID] [P?] [Story] Description`

- **[P]**: can run in parallel — different files, no pending dependency.
- **[Story]**: user-story label (US1–US4); present only in user-story phases.
- Server work concentrates in `crates/scribe-server/src/ipc_server.rs`, so most
  same-file server tasks are sequential; client (`scribe-client`) and
  settings-webview (`scribe-settings`) tasks run parallel to server work.

---

## Phase 1: Setup (Shared Infrastructure)

**Purpose**: Establish a known-good baseline and confirm the edit anchors before
the structural consolidation.

- [x] T001 Record the green baseline: run `cargo build` and `cargo test -p scribe-server` and note that `claim_window_rejects_already_claimed_window`, `concurrent_claims_for_same_window_never_collide`, and `stale_detach_does_not_evict_new_owner` in `crates/scribe-server/src/ipc_server.rs` currently pass, so the WindowShare consolidation's test fallout (T009) is visible as a diff, not a mystery.
- [x] T002 Confirm the current line anchors named across the design docs before editing: `resolve_and_register_claim` (~3982), `connection_controls_window` (~4126), `requires_window_control` (~4140), `handle_resize` (~5158), `send_to_client` (~6008), `send_pty_output` (~6393), `drop_pty_backlog` (~967), `send_resync_replay` (~1099), `REMOTE_AUDIT_TARGET` (748) in `crates/scribe-server/src/ipc_server.rs`, `REMOTE_PROTOCOL_VERSION` (21) in `crates/scribe-common/src/protocol.rs`, and `RemoteConfig` (~1937) in `crates/scribe-common/src/config.rs`.

---

## Phase 2: Foundational (Blocking Prerequisites)

**Purpose**: The protocol/config/state substrate every user story builds on: v3
messages, settings fields, the `WindowShare` consolidation, the fan-out sink, and
mode-aware authorization. **No user story can begin until this phase completes.**

**⚠️ CRITICAL**: These tasks change shared types and the server's per-window state
model; all stories depend on them.

- [x] T003 [P] Add `SharingMode` (`SingleController | SharedSingleTypist | FreeForAll`) and `ControlAcquisition` (`FreeClaim | RequestAndGrant`) enums and extend `RemoteConfig` with `sharing_mode` (default `SingleController`), `control_acquisition` (default `FreeClaim`), and `participant_limit: Option<u32>` (default `None`), every field `#[serde(default)]` so an existing config file loads with legacy behavior, in `crates/scribe-common/src/config.rs` (per D6, data-model SharingSettings; FR-004/FR-005/FR-014/FR-018).
- [x] T004 [P] Bump `REMOTE_PROTOCOL_VERSION` 2 → 3 and add the v3 `ClientMessage` variants `ControlClaim { window_id }`, `ControlRequest { window_id }`, and `ControlGrant { window_id, participant_id, accept }` in `crates/scribe-common/src/protocol.rs` (contracts/remote-protocol-v3.md; D4).
- [x] T005 Add the v3 `ServerMessage` variants `ShareRoster { window_id, participants: Vec<ParticipantInfo>, mode: SharingMode, holder: Option<ParticipantId> }`, `ControlRequested { window_id, from: ParticipantInfo }`, `ControlDenied { window_id }`, and `ShareEnded { window_id, reason: ShareEndReason }`; define `ParticipantInfo { participant_id, device_name, login_name, is_local, is_holder }` (reusing the `ControllerInfo` device/login pair, ~128) and `ShareEndReason` (`OwnerClosed | WindowClosed | ModeChangedToSingleController`); add `#[serde(default)]` fields `participants: Vec<ControllerInfo>`, `mode: Option<SharingMode>`, `participant_count` to `WindowInfo` (~136), retaining `controller` and `WindowTakenOver` unchanged, in `crates/scribe-common/src/protocol.rs` (contracts; D4/D8). Depends on T003 (SharingMode), T004 (same file).
- [x] T006 Define the share-registry types in `crates/scribe-server/src/ipc_server.rs`: `ParticipantId` (`u64`, server-monotonic per share registry), `Participant { id, writer: SharedWriter, identity: ControllerIdentity, transport, viewport: TerminalSize, clipboard_gating: bool, joined_at }`, `ControlState { LegacyExclusive { writer } | SingleTypist { holder: Option<ParticipantId>, pending_request: Option<PendingRequest> } | FreeForAll }`, `AuthoritativeGrid { rows, cols, debounce }`, and `WindowShare { participants, control, grid, mode, control_acquisition, participant_limit }`; add `window_shares: Arc<RwLock<HashMap<WindowId, WindowShare>>>` to the server state and retire the three per-window maps `ConnectedClients` (~139), `WindowControllers` (~213), `WindowClipboardGating` (~148) and their `WindowOwnership` tri-lock (~3942) (D1, data-model WindowShare/Participant/ControlState). Depends on T003, T005.
- [x] T007 Convert the single sink slot `LiveSession.client_writer` (~124/396) into the set of attached `Participant` sinks and rewrite `send_to_client` (~6008), called from `send_pty_output` (~6393), into a non-blocking fan-out that enqueues to every participant's sink — each remote via its existing `RemoteSink` (never awaiting the socket), the local owner inline — in `crates/scribe-server/src/ipc_server.rs` (D1). Depends on T006.
- [x] T008 Replace the `Arc::ptr_eq` single-writer guard `connection_controls_window` (~4126) with `connection_may_type(share, participant, mode)` and route `requires_window_control` (~4140) callers through it: `SingleController` → legacy `Arc::ptr_eq` against the sole holder; `SharedSingleTypist` → participant equals `ControlState::SingleTypist.holder`; `FreeForAll` → interim, admit `KeyInput` from any attached participant only (lifecycle/focus/search authorization lands in T029); a `false` result drops the message safely (FR-006). `Resize` is **exempt from control gating in shared modes** — it is an informational viewport report accepted from any attached participant (D3, consumed by T014) and stays controller-gated only in `SingleController` (legacy grid-set). Keep the remaining gated set (`KeyInput`, `CloseSession`, `CloseWindow`, `FocusChanged`, `SearchRequest`) following the holder in `SharedSingleTypist`, in `crates/scribe-server/src/ipc_server.rs` (D2). Depends on T006.
- [x] T009 Update the existing server unit tests that the WindowShare consolidation breaks (they reference the retired `ConnectedClients`/`claim_window`/`release_window_if_owned` path): rework `claim_window_rejects_already_claimed_window` (~8031), `concurrent_claims_for_same_window_never_collide` (~8055), and `stale_detach_does_not_evict_new_owner` (~8087) to exercise `window_shares`/`Participant` registration and the preserved `Arc::ptr_eq` participant-identity invariant instead, in `crates/scribe-server/src/ipc_server.rs`. Depends on T006, T007, T008.

**Checkpoint**: Protocol v3, settings, the `WindowShare` state model, the fan-out
sink, and mode-aware authorization are in place and the suite compiles/passes —
user stories can now proceed.

---

## Phase 3: User Story 1 - Watch a window live from a second machine (Priority: P1) 🎯 MVP

**Goal**: A second machine joins an in-use window additively and sees the live
terminal; the first machine is never frozen, dimmed, or disconnected. Shared
live view alone (input still held by one machine) is the shippable MVP.

**Independent Test**: Attach a second machine to an in-use window in a shared
mode, stream continuous output on the first machine, and verify both render it
live while the first keeps typing uninterrupted (quickstart Scenario 1).

- [x] T010 [US1] Make the join path additive in `resolve_and_register_claim` (~3982): a `Hello { takeover: false }` against a window whose `mode` permits sharing registers a new `Participant` in that window's `WindowShare` with no writer swap and no disturbance to existing participants (FR-002); `SingleController` mode retains the 013 assign-different-window / `LostControl` outcome, and `Hello { takeover: true }` keeps its exact 013 exclusive-claim semantics. When `takeover: true` lands against an **active `SharedSingleTypist` or `FreeForAll` share**, the exclusive claim ends that share for **every** attached participant, not just a sole writer: each currently-attached remote participant is detached and sent `WindowTakenOver` (the legacy displaced notice) so nobody is silently left half-attached, and the claimer becomes the sole controller (FR-003, spec Edge Case). Implement in `crates/scribe-server/src/ipc_server.rs` (data-model participant lifecycle + control-transition tables; research micro-decisions). Additive registration MUST sit behind the existing remote trust gate — the same-user tailnet WhoIs (013) and explicit LAN device approval (014) that already guard `resolve_and_register_claim`; a `Participant` is constructed only after that gate passes, so sharing adds no new access path (FR-010). Depends on Phase 2.
- [x] T011 [US1] Enforce the participant limit (FR-018) at the join in `crates/scribe-server/src/ipc_server.rs`: before additive registration, if `WindowShare.participant_limit` is `Some(n)` and the remote participant count is already `n`, refuse the join with the existing `RemoteRefusal::Busy` (~1191) / `LanRefusal::Busy` (~1229) and leave the active share undisturbed; the local owner participant is exempt from the count (FR-007). Depends on T010. (Enforcement lives here because the join path is built in US1; the `participant_limit` snapshot field comes from T006.)
- [x] T012 [US1] Fan out live output to every participant in `crates/scribe-server/src/ipc_server.rs`: send a fresh `SessionReplay` to the joining participant only, and route `PtyOutput` through the T007 fan-out to all attached participant sinks so A, B, … all render the same live stream. Depends on T007, T010.
- [x] T013 [US1] Wire per-participant resync-on-overflow in `crates/scribe-server/src/ipc_server.rs`: apply the existing `drop_pty_backlog` (~967) + `send_resync_replay` (~1099) machinery per `Participant`'s `RemoteSink` so a stalled participant sheds its own `PtyOutput` backlog and catches up alone, with zero back-pressure on the PTY reader or other participants (FR-009, SC-004; D5). Depends on T012.
- [x] T014 [US1] Implement the smallest-wins `AuthoritativeGrid` in `crates/scribe-server/src/ipc_server.rs`: reinterpret each participant's `Resize { session_id, size }` (~236) as a viewport report stored in `Participant.viewport`, compute `min(rows) × min(cols)` across attached non-reconnecting participants over a 250 ms debounce window, and drive the existing `resize_term` + `set_pty_winsize`/`TIOCSWINSZ` path in `handle_resize` (~5158) once per settled change; regrow when the smallest participant detaches (FR-012; D3). In shared modes the `Resize` viewport report is ungated (T008), so a viewer's report reaches this path and can shrink the grid; `SingleController` retains the legacy holder-gated direct grid-set. Depends on T006, T008.
- [x] T015 [P] [US1] In `crates/scribe-client/src/main.rs`, add the live-viewer state: when attached to a shared window as a viewer, render live output with local input suppressed (no frozen `LostControlState`, which stays reserved for `SingleController` displacement), keep the local view live on join, render the authoritative grid responsively (centered/padded inside the client's own independently-sized window), and report the client viewport via `Resize` on window resize (FR-002/FR-012; D3/D8).
- [ ] T016 [US1] Manual verification per `specs/015-multi-machine-sharing/quickstart.md`: Scenario 1 (shared live view within 2 s, no interruption — SC-001), Scenario 2 (stalled-participant isolation + single resync — SC-004/FR-009), and Scenario 6 (smallest-wins grid + regrow + no-flapping — FR-012).

**Checkpoint**: US1 is independently functional — MVP shippable (shared live view
with control still held by one machine).

---

## Phase 4: User Story 2 - Pass input control without disconnecting anyone (Priority: P2)

**Goal**: In `SharedSingleTypist` mode, input control passes between attached
machines without disconnecting anyone; the previous holder stays a live viewer.

**Independent Test**: With two machines attached, transfer control both ways and
verify the new control holder's keystrokes reach the session while the previous
holder keeps a live view (quickstart Scenario 3).

- [x] T017 [US2] Handle `ControlClaim` / `ControlRequest` / `ControlGrant` in `crates/scribe-server/src/ipc_server.rs`, driving the `ControlState::SingleTypist` transitions: a claim sets `holder` and demotes the previous holder to a still-live viewer; a request records `pending_request` and sends `ControlRequested` to the current holder (or the owner when unheld); a grant with `accept: true` transfers `holder`, with `accept: false` clears the request and sends `ControlDenied`; the owning machine may always claim regardless of setting (FR-005/FR-007; data-model control transitions). Depends on Phase 2.
- [x] T018 [US2] Apply the `control_acquisition` setting in `crates/scribe-server/src/ipc_server.rs`: under `FreeClaim`, `ControlClaim` takes control instantly; under `RequestAndGrant`, a non-owner viewer must use `ControlRequest` and be approved via `ControlGrant`, with only the named approver (holder, or owner when unheld) honored (FR-005). Depends on T017.
- [x] T019 [US2] Implement holder-loss transitions (FR-016) in `crates/scribe-server/src/ipc_server.rs`: when the `SingleTypist` holder detaches or is ejected, set `holder = None` (no silent inheritance), clear any `pending_request`, and broadcast the roster so remaining participants (incl. the owner) can claim. Depends on T017.
- [x] T020 [P] [US2] In `crates/scribe-client/src/main.rs`, add the claim/request affordances: while a viewer's input is suppressed, show who holds control and how to obtain it (FR-006), bind the claim/request action to emit `ControlClaim` / `ControlRequest`, and handle incoming `ControlRequested` (holder/owner prompt) and `ControlDenied` (requester notice). Depends on nothing in this phase structurally (parallel to server T017–T019).
- [ ] T021 [US2] Manual verification per `specs/015-multi-machine-sharing/quickstart.md` Scenario 3: free-claim pass both ways (SC-003, previous holder stays a live viewer, viewer input discarded with a "who holds control" hint — FR-005/FR-006), the request-and-grant variant (grant/deny, owner-always-claim), the holder-loss variant (control becomes unheld, claimable — FR-016), and the three-machine control-holder echo-latency measurement (holder typing adds ≤5 ms over the single-machine baseline — SC-002).

**Checkpoint**: US1 and US2 both work independently — shared view plus fluid
control hand-off.

---

## Phase 5: User Story 3 - See who is attached and who is in control (Priority: P3)

**Goal**: Every participant, and always the owning machine, can see the roster
(attached devices/accounts + current holder); joins, leaves, and control changes
are announced to all.

**Independent Test**: With three machines attached, verify each shows the same
roster, notices appear promptly on all, and the owner can always enumerate every
attached device (quickstart Scenario 4).

- [x] T022 [US3] Wire the full-state `ShareRoster { window_id, participants, mode, holder }` broadcast in `crates/scribe-server/src/ipc_server.rs`, emitted to every participant on each join, leave, control transfer, ejection, and mode change (no deltas); build each `ParticipantInfo` with `is_local`/`is_holder` (FR-008, SC-005; D8). US3 is independently deliverable on the membership-change triggers (join, leave, ejection), which depend only on Phase 2; the control-transfer and mode-change trigger points arrive with US2 (T017/T019) and are wired at those tasks where present. Depends on Phase 2.
- [x] T023 [US3] Record membership audit events (FR-015) in `crates/scribe-server/src/ipc_server.rs` on the existing 013 remote-audit surface (`REMOTE_AUDIT_TARGET`, 748): join, leave, control transfer, ejection, and share-end — 100% of membership changes (SC-007). Depends on T022.
- [x] T024 [P] [US3] In `crates/scribe-client/src/main.rs`, render the presence badge / status-bar roster from `ShareRoster`: list attached devices/accounts, mark the current control holder, and surface join / leave / control-change notices as they arrive (FR-008).
- [x] T025 [P] [US3] In `crates/scribe-client/src/remote_connect.rs`, change the connect-picker to show share occupancy ("N attached") from `WindowInfo.participants` / `participant_count` instead of the 013 binary in-use flag.
- [x] T026 [US3] In `crates/scribe-client/src/main.rs`, give the owning machine roster visibility: the owner (always a share member) can enumerate every attached device/account for its own window without entering any remote flow (FR-008, US3 scenario 3). Depends on T024 (same file).
- [ ] T027 [US3] Manual verification per `specs/015-multi-machine-sharing/quickstart.md` Scenario 4 (three-machine roster parity, join/leave/control notices within 1 s, owner enumeration — SC-005) and the audit spot-check (every membership change present — SC-007/FR-015).

**Checkpoint**: US1–US3 all independently functional — sharing is now
trustworthy (presence + audit).

---

## Phase 6: User Story 4 - Type together in one terminal (Priority: P4)

**Goal**: In `FreeForAll` mode, every attached participant types into the same
shell (`screen -x` style); input interleaves by arrival and all see the identical
screen.

**Independent Test**: With free-for-all enabled, type from two machines in quick
alternation and verify both streams reach the session in arrival order and every
participant sees the identical screen (quickstart Scenario 5).

- [x] T028 [US4] Implement the free-for-all input delivery path in `crates/scribe-server/src/ipc_server.rs`: relying on the T008 `connection_may_type` `FreeForAll` arm (which admits `KeyInput` from every attached participant), deliver those keystrokes to the PTY interleaved in arrival order and echo to all via the T012 fan-out (FR-004, US4; D2). Depends on T008.
- [x] T029 [US4] Add the per-message authorization split in `connection_may_type` / `requires_window_control` in `crates/scribe-server/src/ipc_server.rs`: in `FreeForAll`, `KeyInput` is allowed for all and `Resize` viewport reports are ungated (T008); the lifecycle actions `CloseSession` and `CloseWindow` plus `FocusChanged` and `SearchRequest` have no control holder to follow in this mode and are authorized for the owning machine only (the always-present `ControllerIdentity::Local` participant), per spec Assumptions (D2). Depends on T028, T008.
- [x] T030 [US4] Implement OSC 52 owner fallback (FR-013/D7) in `crates/scribe-server/src/ipc_server.rs`: route session-initiated clipboard requests to the current control holder, falling back to the owning machine (`ControllerIdentity::Local` participant) when control is unheld or the mode is `FreeForAll`, using the per-participant `clipboard_gating` bit now on `Participant`; client-initiated paste/link confirmations remain on the acting machine (unchanged, per participant). Depends on T006.
- [ ] T031 [US4] Manual verification per `specs/015-multi-machine-sharing/quickstart.md` Scenario 5 (interleaved free-for-all typing, identical screen, three-machine parity — SC-002/FR-004) and Scenario 10 (host-privileged action routing: paste/link on the acting machine, OSC 52 to holder with owner fallback — FR-013).

**Checkpoint**: All four user stories independently functional.

---

## Phase 7: Polish & Cross-Cutting Concerns

**Purpose**: Settings surface, mode-change safety, revoke ejection, legacy-parity
verification, and the required lat.md sync.

- [x] T032 Implement immediate mode-change application (FR-017) in `crates/scribe-server/src/ipc_server.rs`, applied over the existing `ConfigReloaded` reconcile path (no restart): switching to `SharedSingleTypist` demotes all participants to viewers with `control = SingleTypist { holder: None, pending_request: None }`; switching to `SingleController` detaches remote participants with `WindowTakenOver` + `ShareEnded { reason: ModeChangedToSingleController }`; switching to `FreeForAll` makes all participants typists; any pending `ControlRequest` is cancelled and the requester informed; broadcast the roster in every case (data-model mode-change table; D6). Depends on T017, T019, T022 (demote/detach/broadcast primitives from US2/US3).
- [x] T033 Implement per-device revoke ejection (FR-011) in `crates/scribe-server/src/ipc_server.rs`: when a device is revoked or its transport severed, remove only the affected `Participant`(s) from the `WindowShare`, notify them (`ShareEnded` / roster), and leave the share running for everyone else; if an ejected participant held control, apply the T019 holder-loss transition. Depends on T019, T022.
- [x] T034 [P] Add the three Remote-settings controls to the settings webview in `crates/scribe-settings/src/assets/settings.html` and the wiring in `crates/scribe-settings/src/{lib.rs,server_action.rs}`: `sharing_mode` (Single controller / Shared view, single typist / Collaborative free-for-all), `control_acquisition` (Free claim / Request and grant), and `participant_limit` (unlimited or a number), persisted to `RemoteConfig` and applied live via `ConfigReloaded` with no server restart (FR-004/FR-005/FR-018; D6). Matches nearby Remote settings language/hierarchy (constitution Principle III).
- [ ] T035 Manual verification per `specs/015-multi-machine-sharing/quickstart.md` Scenario 7 (mode change applies immediately: demote to viewers / detach with legacy notice / pending request cancelled — FR-017), Scenario 8 (participant-limit `Busy` refusal, owner exempt, active share undisturbed — FR-018), and Scenario 11 (trust-gate refusal on join — an unapproved/revoked device can never join a share, FR-010 — plus revoke mid-share: only the revoked participant is ejected, the share continues, and the holder-loss transition applies if it held control — FR-011/FR-016).
- [ ] T036 Manual verification per `specs/015-multi-machine-sharing/quickstart.md` Scenario 9 (legacy parity: `SingleController` default behaves identically to the previous release — local use, remote attach, takeover, reclaim — SC-006; a v2 remote against a v3 server gets an `IncompatibleVersion` refusal naming both versions, never a half-join — FR-014; and exclusive takeover against an active `SharedSingleTypist`/`FreeForAll` share detaches and notifies **every** attached participant with `WindowTakenOver`, claimer becomes sole controller — FR-003, per T010).
- [x] T037 [P] Update `lat.md/server.md` "## Remote Control" (:437) to document the `WindowShare` consolidation (replacing the three per-window maps + tri-lock), the participant fan-out sink, mode-aware `connection_may_type` authorization, the smallest-wins `AuthoritativeGrid`, roster broadcast, per-device revoke ejection, and mode-change application; add `[[src/...]]` links to the new types/functions.
- [x] T038 [P] Update `lat.md/protocol.md` "## Remote Protocol" (:193) for the v2 → v3 bump and the additive messages (`ControlClaim`/`ControlRequest`/`ControlGrant`, `ShareRoster`/`ControlRequested`/`ControlDenied`/`ShareEnded`, `WindowInfo` sharing fields, `Resize` reinterpreted as a viewport report) and the exact-match negotiation / `IncompatibleVersion` outcome.
- [x] T039 [P] Update `lat.md/client.md` "## Remote Control" (:364) and "### Remote Control Surfaces" (:636) for the live-viewer state (input suppressed, no frozen banner in shared modes), the roster/presence badge, the claim/request affordances, and the connect-picker share-occupancy display.
- [x] T040 Run `lat check` and fix any failing wiki links or code refs across the updated `lat.md/` sections (constitution Engineering Constraint; project post-task checklist). Depends on T037, T038, T039.

---

## Dependencies & Execution Order

### Phase dependencies

- **Setup (Phase 1)**: no dependencies — start immediately.
- **Foundational (Phase 2)**: depends on Setup — **blocks all user stories**. T003/T004 are parallel (different files); T005 depends on T003+T004; T006 depends on T003+T005; T007/T008 depend on T006; T009 depends on T006+T007+T008.
- **US1 (Phase 3)**: depends on Foundational. This is the MVP.
- **US2 (Phase 4)**: depends on Foundational; independently testable. Does not require US1 code but is demoed on top of it.
- **US3 (Phase 5)**: depends on Foundational; its control-change roster/audit triggers (T022) reference US2's transition points where present.
- **US4 (Phase 6)**: depends on Foundational (T008 authorization predicate); independently testable.
- **Polish (Phase 7)**: T032 depends on US2 (T017/T019) + US3 (T022) primitives; T033 depends on T019+T022; T034 depends on T003; T035/T036 are verification; T037–T039 documentation (parallel); T040 depends on T037–T039.

### Story completion order

Priority order P1 → P2 → P3 → P4 (US1 → US2 → US3 → US4). Each story is
independently shippable after Foundational; later stories layer on without
breaking earlier ones.

### Which phases block which

- Phase 1 → Phase 2 → (Phase 3 | Phase 4 | Phase 5 | Phase 6) → Phase 7.
- Phase 2 is the sole hard gate for all four stories. Phase 7's T032/T033 are the
  only cross-story-dependent items (they need US2/US3 transition + broadcast
  primitives); the rest of Phase 7 gates only on its own inputs.

### Within each user story

- Server state/predicate tasks precede the tasks that consume them (T017 before
  T018/T019; T028 before T029).
- Client tasks (`scribe-client`) run parallel to same-story server tasks.
- The manual verification task runs last in its story (after that story's
  implementation tasks).

---

## Parallel Execution Examples

**Foundational (Phase 2)** — different files, no pending deps:

```bash
Task: "T003 SharingMode/ControlAcquisition + RemoteConfig fields in crates/scribe-common/src/config.rs"
Task: "T004 REMOTE_PROTOCOL_VERSION bump + ControlClaim/ControlRequest/ControlGrant in crates/scribe-common/src/protocol.rs"
# T005 waits for both (uses SharingMode, same file as T004); T006+ wait for T005.
```

**User Story 1** — client task parallel to server work:

```bash
# Server (sequential, all in ipc_server.rs): T010 → T011 → T012 → T013; T014 in parallel branch.
Task: "T015 [US1] live-viewer state + responsive centering + viewport reporting in crates/scribe-client/src/main.rs"
```

**User Story 2** — client affordances parallel to server control-state work:

```bash
# Server: T017 → T018 / T019 in ipc_server.rs.
Task: "T020 [US2] claim/request affordances + ControlRequested/ControlDenied handling in crates/scribe-client/src/main.rs"
```

**User Story 3** — two client files in parallel with server broadcast/audit:

```bash
Task: "T024 [US3] presence badge/status-bar roster in crates/scribe-client/src/main.rs"
Task: "T025 [US3] connect-picker share occupancy in crates/scribe-client/src/remote_connect.rs"
# Server: T022 → T023 in ipc_server.rs.
```

**Polish** — the three lat.md updates run in parallel, then `lat check`:

```bash
Task: "T037 lat.md/server.md Remote Control"
Task: "T038 lat.md/protocol.md Remote Protocol"
Task: "T039 lat.md/client.md Remote Control"
# T040 lat check runs after all three.
```

---

## Implementation Strategy

### MVP first (Phase 1 + Phase 2 + US1)

1. Complete Phase 1 (Setup) — baseline green, anchors confirmed.
2. Complete Phase 2 (Foundational) — protocol v3, settings, `WindowShare`,
   fan-out sink, mode-aware authorization. **CRITICAL — blocks all stories.**
3. Complete Phase 3 (US1) — additive join, output fan-out, per-participant
   resync, smallest-wins grid, client live-viewer.
4. **STOP and VALIDATE**: run T016 (quickstart Scenarios 1, 2, 6). Shared live
   view — even with control still held by one machine — is a complete, shippable
   MVP answering the original "only one machine at a time" complaint.

### Incremental delivery

1. Setup + Foundational → substrate ready.
2. US1 → verify (T016) → ship MVP (shared live view).
3. US2 → verify (T021) → ship control hand-off.
4. US3 → verify (T027) → ship presence + audit.
5. US4 → verify (T031) → ship collaborative typing.
6. Polish → settings UI, mode-change/limit/revoke safety, legacy-parity
   verification (T035/T036), lat.md sync + `lat check` (T037–T040).

### Checkpoints

- After US1 (T016): MVP demo — second machine watches live, first uninterrupted.
- After US2 (T021): control passes both ways, no one disconnected.
- After US3 (T027): three-machine roster parity + audit trail.
- After US4 (T031): interleaved free-for-all typing.
- After Polish (T040): settings live-applied, legacy flows identical, docs synced.

---

## Notes

- [P] = different files, no pending dependency; most server tasks share
  `ipc_server.rs` and are therefore sequential.
- No new test files (repo rule); T009 updates the existing server tests broken by
  the `WindowShare` consolidation, and each story ends in a manual quickstart
  verification task (constitution Principle II).
- Legacy behavior is preserved by construction: `sharing_mode` defaults to
  `SingleController` and every new config/protocol field is `#[serde(default)]`,
  so an upgraded server behaves identically until the user opts in (FR-014,
  SC-006).
- Do not restart the Scribe server; all settings apply live via `ConfigReloaded`.
