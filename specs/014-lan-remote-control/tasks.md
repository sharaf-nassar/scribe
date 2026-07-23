# Tasks: LAN Remote Window Control (without Tailscale)

**Input**: Design documents from `/specs/014-lan-remote-control/`

**Prerequisites**: plan.md, spec.md, research.md (D1–D10), data-model.md, contracts/lan-protocol.md, contracts/settings-and-config.md, quickstart.md. Builds on feature **013** (must be present on the branch).

**Tests**: Not requested — no automated test tasks (research D10, Constitution II). Each story ends with a mandatory manual quickstart verification task. High-ROI pure-logic unit-test seams (pinning-verifier decision, Device-ID derivation, network-fingerprint match/fail-closed, mDNS TXT dedupe) are noted as optional and added only on request. All two-machine verification runs with **Tailscale off** (to prove local-first) except the fallback scenario.

**Organization**: Tasks grouped by user story for independent implementation and testing.

## Format: `[ID] [P?] [Story] Description`

- **[P]**: Can run in parallel (different files, no dependencies)
- **[Story]**: US1–US4 (story phases only)
- Include exact file paths.

## Path Conventions

Multi-crate Rust workspace; new server logic in `crates/scribe-server/src/lan/`.

---

## Phase 1: Setup (Shared Infrastructure)

**Purpose**: Dependencies, wire types, and config every later task builds on

- [X] T001 Add dependencies (research D9) to Cargo.toml workspace + crate manifests: `mdns-sd`, `netdev`, `rcgen`, `tokio-rustls`, and promote `rustls` to a direct `scribe-server` dependency; update the `[package.metadata.cargo-machete]` ignore list only if a dep is used solely via macro; run `cargo deny check` to confirm licenses
- [X] T002 [P] Add `[remote.lan]` config (`enabled: bool = false`, `port: u16 = 46062`, serde defaults, missing-table ⇒ defaults) in crates/scribe-common/src/config.rs, threaded into server config in crates/scribe-server/src/config.rs per contracts/settings-and-config.md
- [X] T003 [P] Add LAN wire messages to crates/scribe-common/src/protocol.rs per contracts/lan-protocol.md: `ClientMessage::LanHello`, `ServerMessage::{LanApprovalPending, LanApprovalResult}`, `LanRefusal` enum (Declined/NotTrustedNetwork/Disabled/IncompatibleVersion/Busy — NO IdentityChanged), the approval push/reply `ServerMessage::LanApprovalRequest{request_id, device_name, fingerprint_words, network_label, name_collision}` + `ClientMessage::LanApprovalDecision{request_id, approve}` (local-only), and the local-only helpers `ListLanPeers`/`LanPeerList`, `ListTrustedDevices`/`TrustedDeviceList`, `RevokeTrustedDevice`, `ListTrustedNetworks`/`TrustedNetworkList`, `AddCurrentNetworkTrusted`, `RemoveTrustedNetwork` (+ `LanPeerInfo` incl. `host`, `TrustedDeviceInfo`, `TrustedNetworkInfo`); bump the remote protocol version (exact-match policy)

---

## Phase 2: Foundational (Blocking Prerequisites)

**Purpose**: The LAN spine — identity, discovery, network gate, TLS, trust, the per-transport `RemoteControl` refactor, listener, accept path

**⚠️ CRITICAL**: No user story work can begin until this phase is complete

