# Implementation Plan: LAN Remote Window Control (without Tailscale)

**Branch**: `014-lan-remote-control` | **Date**: 2026-07-08 | **Spec**: [spec.md](./spec.md)

**Input**: Feature specification from `/specs/014-lan-remote-control/spec.md`

**Note**: This plan builds directly on feature 013 (remote window control),
which must be present. See [research.md](./research.md) for the Phase 0
technology decisions (D1–D10).

## Summary

Add a Tailscale-free LAN path to remote window control: discover peers on
the local network (mDNS), authenticate and encrypt with mutual TLS pinned to
a per-install device identity, gate first access behind an explicit
approve-on-the-owning-machine step (trust-on-first-use, remembered), and
keep the whole LAN surface dormant except on networks the user has marked
trusted. Reuse 013's session protocol, `serve_connection` dispatch,
single-controller takeover, flow control, and host-action gates unchanged;
the LAN listener slots alongside the existing tailnet listener. This requires
refactoring 013's single-transport `RemoteControl` into **per-transport
state** — each transport (tailnet, LAN) gets its own `enabled` flag,
listener/port, connection + pending-handshake caps, and sever registry — plus
a `device_id → connection-id` index so a per-device revoke and a
per-transport disable target only the right connections (analysis C4/S1/FR-010/FR-012).
Prefer a direct LAN peer when present and fall back to the tailnet path when
off-LAN. LAN access is a separate opt-in from tailnet access, off by default.

## Technical Context

**Language/Version**: Rust (existing workspace toolchain; tokio server,
winit/wgpu client)

**Primary Dependencies**: Existing 013 machinery (RemoteControl supervisor,
serve_connection, ClientSink, takeover, replay). New third-party crates
(research D1–D3, D9): `mdns-sd` (pure-Rust mDNS advertise+browse), `netdev`
(gateway-MAC/subnet network fingerprint), `rcgen` (self-signed Ed25519
cert), `tokio-rustls` (async mutual TLS); promote `rustls` (already
transitive via reqwest) to a direct dependency. OS keyring (already present)
seals the device private key.

**Storage**: New per-machine on-disk state — the device keypair/cert
(private key in the OS keyring), a trusted-devices store, and a
trusted-networks store — plus a new `[remote.lan]` TOML config table. No
database. Compatibility decision (analysis I2): the device identity is
generated on first LAN enable and requires an interactive session + an
available keyring; if the keyring is unavailable the LAN owning-side fails
closed with a clear message (never a plaintext key on disk), and a headless
machine cannot be an owning-side LAN host in v1. Stores have a documented
location under the server's per-user state dir.

**Testing**: Manual quickstart on two machines on a real LAN
([quickstart.md](./quickstart.md)); optional high-ROI unit tests for pure
seams (pinning verifier decision, Device-ID derivation, network-fingerprint
match/fail-closed, TXT dedupe) only if requested. No new automated tests
otherwise (Constitution II, repo rule).

**Target Platform**: Linux (x86_64/arm64) and macOS (arm64/x86_64) desktop;
both machines run Scribe with feature 013 + 014.

**Project Type**: Multi-crate Rust workspace; client–server desktop app.

**Performance Goals**: Spec PR-001–004 — p95 keystroke-to-display ≤100 ms on
the LAN (at least as fast as a direct tailnet path); a discovered peer
appears in the picker ≤5 s; enabled-but-idle has no measurable local impact;
a stalled remote LAN consumer never affects local sessions (reuses 013 flow
control).

**Constraints**: Off by default and separate from tailnet opt-in
(Constitution VI, FR-012); dormant — nothing advertised or listening — on
any non-trusted network (FR-018, SEC-003); the LAN link is always encrypted
(mutual TLS) and gated by device approval before any data (SEC-001/002);
fail closed if identity/encryption/network prerequisites are unmet (FR-015);
listener started/stopped/rebound live off config and network state — the
running server is NEVER restarted for this feature (repo rule); local
Unix-socket and tailnet paths unchanged; existing frame caps and remote
protocol version policy unchanged.

