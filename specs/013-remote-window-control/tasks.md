# Tasks: Remote Window Control over Tailscale

**Input**: Design documents from `/specs/013-remote-window-control/`

**Prerequisites**: plan.md, spec.md, research.md (D1–D10), data-model.md, contracts/remote-protocol.md, contracts/settings-and-config.md, quickstart.md

**Tests**: Not requested — no automated test tasks (research D10, Constitution II). Each story ends with a mandatory manual verification task tied to its quickstart scenario. All verification runs against **dev-flavor instances only; the production server is never restarted**.

**Organization**: Tasks are grouped by user story to enable independent implementation and testing of each story.

## Format: `[ID] [P?] [Story] Description`

- **[P]**: Can run in parallel (different files, no dependencies)
- **[Story]**: Which user story this task belongs to (US1–US4)
- Include exact file paths in descriptions

## Path Conventions

Multi-crate Rust workspace (plan.md Structure Decision): all paths are
repo-relative under `crates/`.

---

## Phase 1: Setup (Shared Infrastructure)

**Purpose**: Wire-level and config-level types every later task builds on

- [X] T001 Add `RemoteConfig` types for the `[remote]` TOML table (`enabled: bool = false`, `port: u16 = 46061`, serde defaults, missing-table ⇒ defaults) in crates/scribe-common/src/config.rs and thread them into server config load in crates/scribe-server/src/config.rs per contracts/settings-and-config.md
- [X] T002 [P] Add `REMOTE_PROTOCOL_VERSION: u32 = 1`, `ClientMessage::RemoteHandshake`, `ServerMessage::RemoteHandshakeReply`, `RemoteRefusal` enum (the single canonical refusal taxonomy), `ServerMessage::WindowTakenOver`, `ServerMessage::RemoteDisconnect`, `ClientMessage::ListRemotePeers`/`ServerMessage::RemotePeerList`, and `#[serde(default)] takeover: bool` on `Hello` in crates/scribe-common/src/protocol.rs per contracts/remote-protocol.md
- [X] T003 [P] Verify `serde_json` is available to scribe-server for LocalAPI response parsing; add the dependency in crates/scribe-server/Cargo.toml only if absent

---

## Phase 2: Foundational (Blocking Prerequisites)

**Purpose**: Tailnet identity + transport spine — every user story rides on this

**⚠️ CRITICAL**: No user story work can begin until this phase is complete

