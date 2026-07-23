# Implementation Plan: Multi-Machine Collaborative Window Sharing

**Branch**: `015-multi-machine-sharing` | **Date**: 2026-07-22 | **Spec**: [spec.md](spec.md)

**Input**: Feature specification from `/specs/015-multi-machine-sharing/spec.md`

## Summary

Replace the per-window single-writer model (one `SharedWriter` per `WindowId`, takeover swaps it, displaced clients freeze) with a per-window participant set: every attached client receives the live output fan-out through its own existing bounded `RemoteSink` queue, and input authorization is decided by a server-side sharing mode chosen in Remote settings — Single controller (legacy, default), Shared view/single typist (control passes without disconnecting anyone), or Collaborative free-for-all (all participants type, tmux/`screen -x` style). One authoritative PTY grid is maintained smallest-attached-wins while each client renders it responsively at its own window size. Trust gates (013 tailnet WhoIs, 014 LAN device approval) are unchanged; the remote protocol version bumps 2 → 3.

## Technical Context

**Language/Version**: Rust (workspace toolchain, edition 2021; same as branch `013-remote-window-control`)

**Primary Dependencies**: existing workspace crates only — `scribe-server` (tokio, authoritative Term/PTY), `scribe-common` (named-MessagePack framed protocol), `scribe-client` (winit/wgpu). No new external dependencies.

**Storage**: server config file (existing Remote settings section) gains `sharing_mode`, `control_acquisition`, `participant_limit`; no other persistence.

**Testing**: `cargo test` via the existing pre-commit harness; existing `ipc_server` unit tests updated where claim/authorization behavior changes; two-machine manual validation per quickstart.md. New test files only if explicitly requested.

**Target Platform**: Linux + macOS (same as current client/server), local Unix socket + tailnet TCP (013) + LAN mTLS (014) transports.

**Project Type**: desktop app (GPU terminal client) + long-lived session server, single Rust workspace.

**Performance Goals**: control-holder typing latency indistinguishable from single-machine use (SC-002); fan-out adds only per-participant queue enqueues on the PTY output path — no added await points on the authoritative Term; a fully stalled participant causes zero measurable slowdown for others (SC-004, preserves 013 FR-013/PR-004).

**Constraints**: never block the PTY reader on any client sink; per-participant droppable backlog stays bounded at `REMOTE_OUTPUT_QUEUE_BYTES` (4 MiB) with resync-on-overflow; all per-window share state mutates under a single lock acquisition to prevent roster/controller drift; no server restart required to apply settings (live settings-apply path).

**Scale/Scope**: 2–5 participants typical, configurable limit default unlimited; touches `crates/scribe-server/src/ipc_server.rs`, `crates/scribe-common/src/protocol.rs`, `crates/scribe-client/src/{main.rs,lost_control.rs,remote_connect.rs}`, Remote settings webview, and `lat.md/`.

## Constitution Check

*GATE: Must pass before Phase 0 research. Re-check after Phase 1 design.*

- **I. Code Quality and Clear Boundaries — PASS.** All changes stay inside the existing crates and their documented responsibilities: share state and fan-out in `scribe-server`'s IPC layer, message shapes in `scribe-common`, rendering/UX in `scribe-client`. The three per-window maps (`connected_clients`, `window_controllers`, `window_clipboard_gating`) are consolidated into one `WindowShare` entry per window under one lock — this *removes* the existing tri-lock ordering hazard rather than adding state. No new dependencies.
- **II. Explicit, Risk-Based Testing — PASS.** Each user story has an independent verification path (quickstart.md scenarios); existing `ipc_server` claim/authorization tests are updated because they cover changed behavior; no new test files are planned because the user has not requested test code (per repo rule), and the reason is documented here.
- **III. Consistent User Experience — PASS.** Legacy displaced banner, reconnect overlay, connect-picker, and status-bar idioms are reused for roster/notice/claim affordances; local single-machine flows are pixel-identical when sharing mode is the default.
- **IV. Performance Budgets and Measurement — PASS.** Budget: no added latency on the PTY→local render path; fan-out is N bounded non-blocking enqueues (N = participants). Verification: quickstart includes a stalled-participant scenario and a typing-latency spot check with 3 participants.
- **V. Security and Trust Boundaries — PASS.** No new access paths: join requires the same 013 WhoIs / 014 device-approval gates; OSC 52 prompts route to the control holder with owner fallback (default-safe); paste/link confirmation stays on the acting machine; per-device revoke ejects exactly that participant.
- **VI. Local-First Data Locality — PASS.** No network features added beyond the existing user-enabled transports; everything functions offline locally.
- **Engineering constraint — protocol/config migration decision: PASS.** `REMOTE_PROTOCOL_VERSION` 2 → 3 with exact-match negotiation: older remotes receive the existing `IncompatibleVersion` refusal (explicit outcome per FR-014). New config fields default to legacy behavior (`sharing_mode = single-controller`), so upgraded servers behave identically until the user opts in.
- **Operational safety — PASS.** No server restart is required for any step in this plan; settings apply live.