**Scale/Scope**: Single user, a handful of the user's own devices on a home
LAN. After the per-transport refactor (analysis C4/S1), LAN has its OWN
connection + pending-handshake caps separate from tailnet's, and
pending-approval holds are separately capped and timed out, so LAN activity
cannot starve tailnet admission. Trust stores hold at most a few devices and
networks.

## Constitution Check

*GATE: Must pass before Phase 0 research. Re-check after Phase 1 design.*

| Principle | Verdict | How the plan complies |
|---|---|---|
| I. Code Quality & Clear Boundaries | PASS | New surfaces are focused modules: `scribe-server/src/lan/` (mDNS discovery, device identity/keystore, TLS pinning verifier, trusted-device store, network fingerprint) plus a **per-transport refactor of `RemoteControl`** so LAN and tailnet each own their enable/listener/caps/sever state (analysis C4) — a clean generalization of shared state, not a parallel accept stack; wire additions (incl. the approval-request/decision messages, analysis C1) in `scribe-common/src/protocol.rs`; UI in `scribe-client`; settings in `scribe-settings`. Four new dependencies are justified (research D9): each is the standard, actively-maintained choice with no in-tree abstraction to reuse; documented here rather than hand-rolling mDNS/TLS/cert/network-id. Typed errors per refusal reason; no duplicated protocol/config parsing. |
| II. Explicit, Risk-Based Testing | PASS | Each user story has an independent manual quickstart scenario. Pure security seams (pinning decision, Device-ID, network-fingerprint match, TXT dedupe) are called out as the unit-test seam if tests are requested. No automated tests otherwise — documented per the constitution and repo rule. |
| III. Consistent User Experience | PASS | The connect picker gains a "Local network" peer source beside the tailnet list and a transport indicator; the approval prompt and trusted-devices/trusted-networks management reuse the existing GPU dialog and Settings → Remote surfaces (013). Long-lived server sessions are never disrupted (LAN attach only installs a client writer, like every other attach). |
| IV. Performance Budgets & Measurement | PASS | Budgets fixed from spec PR-001–004; quickstart names the measurement method for each. Discovery and TLS run off the hot path; the LAN connection reuses 013's bounded `ClientSink::Remote` queue so a stalled consumer is already handled; enabled-but-idle cost is a dormant mDNS browse + a periodic network-fingerprint check. |
| V. Security & Trust Boundaries | PASS (with a documented, accepted residual risk) | New inbound trust boundary is default-off, separate opt-in, dormant on untrusted networks, encrypted (mutual TLS), identity-pinned, gated by explicit human approval before any data, fail-closed, revocable, and audited. TOFU's first-connect window is REDUCED (not fully closed) by the trusted-network gate + fingerprint display; the residual case — an attacker already on a trusted LAN impersonating a device on its very first pairing — is explicitly ACCEPTED for v1 (spec FR-006), with mandatory SAS/PAKE secure-pairing as documented future hardening. PTY-side host-action gates (clipboard, paste, links) apply unchanged via 013, routed to the controlling machine. |
| VI. Local-First Data Locality | PASS | The entire LAN path works with no account, no cloud, and no internet — discovery, approval, connection, and control are purely local (FR-002). This feature *strengthens* local-first: it removes the tailnet/account dependency for same-network control. No terminal or microphone data leaves the device except to an approved device over the encrypted LAN link the user opted into. |

**Engineering constraints**: protocol/config/persistence changes carry an
explicit compatibility decision (research D9: additive wire framing under
the exact-match version policy; new `[remote.lan]` config; new on-disk trust
stores + keypair with a documented location and first-run generation). No
server restart is required or performed. `lat.md` updates are an
implementation exit task.

**Post-design re-check (after Phase 1)**: PASS — contracts confine protocol
additions to serde-default-tolerant device-approval messages plus a
local-only discovery/trust query surface; the data model adds three small
persisted stores and one config table; the new dependencies are the only
Complexity-Tracking-worthy items and are justified below.

## Project Structure

### Documentation (this feature)

```text
specs/014-lan-remote-control/
├── plan.md              # This file (/speckit-plan command output)
├── research.md          # Phase 0 output (D1–D10)
├── data-model.md        # Phase 1 output
├── quickstart.md        # Phase 1 output
├── contracts/           # Phase 1 output
│   ├── lan-protocol.md
│   └── settings-and-config.md
└── tasks.md             # Phase 2 output (/speckit-tasks — NOT created here)
```

