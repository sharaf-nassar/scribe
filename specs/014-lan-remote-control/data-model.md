# Data Model: LAN Remote Window Control

**Feature**: `014-lan-remote-control` | **Date**: 2026-07-08
**Sources**: [spec.md](./spec.md) Key Entities, [research.md](./research.md) D1–D8

Three small persisted stores (device keypair, trusted devices, trusted
networks) plus one config table and several runtime types. No database.
Persisted trust data lives under the server's per-user state directory;
the device private key is sealed in the OS keyring.

## DeviceIdentity (persisted once per install)

The machine's own stable identity for LAN pairing (research D2).

| Field | Type | Rules |
|---|---|---|
| `cert_der` | bytes | Self-signed Ed25519 X.509 cert; regenerable |
| `private_key` | keyring-sealed | Ed25519 private key; never on disk in the clear |
| `device_id` | `[u8; 32]` | `SHA-256(SubjectPublicKeyInfo)` — the trust anchor and mDNS TXT `id`; stable across cert re-mint |
| `fingerprint_words` | derived | Word-list SAS for display (research D8) |

Generated on first LAN enable if absent. The `device_id` is what peers pin.

## LanRemoteConfig (persisted, TOML `[remote.lan]`)

| Field | Type | Default | Rules |
|---|---|---|---|
| `enabled` | bool | `false` | Separate opt-in from `[remote]` tailnet access (FR-012); dormant unless on a trusted network (FR-018) |
| `port` | u16 | `46062` | LAN listener port (distinct from the tailnet `46061`); rebind on change |

Trusted networks and trusted devices are separate stores (below), not inline
config, since they grow and carry structured records.

## TrustedNetwork (persisted store)

The activation gate (research D5, FR-018/FR-019). LAN is active only when the
current network matches a record here.

| Field | Type | Rules |
|---|---|---|
| `id` | uuid | Record key |
| `label` | String | User-facing; default from SSID else "Gateway aa:bb:cc…" |
| `gateway_mac` | String | Normalized lowercase; **primary match anchor**; never the zero address |
| `subnet_cidr` | String | e.g. `192.168.1.0/24`; secondary corroborator |
| `gateway_ip` | Option<IpAddr> | Weak corroborator |
| `ssid` | Option<String> | Display hint only; may be None (wired / macOS 14.5+) |
| `added_at` | timestamp | |

**Match rule** (am I on a trusted network?): `current.gateway_mac ==
stored.gateway_mac` (non-zero) AND `current.subnet_cidr == stored.subnet_cidr`
→ activate; otherwise dormant. Fail closed on zero/unresolved gateway MAC, no
default route, or a VPN-tunnel default route (fingerprint the physical LAN
interface, not the tunnel).

## TrustedDevice (persisted store)

An approved peer allowed to control this machine over the LAN (research D4,
FR-004/005/010). The LAN trust boundary.

| Field | Type | Rules |
|---|---|---|
| `device_id` | `[u8; 32]` | `SHA-256(SPKI)` — the exact-match pin key |
| `cert_der` | bytes | Full public identity for re-verify + display |
| `label` | String | Human name (from the peer's advertised name at approval) |
| `first_seen` | timestamp | |
| `approved_on_network` | uuid → TrustedNetwork | Context where approved |
| `fingerprint_words` | derived | For the trusted-devices list display |

**Lifecycle**: absent → (approval) → trusted → (revoke) → absent. A device
whose presented `device_id` doesn't match any record is unknown (approval
required) — this includes a reinstalled device that regenerated its key
(new `device_id`), which is simply a new unknown device, never silently
trusted; the approval prompt carries a name-collision hint if it reuses a
trusted device's name (FR-005, spec US2 #4).

## ApprovalRequest (runtime, owning side)

A pending first-time LAN connection awaiting the owning user's decision
(FR-004, SEC-001/002).

| Field | Type | Rules |
|---|---|---|
| `request_id` | u64 | Correlates the `LanApprovalRequest` push with the `LanApprovalDecision` reply |
| `device_id` | `[u8; 32]` | From the completed TLS handshake (pinning verifier recorded it pending) |
| `cert_der` | bytes | Presented identity |
| `advertised_name` | String | Peer's requested label (display only) |
| `fingerprint_words` | derived | Shown on the prompt |
| `name_collision` | bool | True if an already-trusted device shares this advertised name (informational hint) |
| `network_label` | String | The trusted network the request arrived on |
| `state` | Pending \| Approved \| Declined | No window/session data flows while Pending; bounded by an approval timeout and a LAN pending-approval cap |

The request is delivered to the owning client via
`ServerMessage::LanApprovalRequest` and answered with
`ClientMessage::LanApprovalDecision` (contracts/lan-protocol.md). On
Approve → write a TrustedDevice + proceed into the 013 attach flow. On
Decline or timeout → refuse (`LanRefusal::Declined`), reveal nothing, do not
remember, and release the pending-approval slot.

## LanPeer (runtime, connecting side)

A peer discovered on the LAN via mDNS (research D1, D7).

| Field | Type | Rules |
|---|---|---|
| `device_id` | `[u8; 32]` | From TXT `id`; **dedupe key** (also vs tailnet peer identity) |
| `display_name` | String | mDNS instance name |
| `addrs` | Vec<IpAddr> | Resolved; filtered to the current LAN subnet, tailnet/VPN excluded |
| `port` | u16 | From the SRV record |
| `protovers` | u32 | From TXT; filter incompatible before connecting |
| `online` | bool | `ServiceRemoved`/`verify()` eviction |

## TlsPinVerifier (runtime)

The custom rustls verifier (research D3/D4). Not persisted; wraps the trust
store.

**States / decisions** (per handshake, both roles):

```text
present device_id == pinned known device  → verified (proceed)
present device_id != any known            → pending (TOFU: accept handshake,
                                             record pending, app-layer approval)
signature invalid                          → hard fail (peer lacks private key)
```

There is no distinct "identity changed" verifier state: trust is keyed by
`device_id` (SPKI hash), so a reinstalled/rekeyed peer presents a NEW
`device_id` and is simply unknown → pending → normal approval (spec US2 #4,
FR-005). The app-layer approval carries a `name_collision` flag (below) so
the prompt can *inform* the user when the advertised name matches an
already-trusted device — a display hint, never a trust decision. Signature
methods delegate to `rustls::crypto::verify_tls1{2,3}_signature` (never
stubbed). Approval decision lives at the app layer, not the verifier.

## TransportSelection (runtime, connecting side)

Per-peer choice of direct-LAN vs tailnet (research D7, FR-008/009). Dedup is
a UX convenience matched by **machine name/hostname** (LAN TXT `host` vs the
tailnet MagicDNS name) — NOT by cryptographic identity, since the two
transports use different identity namespaces.

| Rule | Outcome |
|---|---|
| LAN peer and tailnet peer with confidently matching name | Direct LAN; shown once |
| Peer on LAN only | LAN path |
| Peer on tailnet only | Tailnet path (013) |
| Names don't confidently match | May show once per transport, each labeled |

The client surfaces which path is in use; automatic with a manual override.

## Unchanged reused entities (from 013)

- **Window / Session / SessionReplay** — a LAN attachment behaves exactly as
  a 013 remote attachment once past the approval gate.
- **RemoteConnection / ClientSink::Remote** — the bounded output queue and
  flow control are reused for LAN connections.
- **WindowControllers / takeover / lost-control** — single-controller model
  unchanged; a LAN controller identity is `Remote(device label)`.
- **Audit surface** — extended with LAN lifecycle events (approvals,
  declines, revocations, accepted/refused connections; FR-017).
