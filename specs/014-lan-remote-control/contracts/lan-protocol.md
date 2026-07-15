# Contract: LAN Transport, Discovery & Approval

**Feature**: `014-lan-remote-control` | **Date**: 2026-07-08
**Scope**: Everything specific to the LAN path — discovery, the TLS+approval
handshake, and the wire additions — plus what stays identical to 013.
Grounded in [research.md](./research.md) D1–D7.

## Discovery (mDNS / DNS-SD)

- Service type `_scribe._tcp.local.`; control port in the SRV record.
- TXT records: `txtvers=1`, `id=<hex device_id>`, `protovers=<u32>`,
  `host=<machine hostname>` (the name-match key for LAN↔tailnet dedup).
- Advertise only while LAN access is enabled AND the current network is
  trusted (FR-018). Stop advertising (send mDNS goodbye) on disable, on
  leaving a trusted network, or on shutdown. Leaving a trusted network is
  detected by a network-change watcher / periodic trust re-evaluation (not
  only by config reload), so a roam to an untrusted network goes dormant
  promptly (FR-018/SC-007).
- Browse filters: ignore self (own `id`), dedupe by `id`, drop addresses
  outside the current LAN subnet, exclude tailnet/VPN interfaces on both
  advertise and browse. Evict on `ServiceRemoved` / failed `verify()`.

## Connection establishment (LAN, owning side)

```text
1. TCP accept on the LAN listener (bound to the physical LAN address,
   present only while enabled + on a trusted network)
2. Admission permit (the LAN transport's OWN pending-handshake cap, separate
   from tailnet) before any work
3. TLS 1.3 mutual handshake (tokio-rustls):
     - listener presents its device cert; requires + verifies the client cert
     - pinning ClientCertVerifier: known device_id → verified;
       unknown → pending (TOFU); mismatch/bad-signature → hard fail
4. Trust/approval gate (app layer, above TLS):
     - known trusted device (device_id pinned) → proceed to step 5
     - pending (unknown device_id) → hold; raise LanApprovalRequest on the
       owning client (with a name-collision hint if the advertised name
       matches an already-trusted device); reveal NO window/session data until
       approved; on approve write a TrustedDevice and proceed, on
       decline/timeout refuse (Declined) and close
5. Remote protocol version gate (exact match, same as 013)
6. serve_connection dispatch — identical to 013, with ClientSink::Remote and
   a controller identity of Remote(device label)
```

Steps 3–4 replace 013's `WhoIs`/tailnet authorization; steps 5–6 are 013
unchanged. No window, session, or trusted-device data is revealed before
step 4 approves (SEC-001).

## New wire messages (scribe-common/src/protocol.rs)

Additive; `#[serde(tag = "type")]` so order is irrelevant. The remote
protocol version bumps (both machines run 013+014 of a compatible version;
exact match as in 013).

```rust
/// LAN preamble after the TLS handshake, before Hello — carries the peer's
/// advertised display name (identity itself is the pinned TLS cert, not this).
ClientMessage::LanHello { device_name: String, remote_protocol_version: u32 }

/// Owning → connecting: your connection is held pending device approval.
ServerMessage::LanApprovalPending

/// Owning → connecting: approved (proceed to Hello) or refused.
ServerMessage::LanApprovalResult {
    approved: bool,
    refusal: Option<LanRefusal>,   // present iff !approved
}

enum LanRefusal {
    Declined,            // user declined the approval prompt
    NotTrustedNetwork,   // raced the machine leaving a trusted network
    Disabled,            // LAN access turned off mid-handshake
    IncompatibleVersion, // exact-match failure (both versions named)
    Busy,                // LAN connection cap reached
}
```

