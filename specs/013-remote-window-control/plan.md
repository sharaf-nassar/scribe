# Implementation Plan: Remote Window Control over Tailscale

**Branch**: `013-remote-window-control` | **Date**: 2026-07-03 | **Spec**: [spec.md](./spec.md)

**Input**: Feature specification from `/specs/013-remote-window-control/spec.md`

**Note**: This template is filled in by the `/speckit-plan` command. See `.specify/templates/plan-template.md` for the execution workflow.

## Summary

Let a user attach to and control a Scribe window from another of their
machines on the same tailnet, with single-controller takeover semantics,
opt-in enablement, and tailnet-identity authorization. Technical approach
(per [research.md](./research.md)): run the existing length-prefixed
MessagePack protocol over a TCP listener bound strictly to the machine's
Tailscale addresses; authenticate every connection via the Tailscale
daemon's `WhoIs` identity (same-account policy, fail closed); add an
explicit remote handshake for protocol-version gating and a takeover claim
to the window-ownership path; reuse `SessionReplay` for attach fidelity and
slow-link resync. No changes to the local Unix-socket path for existing
clients.

## Technical Context

**Language/Version**: Rust (existing workspace toolchain; server on tokio,
client on winit/wgpu)

**Primary Dependencies**: Existing only — tokio (adds `TcpListener` use),
`rmp-serde` named MessagePack, `alacritty_terminal`, zstd replay, existing
GPU dialog/status-bar chrome, settings webview. New third-party crates:
none (the Tailscale LocalAPI client is a small hand-rolled HTTP-over-Unix-
socket module; `serde_json` for its responses is already in the tree —
verify during implementation, add if absent).

**Storage**: Existing TOML config file — new `[remote]` table (no other
persistence; audit events go to the server log)

**Testing**: Manual quickstart scenarios per user story
([quickstart.md](./quickstart.md)); replay-fidelity checks reuse the
existing `RequestSnapshot`/`scribe-cli` snapshot tooling; no new automated
test code unless explicitly requested (Constitution II, repo testing rule)

**Target Platform**: Linux (x86_64/arm64) and macOS (arm64/x86_64)
desktop; both machines run full Scribe installs

**Project Type**: Multi-crate Rust workspace; client–server desktop app

**Performance Goals**: Spec PR-001–PR-004 — p95 keystroke-to-display
≤100 ms over a direct tailnet path; first full render of a typical window
(≤8 sessions, default scrollback) ≤2 s; enabled-but-idle remote access has
no measurable local impact; local sessions unaffected by a stalled remote
consumer

**Constraints**: Off by default, listener exists only while enabled
(Constitution VI); bind only to tailnet IPs, never `0.0.0.0`; fail closed
when tailscaled/WhoIs is unavailable; enable/disable applies live via the
existing `ConfigReloaded` path — the running server is NEVER restarted for
this (repo rule); local Unix-socket clients see zero protocol change;
existing 64 MiB frame cap and 4 KiB `KeyInput` chunking unchanged

**Scale/Scope**: Single user, a handful of tailnet peers; new cap of 8
concurrent remote connections alongside the existing 32 local; windows with
up to 256 sessions replay within the existing size limits

## Constitution Check

*GATE: Must pass before Phase 0 research. Re-check after Phase 1 design.*

| Principle | Verdict | How the plan complies |
|---|---|---|
| I. Code Quality & Clear Boundaries | PASS | Transport + auth live in `scribe-server` (new `tailnet` module + a parallel accept path in `ipc_server.rs`); wire types in `scribe-common/src/protocol.rs`; UI in `scribe-client`; settings in `scribe-settings`. No duplicated framing/config parsing — the existing `framing.rs` functions are generic over the stream and are reused as-is. Typed errors for refusal reasons. |
| II. Explicit, Risk-Based Testing | PASS | Each user story has an independent manual verification scenario in quickstart.md, with commands. SC-002 fidelity uses existing snapshot tooling. No new automated tests planned: none requested, and the touched behavior (new transport/auth) has no existing harness coverage — documented here per the constitution. |
| III. Consistent User Experience | PASS | Settings in the existing webview (new Remote section, TOML key paths through `apply.rs`); indicators in the existing status-bar segment language; takeover banner and connect picker use the established GPU overlay dialog chrome; long-lived server sessions are never disrupted (takeover moves only the client writer). |
| IV. Performance Budgets & Measurement | PASS | Budgets fixed in Technical Context from spec PR-001–004; quickstart.md names the measurement method for each (latency sampling, attach timing, stalled-consumer memory bound via RSS, local frame health). Hot path adds no allocation for local clients; remote path reuses existing buffers plus one bounded per-connection queue. |
| V. Security & Trust Boundaries | PASS | New inbound trust boundary is default-off, tailnet-only, identity-checked before any data flows, fail-closed, and audited. PTY-side gates route to the controlling client with a dedicated implementation/verification task (tasks T016/T019, quickstart S1.7/S3.3): OSC 52 policy engine and capability bit re-bind to the new controller at takeover; paste confirmation runs on the controlling client; takeover is never silent (banner + reclaim), and auto-reconnect never seizes control. |
| VI. Local-First Data Locality | PASS | Core terminal and AI features unchanged and network-free. Remote access is explicit opt-in (terminal contents leave the device only to the user's own authenticated device over the tailnet's encrypted transport). Disabling tears the listener down immediately. |