- [X] T004 Create minimal Tailscale LocalAPI client (hand-rolled HTTP/1.1 over the tailscaled Unix socket, Linux path `/var/run/tailscale/tailscaled.sock`): `GET /localapi/v0/status` (self identity, tailnet addresses, peer list) and `GET /localapi/v0/whois?addr=ip:port`, with specific error types, in new file crates/scribe-server/src/tailnet.rs (research D2)
- [X] T005 Add macOS LocalAPI variant (sameuserproof discovery of the sandboxed daemon's localhost TCP port + password, `cfg(target_os = "macos")`) in crates/scribe-server/src/tailnet.rs
- [X] T006 Implement identity policy + bind-address enumeration in crates/scribe-server/src/tailnet.rs: authorize iff `peer.user_id == self.user_id`, refuse tagged/identity-less peers (audit `detail=tagged`), fail closed on ANY LocalAPI error; expose the machine's tailnet IPs for listener binding (data-model.md TailnetIdentity rules)
- [X] T007 Add remote TCP listener lifecycle to crates/scribe-server/src/ipc_server.rs: created only while `remote.enabled`, bound ONLY to tailnet addresses from T006 (never 0.0.0.0), started/stopped/rebound live via the existing `ConfigReloaded` path in crates/scribe-server/src/config.rs handling — no server restart anywhere (research D8)
- [X] T008 Implement remote accept path in crates/scribe-server/src/ipc_server.rs per the contracts/remote-protocol.md sequence — preamble FIRST so refusals are typed: accept → read `RemoteHandshake` (bounded; bare-close only on malformed/non-preamble first frame) → WhoIs → authorize → exact `REMOTE_PROTOCOL_VERSION` gate → always answer `RemoteHandshakeReply` (typed refusals incl. `Busy` at the 8-remote-connection cap and `Disabled` on the disable race) → hand accepted connections to the existing per-connection dispatch; emit an audit log line for every outcome
- [X] T009 [P] Add remote dial support to crates/scribe-client/src/ipc_client.rs: TCP connect to `host:port`, send `RemoteHandshake`, map `RemoteHandshakeReply` refusals, `RemoteDisconnect` sever notices, and connect/EOF errors into typed outcomes for the UI layer — including the combined connection-failure outcome (offline / not running / disabled, per spec FR-004)
- [ ] T010 Foundational checkpoint (manual, quickstart Prerequisites + Scenario 2 steps 1–2): on a dev instance with `[remote] enabled = true` hand-edited into TOML, confirm tailnet-only bind via `ss -tlnp`, a same-account raw connect completes the handshake, a wrong first frame closes, and disabling live-closes the listener

**Checkpoint**: Foundation ready — user story implementation can now begin

---

## Phase 3: User Story 1 - Take control of a window from another machine (Priority: P1) 🎯 MVP

**Goal**: From machine B, pick machine A, see its windows, attach (or create a window), and control it with full fidelity — including taking over a window that is open locally on A, with all host-action gates routed to the controlling machine.

**Independent Test**: Quickstart Scenario 1 end-to-end on two dev instances. Until US2 lands, enable remote access by hand-editing the `[remote]` table (T010 method) — the settings toggle is US2 surface, not a US1 dependency.

### Implementation for User Story 1

- [X] T011 [US1] Extend window listing with picker/indicator context (workspace names, session counts, and current controller identity — device + account — when a window is remote-controlled) as serde-additive fields on the existing window-list reply in crates/scribe-common/src/protocol.rs, populated in crates/scribe-server/src/ipc_server.rs (FR-005, feeds FR-009b/SC-006 surfaces)
- [X] T012 [US1] Implement takeover claim in crates/scribe-server/src/ipc_server.rs (`resolve_window_assignment`/`claim_window` path): when `Hello.takeover = true` targets a connected window, atomically swap the writer under the `connected_clients` lock, re-bind ALL per-connection state — including the `clipboard_gating` capability bit and clipboard-bridge routing — to the new controller's `Hello`, send `WindowTakenOver { device, account }` to the displaced writer, and continue the normal attach/replay flow (research D4; FR-014 capability rule); also implement the remote non-takeover claim of a connected window (`Welcome` + immediate `WindowTakenOver`, no attach, no silent reassignment) per contracts/remote-protocol.md; local-socket default-false path stays byte-identical to today; coordinate replay sequencing with crates/scribe-server/src/attach_flow.rs
- [X] T013 [US1] Implement `ListRemotePeers` handler (local-Unix-socket-only; refused over TCP) returning same-account online peers from tailnet.rs status, in crates/scribe-server/src/ipc_server.rs per contracts/remote-protocol.md Local helper messages
- [X] T014 [US1] Create connect flow in new file crates/scribe-client/src/remote_connect.rs: command-palette action "Connect to remote machine…", GPU-overlay picker (peers → windows → attach / "New window"), manual host entry, distinct failure copy per the UX-002 table in contracts/settings-and-config.md (including the combined connection-failure wording)
- [X] T015 [US1] Wire remote-attached windows through the existing session plumbing in crates/scribe-client/src/ipc_client.rs and the pane layer: replay restore, `PtyOutput`, `KeyInput` 4 KiB chunking, `Resize`-before-input ordering, scroll/search — all over the TCP writer (FR-006, FR-008)
- [X] T016 [US1] Route host-action gates to the controlling machine over the remote link (FR-014): confirm/adjust clipboard-bridge and `ClipboardPromptRequest/Response` routing in crates/scribe-server/src/ipc_server.rs + crates/scribe-server/src/clipboard_state.rs so OSC 52 policy (prompts, size caps, headless-deny when the controller lacks the capability) targets the CURRENT controller's clipboard — including immediately after a takeover swap (pairs with T012's re-bind); verify paste-confirmation and OSC 8 link-opening already evaluate on the controlling client in crates/scribe-client and document the trace in the task notes
- [X] T017 [P] [US1] Implement displaced-client state in crates/scribe-client (pane/render + input layers): on `WindowTakenOver`, suppress input for that window, render the last frame dimmed and frozen, show the banner "Controlled by <device> (<account>)" with a one-action reclaim that reconnects with `Hello { takeover: true }` (FR-007, FR-009b, clarification: dimmed frozen view)
- [X] T018 [US1] Implement remote new-window in crates/scribe-client/src/remote_connect.rs (+ claim handling already in crates/scribe-server/src/ipc_server.rs): picker "New window" sends `Hello { window_id: None }` over the remote link and enters the fresh window; confirm workspace-tree reporting works remotely (US1 scenario 6, clarification: attach + create)
- [ ] T019 [US1] Verify User Story 1 via quickstart.md Scenario 1 on two dev instances: attach fidelity vs `scribe-cli` snapshot (SC-002), interactive control incl. TUI mouse + resize + search, host-action gates over remote (OSC 52 prompt on B, paste-confirmation on B, link opens on B — quickstart step 7), takeover of a locally-open window with dimmed-frozen banner on A, new-window creation, ≤30 s / ≤5 interactions (SC-001), first render ≤2 s (PR-002)