*Post-design re-check (after Phase 1): PASS — design artifacts introduce no violations; Complexity Tracking is empty.*

## Project Structure

### Documentation (this feature)

```text
specs/015-multi-machine-sharing/
├── plan.md              # This file
├── research.md          # Phase 0 output — decisions D1–D8
├── data-model.md        # Phase 1 output — WindowShare, Participant, ControlState
├── quickstart.md        # Phase 1 output — two-machine validation scenarios
├── contracts/
│   └── remote-protocol-v3.md   # Phase 1 output — message catalogue delta v2 → v3
└── tasks.md             # Phase 2 output (/speckit-tasks — NOT created by /speckit-plan)
```

### Source Code (repository root)

```text
crates/
├── scribe-common/src/protocol.rs        # v3 messages: roster, control claim/request/grant,
│                                        #   viewport report; version bump 2 → 3
├── scribe-server/src/
│   ├── ipc_server.rs                    # WindowShare map (replaces connected_clients +
│   │                                    #   window_controllers + window_clipboard_gating),
│   │                                    #   multi-sink fan-out in send_to_client path,
│   │                                    #   mode-aware input authorization, min-grid resize,
│   │                                    #   roster broadcast, mode-change application
│   └── tailnet.rs / LAN accept path     # unchanged (identity gates reused as-is)
├── scribe-common/src/config.rs          # RemoteConfig gains sharing_mode,
│                                        #   control_acquisition, participant_limit
└── scribe-client/src/
    ├── main.rs                          # viewer state (live, input-suppressed), roster/presence
    │                                    #   handling, claim/request keybinding, grid-vs-window
    │                                    #   responsive centering
    ├── lost_control.rs                  # legacy displaced path only (single-controller mode)
    ├── remote_connect.rs                # picker shows share occupancy instead of binary in-use
    └── status bar / settings webview    # presence badge; Remote settings: 3 new controls
```

**Structure Decision**: single existing Rust workspace; no new crates, no new modules beyond what `ipc_server.rs` may split off for the share registry if it keeps the file reviewable. The consolidation of three per-window maps into one `WindowShare` map is the only structural change to server state.

## Design Decisions (summary — full rationale in research.md)

- **D1 — Participant set replaces single writer**: one `WindowShare { participants, control, clipboard_gating }` entry per window under a single lock; `LiveSession`'s single sink slot becomes the set of attached participant sinks; `send_to_client` becomes a fan-out over sinks (each remote via its existing bounded `RemoteSink`, never blocking).
- **D2 — Mode-driven input authorization**: `connection_may_type(window, conn, mode)` replaces the `Arc::ptr_eq` single-writer guard: legacy ptr-eq in Single controller; holder-only in single-typist; membership in free-for-all. Gated message set unchanged (`KeyInput`, `Resize`→viewport, `CloseSession`, `CloseWindow`, `FocusChanged`, `SearchRequest` follow the holder in single-typist mode).
- **D3 — Smallest-wins grid, responsive render**: participants report viewports; server applies min(rows, cols) across participants (incl. the owner's window) via the existing `resize_term`/`TIOCSWINSZ` path with debounce; clients center/pad the authoritative grid inside their own window.
- **D4 — Protocol v3**: additive messages (roster broadcast, control claim/request/grant, viewport report) + `WindowInfo.participants`; exact-match version bump 2 → 3; older remotes get the existing `IncompatibleVersion` refusal.
- **D5 — Fan-out reuses 013 flow control**: per-participant bounded queue + `drop_pty_backlog` + `send_resync_replay`; a slow participant resyncs alone (existing D5/PR-004 machinery, now per participant).
- **D6 — Settings**: three Remote-settings controls (`sharing_mode` default Single controller, `control_acquisition` default Free claim, `participant_limit` default unlimited) applied live; mode changes take effect immediately on active shares (demote / detach with notice per FR-017).
- **D7 — Host actions**: paste/link confirm-and-act on the acting machine (today's client-side behavior, per participant); session-initiated OSC 52 routes to the control holder, owner fallback when control is unheld or mode is free-for-all.
- **D8 — Presence & UX**: full-roster broadcast on every membership/control change (no deltas); status-bar presence badge; viewer state renders live with input suppressed except the claim/request affordance; legacy frozen `LostControlState` remains only for Single-controller displacement.

## Complexity Tracking

> No constitution violations — table intentionally empty.