**Engineering constraints**: protocol change carries an explicit
compatibility decision (remote-only version handshake, local socket
untouched — see research.md D3 and contracts/remote-protocol.md); no server
restart is required or performed for any part of this feature; `lat.md`
updates are called out as an implementation exit task.

**Post-design re-check (after Phase 1)**: PASS — the contracts confine all
protocol additions to `serde`-default-tolerant variants plus a remote-only
preamble; the data model adds no persistence beyond one TOML table; no
Complexity Tracking entries required.

## Project Structure

### Documentation (this feature)

```text
specs/013-remote-window-control/
├── plan.md              # This file (/speckit-plan command output)
├── research.md          # Phase 0 output (/speckit-plan command)
├── data-model.md        # Phase 1 output (/speckit-plan command)
├── quickstart.md        # Phase 1 output (/speckit-plan command)
├── contracts/           # Phase 1 output (/speckit-plan command)
│   ├── remote-protocol.md
│   └── settings-and-config.md
└── tasks.md             # Phase 2 output (/speckit-tasks command - NOT created by /speckit-plan)
```

### Source Code (repository root)

```text
crates/
├── scribe-common/src/
│   ├── protocol.rs          # + RemoteHandshake/RemoteHandshakeReply, Hello.takeover,
│   │                        #   ServerMessage::WindowTakenOver, REMOTE_PROTOCOL_VERSION
│   ├── config.rs            # + RemoteConfig ([remote] table types, defaults)
│   └── framing.rs           # unchanged (already generic over AsyncRead/AsyncWrite)
├── scribe-server/src/
│   ├── ipc_server.rs        # + TCP accept path sharing the existing dispatch;
│   │                        #   takeover in window-claim; bounded remote output
│   │                        #   queue + SessionReplay resync; remote conn cap
│   ├── tailnet.rs           # NEW: minimal Tailscale LocalAPI client (status,
│   │                        #   whois), identity policy (same-account, fail closed),
│   │                        #   tailnet address enumeration for bind
│   ├── config.rs            # + [remote] server config + live reload handling
│   └── main/upgrade path    # handoff: carry remote-listener state flag (re-open
│                            #   listener from config after upgrade; no fd passing)
├── scribe-client/src/
│   ├── ipc_client.rs        # + remote transport dial (TCP + preamble), auto-
│   │                        #   reconnect loop w/ backoff + status events
│   ├── remote_connect.rs    # NEW: peer picker / connect flow state (palette +
│   │                        #   GPU dialog), distinct failure messaging
│   ├── pane/render layer    # dimmed-frozen lost-control state + takeover banner
│   └── status bar           # remote-enabled / remote-controlled segments
└── scribe-settings/src/
    ├── apply.rs             # + remote.* key paths → TOML
    └── webview assets       # + "Remote" section (toggle, port, plain-language
                             #   who-can-connect statement)
```

**Structure Decision**: Extend the existing four crates along their current
responsibilities — wire types in `scribe-common`, transport/auth/ownership
in `scribe-server`, connect UX and lost-control rendering in
`scribe-client`, configuration surface in `scribe-settings`. The only new
modules are `scribe-server/src/tailnet.rs` and
`scribe-client/src/remote_connect.rs`; everything else is additive change
inside existing files, preserving the crate map documented in
`lat.md/architecture`.

## Complexity Tracking

> **Fill ONLY if Constitution Check has violations that must be justified**

No violations — table intentionally empty.