**Checkpoint**: MVP — a user can genuinely work on A's windows from B, with the security gates pointing at the right machine

---

## Phase 4: User Story 2 - Opt-in enablement and authorization (Priority: P1)

**Goal**: Remote access is deliberately enabled/disabled with plain-language consequences, refuses everything except the user's own devices, fails closed, and leaves an audit trail.

**Independent Test**: Quickstart Scenario 2 — fresh-default no-exposure, tailnet-only bind, typed unauthorized refusal, fail-closed with tailscaled stopped, live disable severing within 2 s, version-gate refusal.

### Implementation for User Story 2

- [X] T020 [P] [US2] Add "Remote" section to the settings webview assets in crates/scribe-settings (toggle "Allow remote control from my devices" with the UX-003 who-can-connect statement naming the signed-in account, advanced port field validated 1024–65535, passive "Tailscale not detected" note) per contracts/settings-and-config.md
- [X] T021 [P] [US2] Add `remote.enabled` / `remote.port` key-path handling to crates/scribe-settings/src/apply.rs writing the `[remote]` TOML table
- [X] T022 [P] [US2] Add the owning-machine remote status surfaces in the crates/scribe-client status bar segment code: persistent remote-enabled indicator (FR-009a) plus controller identity while any window is remote-controlled (e.g. "laptop-2 controls 1 window") and controlled-by markers in window-listing UI fed by T011's fields — covers remotely created windows with no displaced local client (FR-009b, SC-006, analysis G2)
- [X] T023 [US2] Implement disable semantics in crates/scribe-server/src/ipc_server.rs: on `remote.enabled → false` via live reload, within 2 seconds stop accepting, send each remote connection a best-effort `RemoteDisconnect { reason: Disabled }` notice, close connections and listener (owning sessions untouched), emit the bulk `severed` audit event (FR-016); remote clients present the delivered notice as the disabled cause per contracts/remote-protocol.md Disable semantics
- [X] T024 [US2] Complete the audit-event surface in crates/scribe-server (tailnet.rs + ipc_server.rs): structured `remote: accepted|refused|disconnect|severed` log lines with peer identity and reasons mirroring the wire `RemoteRefusal` taxonomy (+ `detail=tagged`) exactly per contracts/settings-and-config.md Audit log surface (FR-017)
- [ ] T025 [US2] Verify User Story 2 via quickstart.md Scenario 2 including the temporary `REMOTE_PROTOCOL_VERSION`-bump version-gate check (FR-012), the typed identity-unavailable refusal with tailscaled stopped, the 2-second disable sever with delivered notice, and audit-log cross-check (SC-005)