### Source Code (repository root)

```text
crates/
├── scribe-common/src/
│   ├── protocol.rs       # + device-approval framing (LanHello/identity,
│   │                     #   ApprovalPending/Approved/Declined), local-only
│   │                     #   discovery + trust query/mutation messages
│   └── config.rs         # + [remote.lan] table (enabled, port, trusted nets)
├── scribe-server/src/
│   ├── ipc_server.rs     # RemoteControl refactored to PER-TRANSPORT state
│   │                     #   (tailnet + LAN: own enabled/listener/caps/sever
│   │                     #   registry) + device_id→conn-id index for targeted
│   │                     #   revoke/disable; LAN accept: TCP → TLS → approval
│   │                     #   gate (LanApprovalRequest/Decision) →
│   │                     #   serve_connection (reuses ClientSink::Remote)
│   ├── lan/              # NEW module tree:
│   │   ├── mod.rs
│   │   ├── discovery.rs  #   mdns-sd advertise + browse, TXT/dedupe, iface filter
│   │   ├── identity.rs   #   device keypair/cert (rcgen), Device ID = SHA-256(SPKI),
│   │   │                 #   keyring-sealed private key, first-run generation
│   │   ├── tls.rs        #   tokio-rustls mutual-TLS config + pinning verifiers
│   │   │                 #   (ServerCertVerifier/ClientCertVerifier, sig delegation)
│   │   ├── trust.rs      #   trusted-devices store (pin/list/revoke), approval state
│   │   └── network.rs    #   netdev gateway-MAC+subnet fingerprint, trusted-nets
│   │                     #   store, fail-closed + physical-interface binding,
│   │                     #   AND a network-change watcher / periodic re-eval
│   │                     #   that pokes the supervisor to go dormant on roam
│   │                     #   to an untrusted network (analysis C5, FR-018)
│   └── config.rs         # + [remote.lan] server-side apply + live reload
├── scribe-client/src/
│   ├── remote_connect.rs # + "Local network" discovered-peer source, transport
│   │                     #   indicator, dedupe vs tailnet peers by identity
│   ├── lan_approval.rs   # NEW: owning-side approval prompt (device fingerprint),
│   │                     #   approve/decline; displaced/lost-control unchanged (013)
│   └── (settings status) # LAN-enabled + trusted-network-active/dormant indicator
└── scribe-settings/src/
    ├── apply.rs          # + remote.lan.* key paths → TOML
    └── assets + lib.rs   # + Remote → LAN section: enable, Trusted Networks
                          #   (add current / remove), Trusted Devices (list/revoke)
```

**Structure Decision**: Keep the existing crate boundaries. All genuinely
new server logic lives in a self-contained `scribe-server/src/lan/` module
tree so discovery, identity, TLS, device-trust, and network-trust are
separable and testable; the listener integrates through the existing
`RemoteControl` supervisor rather than a parallel accept stack. The client
adds one new module for the approval prompt and extends the existing connect
picker; settings and protocol changes are additive.

## Complexity Tracking

Four new dependencies are the only entries worth justifying (Constitution I
prefers existing abstractions before new deps). None have an in-tree
equivalent.

| Addition | Why needed | Simpler alternative rejected because |
|---|---|---|
| `mdns-sd` | LAN peer discovery with no daemon | Hand-rolling mDNS/DNS-SD is a large, error-prone protocol surface; daemon-based crates (zeroconf) need Avahi on Linux, breaking local-first |
| `tokio-rustls` + direct `rustls` | Async mutual TLS on the LAN link | reqwest (already present) is HTTP-client-only and cannot accept inbound; hand-rolling a handshake or adding Noise is more crypto surface than reusing the rustls already in the tree |
| `rcgen` | Generate the self-signed Ed25519 identity cert | Writing an X.509/ASN.1 encoder by hand is unjustifiable risk for a one-shot cert mint |
| `netdev` | Cross-platform gateway-MAC/subnet for the trusted-network gate | Shelling out to `ip`/`route`/`arp` per platform is brittle and parser-heavy; netdev reads netlink/routing tables permissionlessly |