- [X] T004 Create device identity module in new file crates/scribe-server/src/lan/identity.rs: generate once a self-signed Ed25519 X.509 cert (`rcgen`, `PKCS_ED25519`), Device ID = `SHA-256(SubjectPublicKeyInfo)`, private key sealed in the OS keyring, word-list fingerprint (research D2/D8); FIRST-RUN requires an interactive session + an available keyring — if the keyring is unavailable, fail closed with a clear message (never a plaintext key on disk, analysis I2); register `pub mod lan` in lib.rs
- [X] T005 Create mDNS discovery module in crates/scribe-server/src/lan/discovery.rs (`mdns-sd`): advertise `_scribe._tcp.local.` with TXT `txtvers`/`id`/`protovers`/`host` (hostname, for LAN↔tailnet name dedup) and the control port in SRV; browse + resolve; dedupe by TXT `id`, filter resolved addrs to the current LAN subnet, `disable_interface` tailnet/VPN interfaces both sides; evict on `ServiceRemoved`/failed `verify()` (research D1)
- [X] T006 Create network fingerprint + trusted-networks store + change watcher in crates/scribe-server/src/lan/network.rs (`netdev`): gateway-MAC + subnet of the PHYSICAL LAN interface (not tunnel), match rule (equal non-zero gateway MAC AND subnet), fail-closed on zero MAC / no default route / VPN-only; add/remove/list trusted networks and `is_current_network_trusted()`; AND a network-change watcher (link/route-change subscription and/or periodic poll) that re-evaluates trust and pokes the `RemoteControl` supervisor to go dormant/active promptly on roam — NOT only on config reload (analysis C5, research D5, FR-018/SC-007)
- [X] T007 Create TLS module in crates/scribe-server/src/lan/tls.rs (`tokio-rustls`, `rustls` 0.23): mutual-TLS server + client configs presenting the device cert; custom `ServerCertVerifier` + `ClientCertVerifier` implementing the SPKI-pin decision (known→verified, unknown→pending/TOFU, bad-signature→hard fail; NO "identity changed" state — an unpinned id is simply unknown) with signature checks DELEGATED to `rustls::crypto::verify_tls1{2,3}_signature` (never stubbed); fixed placeholder `ServerName` (research D3/D4)
- [X] T008 Create trusted-devices store + approval state in crates/scribe-server/src/lan/trust.rs: persist approved devices (device_id, cert, label, first_seen, approved_on_network), pin/list/revoke keyed by device_id, and the `ApprovalRequest` lifecycle (pending→approved/declined) with a bounded approval timeout; a `name_collision` check (advertised name matches an already-trusted device — informational hint only, never a trust key) (research D4, data-model.md)
- [X] T009 Refactor `RemoteControl` to PER-TRANSPORT state in crates/scribe-server/src/ipc_server.rs (analysis C4/S1): split the single `enabled`/listener/`conn_limit`/`handshake_limit`/sever-`connections` registry into independent per-transport (tailnet, LAN) state so disabling or going dormant on one severs only that transport, and add a `device_id → connection-id` index so a single device can be severed on revoke. Preserve tailnet behavior byte-identically; add LAN-specific connection + pending-handshake caps and a separate cap+timeout on concurrent pending-approval holds (S1). This is a prerequisite for the LAN listener and per-device revoke
- [X] T010 Add the LAN listener to the refactored supervisor in crates/scribe-server/src/ipc_server.rs: a LAN transport present only while `remote.lan.enabled` AND `is_current_network_trusted()` (T006), bound to the physical LAN address on `remote.lan.port`; start/stop/rebind live via the `ConfigReloaded` path AND the network-change watcher (T006); advertise via T005 only while active, send mDNS goodbye when going dormant — no server restart
- [X] T011 Implement the LAN accept path in crates/scribe-server/src/ipc_server.rs per contracts/lan-protocol.md: TCP accept → reserve the LAN-specific pending-handshake permit → `tokio-rustls` mutual handshake (pinning verifier) → read `LanHello` → trust/approval gate (known trusted → proceed; unknown → record pending under the pending-approval cap+timeout, push `ServerMessage::LanApprovalRequest` to the owning client + send `LanApprovalPending`, hold with NO data; `LanApprovalDecision{approve:false}`/timeout → `LanApprovalResult{refusal:Declined}`) → exact version gate → hand to `serve_connection` with `ClientSink::Remote` and a `Remote(device label)` controller identity; register in the LAN sever registry + device index (T009); emit LAN audit lines
- [ ] T012 Foundational checkpoint (manual): on two dev instances with Tailscale off, `[remote.lan] enabled=true` and the network hand-trusted, confirm A advertises + binds only the LAN address, mutual TLS completes, an unknown device is held pending (no data, `LanApprovalRequest` raised), a hand-approve reaches `serve_connection`, and roaming to an untrusted network goes dormant promptly

**Checkpoint**: LAN spine ready — user story implementation can begin