**Checkpoint**: Security posture complete — safe to use beyond the dev bench

---

## Phase 5: User Story 3 - Return to working locally (Priority: P2)

**Goal**: Control moves back to the owning machine with one action, the remote side is honestly displaced, and races resolve to exactly one controller.

**Independent Test**: Quickstart Scenario 3 — reclaim on A shows full current state; B shows the dimmed-frozen banner; repeated near-simultaneous claims never yield two live controllers; gates re-route after reclaim.

### Implementation for User Story 3

- [X] T026 [US3] Complete reclaim symmetry in crates/scribe-client: the dimmed-banner reclaim action performs a fresh claim with `takeover = true` from the local side; the remote client handles `WindowTakenOver` with the same displaced rendering and input suppression built in T017 — verify no stale-interactive path remains (US3 scenarios 1–2)
- [X] T027 [US3] Harden takeover races in crates/scribe-server/src/ipc_server.rs: near-simultaneous claims resolve deterministically under the existing claim lock (exactly one controller, loser displaced with correct identity in the banner, capability re-bind from T012 applied on every transition), preserving the `Arc::ptr_eq` stale-release guard; add tracing for every control transition (edge case: two machines racing)
- [ ] T028 [US3] Verify User Story 3 via quickstart.md Scenario 3 (reclaim, symmetry, race loop, post-reclaim OSC 52 routing back to A)

**Checkpoint**: Control round-trips cleanly in both directions

---

## Phase 6: User Story 4 - Survive network interruptions (Priority: P3)

**Goal**: Link loss never harms sessions; the remote client reconnects automatically and converges to true current state without ever seizing control back silently; slow links are caught up, not buffered forever.

**Independent Test**: Quickstart Scenario 4 — cut/restore network mid-output with automatic convergent reconnect, reclaim-during-outage lands B in the lost-control state (not control theft), cancel path, cycle burst with zero session damage.

### Implementation for User Story 4

- [X] T029 [US4] Implement bounded per-remote-connection output queue with catch-up semantics in crates/scribe-server/src/ipc_server.rs: on overflow, drop that connection's queued `PtyOutput`, mark affected sessions replay-dirty, send a fresh `SessionReplay` when the link drains; local Unix-socket connections unchanged (research D5, FR-013)
- [X] T030 [US4] Implement the auto-reconnect loop in crates/scribe-client/src/ipc_client.rs + crates/scribe-client/src/remote_connect.rs: capped exponential backoff, cancelable "Reconnecting to <peer>… (attempt n)" overlay, settled disconnected state with one-action reconnect; on success re-handshake + `Hello { takeover: false }` — resume with full replay when the window is unconnected, and render the lost-control state (never seize) when another controller holds it (research D6, FR-011, analysis C3)
- [ ] T031 [US4] Verify User Story 4 via quickstart.md Scenario 4 (mid-output interruption, automatic convergence vs snapshot, reclaim-during-outage lands in lost-control state, cancel path, ≥10-cycle burst — SC-004 sampled here, full bar in T035)

**Checkpoint**: All user stories independently functional

---

## Phase 7: Polish & Cross-Cutting Concerns

**Purpose**: Upgrade-path correctness, performance evidence, knowledge-graph sync, full validation

