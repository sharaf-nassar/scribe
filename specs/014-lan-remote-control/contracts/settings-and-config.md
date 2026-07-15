# Contract: Settings, Config & Trust Stores

**Feature**: `014-lan-remote-control` | **Date**: 2026-07-08
**Scope**: TOML schema, the Settings → Remote LAN surface, trust-store
locations, UI states, and failure copy. Grounded in research D5/D8/D9.

## TOML schema (`[remote.lan]` table)

```toml
[remote.lan]
enabled = false   # default; separate opt-in from [remote] (tailnet); FR-012
port = 46062      # LAN listener port (distinct from tailnet 46061)
```

- Types live beside the existing `[remote]` config in
  `scribe-common/src/config.rs`; server-side apply + live reload in
  `scribe-server/src/config.rs`, driven by the same watcher →
  `ConfigReloaded` → `RemoteControl` path 013 already uses (no restart).
- Missing table ⇒ defaults (feature off). Enabling has effect only while on
  a trusted network (FR-018).

## Trust stores & identity (on-disk)

Not in TOML (they grow and carry structured records). Stored under the
server's per-user state directory; documented location; created on first LAN
enable.

| Store | Contents | Notes |
|---|---|---|
| Device keypair/cert | self-signed Ed25519 cert (DER); private key in the OS **keyring** | generated once on first enable |
| Trusted devices | approved peers (device_id, cert, label, first_seen, network) | pin store; revocable |
| Trusted networks | gateway_mac + subnet + label (+ ssid hint) | activation gate |

## Settings → Remote → "Local network" section

| Control | Key/action | Behavior |
|---|---|---|
| Toggle "Allow control from devices on trusted local networks" | `remote.lan.enabled` | Default off. Copy (UX-003): "When on, and while you're on a network you've marked trusted, your approved devices on that network can find and control this machine's windows — no Tailscale needed." |
| Status line | (derived) | Shows LAN access **active** (on a trusted network) vs **dormant** (network not trusted) vs **off** (UX-004). |
| Trusted Networks list | `ListTrustedNetworks` / `AddCurrentNetworkTrusted` / `RemoveTrustedNetwork` | Lists trusted networks; "Add current network" (disabled if the network can't be fingerprinted — zero gateway MAC / VPN-only, with an explanatory note); remove any. |
| Trusted Devices list | `ListTrustedDevices` / `RevokeTrustedDevice` | Each approved device with name, fingerprint words, approval time; "Revoke" ends any live connection and forces re-approval. |
| This device's fingerprint | (derived from `GetRemoteEnv`-style local query) | Shows THIS machine's own device fingerprint (word list + grouped hex) so the user can read it aloud / compare it against the approval prompt on another machine (optional out-of-band MITM check, FR-006). |
| Advanced: port | `remote.lan.port` | Numeric, validated 1024–65535; helper notes default 46062. |

The LAN section is visually distinct from the existing Tailscale remote
section (separate opt-ins) but lives in the same Settings → Remote panel.

## Client UI states

| State | Surface | Contract |
|---|---|---|
| Approval prompt (owning machine) | GPU overlay dialog | "<device name> on <network> wants to control this machine." Shows the requesting device's fingerprint (word list + grouped hex). Approve / Decline equally prominent (UX-002). No data flows until Approve. If the advertised name collides with an already-trusted device (`name_collision`), the prompt adds an informational line: "You already trust a *different* device named <name> — approve only if you recognize this one." The user MAY compare this fingerprint against the connecting machine's own fingerprint (shown in its Settings) out of band; comparison is optional in v1 (approve-on-sight), an accepted-residual-risk decision (see spec FR-006). |
| Waiting for approval (connecting machine) | Overlay on the pending window | "Waiting for approval on <peer>…" (from `LanApprovalPending`), cancelable. |
| Connect picker (connecting machine) | Existing remote picker + "Local network" source | Discovered LAN peers by name, deduped against tailnet peers by identity; transport shown; manual `host:port` entry retained. |
| Transport indicator (controlled window) | Status area | Which path is in use — "Local network" vs "Tailscale" (FR-009). |
| Trusted-network dormant | Settings status + connect flow | Clear "LAN access is on but this network isn't trusted — add it to connect locally." |
| Refusals | Dialog/toast | Distinct copy per `LanRefusal` (below). |

Failure copy (UX-002 → `LanRefusal`):

| Outcome | Copy sketch |
|---|---|
| `Declined` | "<peer> declined this device." |
| `NotTrustedNetwork` | "<peer> isn't accepting local connections on this network." |
| `Disabled` | "Local remote access is turned off on <peer>." |
| `IncompatibleVersion` | "Scribe versions don't match: this machine <x>, <peer> <y>." |
| `Busy` | "<peer> has too many remote connections right now." |
| Connection failure | "Can't reach <peer> on the local network — it may be offline, asleep, or not on this network." |

## Audit log surface (owning machine)

Structured server-log lines (FR-017), reusing 013's audit target:

```text
lan: approved   device=<name> id=<short> network=<label>
lan: declined   device=<name> id=<short>
lan: revoked    device=<name> id=<short>
lan: accepted   device=<name> id=<short> window=<id>
lan: refused    device=<name?> reason=<declined|not-trusted-network|disabled|version|busy>
lan: disconnect device=<name> window=<id?>
lan: dormant    reason=<network-untrusted|disabled>   (bulk, on going dormant)
```