---

## Phase 3: User Story 1 - Control a nearby machine with no Tailscale (Priority: P1) 🎯 MVP

**Goal**: From B (Tailscale off), discover A on the LAN, connect over encrypted mutual TLS, and control its window with full 013 fidelity.

**Independent Test**: Quickstart Scenario 1 end-to-end on two dev instances with Tailscale off. Note: the human-approve step is delivered by US2's approval prompt (T018) — both are P1 and form the MVP together; until T018 lands, approve via a temporary dev shortcut.

### Implementation for User Story 1

- [X] T013 [US1] Implement the `ListLanPeers` handler (local-Unix-socket only; refused over any remote transport) returning discovered LAN peers (incl. `host`) from the discovery module, in crates/scribe-server/src/ipc_server.rs
- [X] T014 [US1] Add a "Local network" peer source to the connect picker in crates/scribe-client/src/remote_connect.rs: list discovered LAN peers by name (via `ListLanPeers`), keep manual `host:port` entry, and dial the chosen peer over the LAN transport
- [X] T015 [US1] Add LAN dial over TLS in crates/scribe-client/src/ipc_client.rs: `tokio-rustls` connect presenting this device's client cert with the pinning `ServerCertVerifier`, send `LanHello`, and map `LanApprovalPending` (→ "waiting" state), `LanApprovalResult`/`LanRefusal`, and connect/TLS errors into typed UI outcomes (incl. the combined connection-failure copy)
- [X] T016 [US1] Wire LAN-attached windows through the existing 013 session plumbing in crates/scribe-client/src/ipc_client.rs + pane layer: replay restore, `PtyOutput`, `KeyInput` chunking, resize-before-input ordering, scroll/search — all over the TLS stream, transport-agnostic after `Hello`
- [ ] T017 [US1] Verify User Story 1 via quickstart.md Scenario 1 on two Tailscale-off dev instances: discover A ≤5 s, dial over TLS, approve (via T018 or dev shortcut), full control per 013 (SC-001/002/003), encrypted-link + no-internet properties

**Checkpoint**: MVP — LAN discover → encrypted connect → control works (paired with US2 approval)

---

## Phase 4: User Story 2 - Approve a new device + trusted networks (Priority: P1)

**Goal**: First connection from an unknown device is gated by an explicit approval prompt on the owning machine; approved devices are remembered; the whole LAN surface is dormant on untrusted networks.

**Independent Test**: Quickstart Scenarios 2 & 3 — untrusted-network dormancy, approve/decline, reinstalled-device-as-unknown.

### Implementation for User Story 2