- [X] T032 [P] Handle server upgrade handoff in crates/scribe-server/src/handoff.rs + ipc_server.rs: the post-handoff server re-derives the remote listener from config; active remote connections drop cleanly (no fd carry) and recover via US4 auto-reconnect; document the decision inline per the contracts Compatibility statement
- [ ] T033 [P] Run the quickstart Performance checks and record results: PR-001 direct-path p95 keystroke latency ≤100 ms, PR-002 attach ≤2 s, PR-003 idle-enabled overhead none, PR-004 stalled-consumer bounded RSS with fluid local rendering (SC-003)
- [X] T034 Update lat.md for everything this feature changed — lat.md/protocol.md (remote transport, handshake, takeover, sever notice, helper messages), lat.md/server.md (listener lifecycle, tailnet module, flow control, audit), lat.md/client.md (remote connect, displaced state, reconnect), lat.md/settings.md (Remote section), lat.md/architecture.md (data flow) — then run `lat check` until clean
- [ ] T035 Full validation pass: complete quickstart.md Exit checklist on Linux↔Linux (plus macOS leg if hardware available) INCLUDING the scripted 100-cycle attach/detach/interruption run that closes SC-004 (analysis G3), cross-check audit log against performed actions, note any skipped legs in the completion report

---

## Dependencies & Execution Order

### Phase Dependencies

- **Setup (Phase 1)**: No dependencies — start immediately
- **Foundational (Phase 2)**: Depends on Phase 1 — BLOCKS all user stories
- **User Stories (Phases 3–6)**: All depend on Phase 2 completion
  - Priority order: US1 (P1) → US2 (P1) → US3 (P2) → US4 (P3)
  - US3 builds on US1's takeover machinery (T012/T017)
  - US4 builds on US1's connect scaffold (T030 extends T014's `remote_connect.rs` and overlay surface) — US4 is NOT independent of US1 (analysis I2)
  - US2 is independent of US1 except small ipc_server.rs merges (T023/T024) and T022's use of T011's listing fields
- **Polish (Phase 7)**: T032/T033 after the stories they exercise; T034/T035 last

### Task-level notes

- T005/T006 extend T004's file — sequential within Phase 2
- T008 depends on T006 (policy) + T007 (listener) + T002 (messages)
- T012 before T016's takeover-rebind check and T017's end-to-end check; T017 before T026
- T022 consumes T011's listing fields; schedule after T011 lands (or stub behind the field defaults)
- T030 reuses T014's overlay surface and T009's typed outcomes

### Parallel Opportunities

- Phase 1: T002, T003 alongside T001
- Phase 2: T009 (client crate) in parallel with T004–T008 (server crate)
- Phase 3: T017 (render/input layer) in parallel with T011–T013 (server)
- Phase 4: T020, T021, T022 all parallel (settings assets / apply.rs / client status bar)
- Phase 7: T032 and T033 in parallel
- After Phase 2, US2 can proceed largely in parallel with US1 (T022's data dependency on T011 noted above)

## Parallel Example: User Story 2

```bash
# Launch the three [P] surface tasks together:
Task: "Settings webview Remote section in crates/scribe-settings webview assets"      # T020
Task: "remote.* key paths in crates/scribe-settings/src/apply.rs"                     # T021
Task: "Owning-machine remote status surfaces in crates/scribe-client status bar"      # T022
```

---

## Implementation Strategy

### MVP First (User Story 1)

1. Phase 1 → Phase 2 (foundation is the bulk of the risk: tailnet identity + listener + handshake)
2. Phase 3 (US1) → **STOP and VALIDATE** with quickstart Scenario 1 (config enabled by hand-edit)
3. Demo-able MVP: attach & control across two machines with gates routed correctly

### Incremental Delivery

1. US1 = working remote control (MVP)
2. **US2 is also P1 — do not use beyond the dev bench, and do not ship, before US2 lands** (settings surface, disable severing, audit, fail-closed verification)
3. US3 rounds out the takeover loop; US4 adds resilience
4. Polish phase produces performance evidence + lat.md sync + full validation (incl. the 100-cycle SC-004 run)

### Notes

- No automated test tasks: not requested (research D10); every story carries a mandatory quickstart verification task instead
- Commit after each task or logical group
- All manual verification on dev-flavor instances; never restart the production Scribe server