`LanApprovalPending` MUST be sent before any window data so the connecting
client can show a "waiting for approval on <peer>" state (FR-014, US2.5).
There is no `IdentityChanged` refusal: because trust is keyed by
`device_id = SHA-256(SPKI)` (FR-005), a reinstalled peer simply presents a
new, unpinned `device_id` and is treated as a normal unknown device
(re-approval required, spec US2 acceptance #4). The approval prompt MAY show
an informational hint when the advertised name collides with an already-
trusted device (see below) — a display hint only, never a trust key.

## Approval request/decision messages (local Unix socket only)

The owning server raises the approval prompt on ITS OWN local client and
receives the decision back over the local socket (the GUI never handles the
remote TLS stream). Refused over any remote transport.

```rust
/// Owning server → owning local client: an unknown LAN device is pending.
ServerMessage::LanApprovalRequest {
    request_id: u64,             // correlates the decision
    device_name: String,         // advertised name (display only)
    fingerprint_words: String,   // the peer's identity fingerprint (D8)
    network_label: String,       // the trusted network it arrived on
    name_collision: bool,        // true if an already-trusted device shares this name
}

/// Owning local client → owning server: the user's decision.
ClientMessage::LanApprovalDecision { request_id: u64, approve: bool }
```

On `approve: true` the server writes a `TrustedDevice`, sends the pending
remote connection `LanApprovalResult { approved: true }`, and proceeds to
the 013 attach flow. On `approve: false` (or an approval timeout) the server
sends `LanApprovalResult { approved: false, refusal: Declined }` and closes.
A pending hold is time-bounded and counted against a LAN-specific
pending-approval cap (see Admission below) so unapproved dialers cannot
occupy admission slots indefinitely.

## Local-only helper messages (Unix socket only)

The connecting client and settings talk to their OWN local server for
discovery and trust management (the server owns mDNS, identity, and the trust
stores; the GUI never does). Refused over any remote transport.

```rust
ClientMessage::ListLanPeers            → ServerMessage::LanPeerList { peers }
ClientMessage::ListTrustedDevices      → ServerMessage::TrustedDeviceList { devices }
ClientMessage::RevokeTrustedDevice { device_id }   → (ack; ends live conn)
ClientMessage::ListTrustedNetworks     → ServerMessage::TrustedNetworkList { networks, current_trusted: bool }
ClientMessage::AddCurrentNetworkTrusted            → (ack, or error if unidentifiable)
ClientMessage::RemoveTrustedNetwork { id }         → (ack; may go dormant)
```

The approval prompt itself is delivered to the owning client as a
server→client event carrying the `ApprovalRequest` (device name +
fingerprint words); the client replies approve/decline, which the server
turns into a TrustedDevice write or a `Declined` refusal.

## Transport selection (connecting side)

Dedup is a **UX convenience, not a trust boundary** — a LAN peer's
cryptographic `device_id` and a tailnet peer's tailnet identity are
different namespaces (013's `RemotePeerInfo` carries no Scribe device id),
so they cannot be matched cryptographically. They are matched heuristically
by **machine name/hostname**:

- The mDNS advert carries the machine hostname (TXT `host`) in addition to
  `id`; the tailnet peer's MagicDNS short name is its name. `ListLanPeers`
  and the tailnet peer list are merged in the picker, matching a LAN peer to
  a tailnet peer when their names/hostnames match (best effort).
- A confidently name-matched dual-reachable peer is shown **once** with the
  **direct LAN path preferred** (FR-008). When names do not confidently
  match, the peer may appear once per transport, each row clearly labeled
  ("Local network" / "Tailscale"); connecting via either works.
- Attaching to a LAN peer dials `host:port` over TLS and runs the sequence
  above; attaching to a tailnet-only peer uses 013's path unchanged.
- The client displays which transport a controlled window is using (FR-009).

## Admission (per-transport, LAN)

The LAN transport has its **own** connection and pending-handshake caps,
separate from the tailnet transport's (013's shared `RemoteControl` state is
split per-transport — see plan.md). A LAN connection held **pending
approval** (no data) counts against a dedicated pending-approval cap and is
released on a bounded approval timeout, so unapproved LAN dialers can neither
exhaust the tailnet admission pool nor hold a slot across an unbounded
human-decision window (SEC / resource-exhaustion). Disabling or going dormant
on one transport severs only that transport's connections.

## Reused unchanged from 013 (explicit)

- The whole post-approval session: `Hello`/`Welcome`, `AttachSessions`,
  `SessionReplay`, `PtyOutput`, `KeyInput`, resize/scroll/search, workspace
  messages — byte-identical semantics.
- Single-controller takeover, dimmed lost-control view, one-action reclaim,
  the server-side `Arc::ptr_eq` control-authorization guard.
- Bounded `ClientSink::Remote` output queue + `SessionReplay` resync (flow
  control, PR-004).
- Host-action gates (OSC 52 clipboard policy, paste confirmation, link
  opening) routed to the controlling machine, and the auto-reconnect model.

## Compatibility statement

- Local Unix-socket clients: zero change.
- Tailnet path (013): zero change; LAN is a separate opt-in and a separate
  listener/port.
- Remote wire protocol: additive messages under the exact-match version
  policy; a version mismatch is refused with both versions named. Client and
  server of one install upgrade together, as today.
- Server↔server handoff: LAN listener + trust state re-derive from config +
  stores after upgrade; live LAN connections drop and the client
  auto-reconnects (as 013 does for the tailnet path); no handoff-state shape
  change unless the device keypair must be carried (it is on disk/keyring, so
  it need not be).