- [X] T018 [P] [US2] Create the owning-side approval prompt in new file crates/scribe-client/src/lan_approval.rs: on `ServerMessage::LanApprovalRequest`, a GPU-overlay dialog (reusing existing dialog chrome) showing the requesting device's name, network, and fingerprint words, an informational name-collision hint when `name_collision` is set, and equally-prominent Approve/Decline → `ClientMessage::LanApprovalDecision`; on Approve the server writes a `TrustedDevice`, on Decline it returns `LanRefusal::Declined` (FR-004/006, SEC-001/002, UX-002)
- [X] T019 [US2] Add the connecting-side "Waiting for approval on <peer>…" pending overlay (from `LanApprovalPending`) in crates/scribe-client/src/remote_connect.rs, cancelable, settling on the `LanApprovalResult`
- [X] T020 [P] [US2] Add the "Local network" section to Settings → Remote in crates/scribe-settings (assets + lib.rs): enable toggle with UX-003 copy, active/dormant/off status (UX-004), Trusted Networks list (list/add-current — disabled+explained when unidentifiable/remove), Trusted Devices list (list/revoke), and a read-only "This device's fingerprint" display (word list + hex) for the optional out-of-band compare (FR-006, analysis C2); add `remote.lan.*` key paths to crates/scribe-settings/src/apply.rs
- [X] T021 [US2] Implement trusted-network management + gate enforcement in crates/scribe-server/src/ipc_server.rs: `AddCurrentNetworkTrusted`/`RemoveTrustedNetwork`/`ListTrustedNetworks` handlers; enforce dormancy — on leaving a trusted network (via the T006 watcher) or removing the current one, stop advertising and drop the LAN transport promptly (coordinated with T010), severing only LAN connections (T009)
- [X] T022 [US2] Complete the LAN audit surface in crates/scribe-server (lan/* + ipc_server.rs): structured `lan: approved|declined|revoked|accepted|refused|disconnect|dormant` lines with device/network identity and typed reasons (no identity-changed), per contracts/settings-and-config.md (FR-017)
- [ ] T023 [US2] Verify User Story 2 via quickstart.md Scenarios 2 & 3: untrusted-network dormancy incl. a roam going dormant without a config change (undiscoverable, nothing listening — SC-007), approve/decline, a reinstalled device appearing as a new unknown device (with name-collision hint) requiring fresh approval, audit cross-check

**Checkpoint**: Security posture complete — safe to use beyond the dev bench

---

## Phase 5: User Story 3 - Prefer local, fall back to Tailscale (Priority: P2)

**Goal**: The same connect action prefers a direct LAN peer when present and falls back to the tailnet path when off-LAN, showing which path is in use, with a confidently name-matched dual-reachable peer listed once.

**Independent Test**: Quickstart Scenario 5 — prefer LAN at home, fall back to Tailscale away, single entry for a name-matched dual-reachable peer.

### Implementation for User Story 3

- [X] T024 [US3] Implement transport selection + name-based dedup in crates/scribe-client/src/remote_connect.rs: merge LAN-discovered peers (by TXT `host`) and tailnet peers (by MagicDNS name) matching on machine name/hostname — a best-effort UX heuristic, NOT identity (analysis C3); a confidently matched dual-reachable peer appears once with the direct LAN path preferred, an unmatched peer may list once per transport with clear labels; fall back to the 013 tailnet path when the peer is not on the LAN (FR-008/009)
- [X] T025 [US3] Add a transport indicator for a controlled window in crates/scribe-client (status area): show "Local network" vs "Tailscale" for the active path (FR-009)
- [ ] T026 [US3] Verify User Story 3 via quickstart.md Scenario 5 (prefer LAN, fallback off-LAN, single entry for a name-matched peer) — the one scenario that runs with Tailscale ON

**Checkpoint**: Transport selection works both home and away

---

## Phase 6: User Story 4 - Review and revoke trusted devices (Priority: P2)

**Goal**: The user can review approved devices and revoke any; a revoked device loses access immediately and must re-approve.

**Independent Test**: Quickstart Scenario 4 — list, revoke a connected device, verify re-approval required.

### Implementation for User Story 4

- [X] T027 [US4] Implement `ListTrustedDevices` + `RevokeTrustedDevice` handlers in crates/scribe-server/src/ipc_server.rs + lan/trust.rs: revoke removes the pin and, via the `device_id → connection-id` index (T009), severs ONLY that device's live connection promptly, forcing re-approval on the next attempt (FR-010, SC-006); surfaced in the Settings Trusted Devices list (T020)
- [ ] T028 [US4] Verify User Story 4 via quickstart.md Scenario 4 (list with fingerprint + approval time, revoke a connected device, confirm only that device is severed and re-approval is required)

**Checkpoint**: All user stories independently functional

---

## Phase 7: Polish & Cross-Cutting Concerns

**Purpose**: Upgrade path, performance evidence, platform verification, knowledge-graph sync, full validation

- [X] T029 [P] Handle server upgrade handoff in crates/scribe-server/src/handoff.rs + ipc_server.rs: the LAN listener, device identity, and trust stores re-derive from config + on-disk stores after handoff (keypair on disk/keyring need not be carried); live LAN connections drop and the client auto-reconnects (as 013's tailnet path); confirm `HANDOFF_VERSION` only bumps if the handoff state shape actually changed (it should not)
- [ ] T030 [P] Run the quickstart Performance checks and record results: PR-001 LAN p95 keystroke latency ≤100 ms, PR-002 discovery ≤5 s, PR-003 idle-enabled overhead none, PR-004 stalled-consumer bounded (reuses 013 queue) — SC-005
- [ ] T031 macOS platform verification: confirm the `netdev` gateway/interface reads trigger no Location/TCC prompt; confirm a background Scribe server can actually obtain/receive the OS "Local Network" permission (and define the denied fallback — manual `host:port` still works); cold-ARP-cache gateway-MAC gets a retry/probe (research D5, analysis P1)
- [X] T032 Update lat.md for the LAN transport — lat.md/protocol.md (LAN discovery, TLS+approval handshake, approval-request/decision + device-approval messages, local-only trust/discovery helpers), lat.md/server.md (lan/* modules; the per-transport `RemoteControl` refactor + device→conn index; network-change watcher; accept path; audit), lat.md/client.md (LAN connect source, approval prompt + name-collision hint, transport indicator, pending state), lat.md/settings.md (Local network section + own-fingerprint) — then run `lat check` until clean
- [ ] T033 Full validation: complete quickstart.md Exit checklist on Linux↔Linux (plus macOS↔Linux if hardware available), Tailscale-off for Scenarios 1–4 and Tailscale-on for Scenario 5, audit-log cross-check; note skipped legs

---

## Dependencies & Execution Order

### Phase Dependencies

- **Setup (Phase 1)** → **Foundational (Phase 2)** BLOCKS all stories.
- Within Phase 2: T004→T005/T007/T008 (identity feeds TXT id, cert, self-vs-peer); T006→T010 (network gate feeds listener); **T009 (RemoteControl per-transport refactor) is a prerequisite for T010, T011, and T027** and preserves tailnet behavior; T011 depends on T007/T008/T009 + T003 messages.
- **User Stories (Phases 3–6)** depend on Phase 2.
  - Priority: US1 (P1) → US2 (P1) → US3 (P2) → US4 (P2).
  - **US1 + US2 are the joint MVP**: US1's end-to-end verify (T017) needs US2's approval prompt (T018), since every first connection requires approval. Implement T018 alongside US1 or use a temporary dev-approve for T017.
  - US3 builds on US1's picker + the tailnet path; US4's per-device revoke relies on the T009 device→conn index.
- **Polish (Phase 7)**: T029/T030/T031 after the stories they exercise; T032/T033 last.

### Parallel Opportunities

- Phase 1: T002, T003 alongside T001.
- Phase 2: T004, T005, T006 are independent new files — parallel; T007/T008 need T004; T009 is a refactor of ipc_server.rs (serialize with T010/T011).
- Phase 3: T014 (client picker) alongside T013 (server handler).
- Phase 4: T018 (client dialog), T020 (settings) parallel with T021/T022 (server).
- Phase 7: T029, T030 parallel.

## Parallel Example: Phase 2 foundational modules

```bash
# The three independent new lan/ modules can start together:
Task: "Device identity in crates/scribe-server/src/lan/identity.rs"      # T004
Task: "mDNS discovery in crates/scribe-server/src/lan/discovery.rs"       # T005
Task: "Network fingerprint + watcher in crates/scribe-server/src/lan/network.rs"  # T006
```

---

## Implementation Strategy

### MVP (User Stories 1 + 2 together)

1. Phase 1 → Phase 2 (the LAN spine + the `RemoteControl` per-transport refactor are the bulk of the risk).
2. Phase 3 (US1) + the T018 approval prompt from US2 → **STOP and VALIDATE** quickstart Scenario 1 (Tailscale off).
3. Complete US2 (trusted networks, dormancy-on-roam, decline, name-collision, audit) before using beyond the dev bench.

### Incremental Delivery

1. MVP = discover + encrypted connect + approve + control on the LAN, no Tailscale.
2. US2 rounds out the security surface; US3 adds prefer-LAN/fallback; US4 adds revoke.
3. Polish: handoff, performance evidence, macOS verification, lat.md sync, full validation.

### Notes

- No automated test tasks (not requested — research D10); each story carries a manual quickstart verification. Optional high-ROI unit seams if requested: pinning-verifier decision, Device-ID derivation, network-fingerprint match/fail-closed, TXT dedupe.
- Commit after each task or logical group.
- All two-machine verification on dev-flavor instances; never restart the production Scribe server. Tailscale off except Scenario 5.
