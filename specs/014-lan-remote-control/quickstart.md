# Quickstart: LAN Remote Window Control

**Feature**: `014-lan-remote-control` | **Date**: 2026-07-08
Manual validation on two machines on the same local network. See
[contracts/](./contracts/) for exact message/UI contracts. Reuses 013's
control/takeover behavior — this guide focuses on the LAN-specific surfaces:
discovery, device approval, the trusted-network gate, and transport
selection.

## Prerequisites

- Two machines (**A** = owning, **B** = connecting) on the same LAN.
- **Tailscale stopped/absent on both** for the core scenarios — this proves
  local-first (no account/internet). Re-enable only for the fallback test.
- Both run a build with features 013 + 014, matching remote protocol version.
- Note A's LAN address, its device name, and (after first enable) its device
  fingerprint shown in Settings.

## Scenario 1 — First pairing + control, no Tailscale (US1+US2, SC-001/002)

1. On A: Settings → Remote → Local network → enable. **Expect** a status
   line; if A is on an untrusted/new network, it reads "dormant — network
   not trusted."
2. On A: Trusted Networks → "Add current network." **Expect** it's added
   (label from SSID or gateway MAC); status flips to "active." (If A has only
   a VPN/tunnel route, "Add current" is disabled with a note — connect A to
   the real LAN.)
3. On B (Tailscale off): open the connect picker → "Local network." **Expect**
   A appears as a discovered peer within ~5 s (PR-002), by name; manual
   `host:port` entry also works.
4. On B: attach to A. **Expect** A shows an **approval prompt** naming B and
   B's fingerprint; B shows "Waiting for approval on A…"; no window content
   appears on B yet (SEC-001).
5. On A: the prompt shows B's device fingerprint (optionally compare it
   against B's own fingerprint shown in B's Settings — an out-of-band MITM
   check that is optional in v1), then **Approve**. **Expect** B immediately
   renders and controls A's window (per 013 — full state,
   type/scroll/resize/takeover), with no internet connectivity present. A
   logs `lan: approved` + `lan: accepted`.
6. On B: disconnect and reconnect. **Expect** no prompt this time (B is
   remembered); connects straight through.

## Scenario 2 — Untrusted-network dormancy (US2, FR-018, SC-007)

1. On A: move to a different network the user has NOT trusted (or remove the
   current network from Trusted Networks). **Expect** A's LAN status → "dormant."
2. From B on that same network: browse/connect. **Expect** A is **not
   discoverable** and not listening — `nmap`/`ss`/`lsof` shows nothing on the
   LAN port, no approval prompt, no data (SEC-003). This is the café-Wi-Fi
   case: zero LAN surface.
3. On A: add the current network as trusted. **Expect** A becomes
   discoverable and the normal approval flow returns.

## Scenario 3 — Decline & reinstalled device (US2, security)

1. From an unapproved device, attempt to connect to A. On A: **Decline**.
   **Expect** the device is refused, nothing revealed, not remembered; A logs
   `lan: refused reason=declined`.
2. Reinstall B (or reset B's device key) so it presents a new identity, and
   connect using B's old name. **Expect** A does NOT silently trust it — it
   appears as a new **unknown** device requiring fresh approval, and because
   the name matches an already-trusted device the prompt shows an
   informational "you already trust a different device named B" hint. Never a
   silent accept.

## Scenario 4 — Revoke (US4, SC-006)

1. On A: Settings → Trusted Devices → verify B is listed with its fingerprint
   and approval time.
2. With B connected, **Revoke** B. **Expect** B's connection ends within a
   few seconds; a later connect from B triggers a fresh approval prompt (`lan:
   revoked` logged).

## Scenario 5 — Prefer LAN, fall back to Tailscale (US3, SC-004)

1. Bring Tailscale back up on both A and B (both on the same LAN AND tailnet).
   **Expect** A appears **once** in B's picker, and connecting uses the
   **direct LAN** path (transport indicator shows "Local network").
2. Move B off the LAN (but reachable over the tailnet). **Expect** the same
   "connect to A" action now uses the **Tailscale** path (013), indicator
   shows "Tailscale."
3. Back on the LAN with Tailscale still up: **Expect** LAN is preferred again,
   still one entry.

## Performance checks (PR-001/003/004, SC-005)

- **Keystroke latency (PR-001)**: in a B-controlled shell on the LAN, sample
  ≥20 keystrokes (echo-timestamp or 240 fps capture): p95 ≤100 ms.
- **Idle overhead (PR-003)**: LAN enabled + on a trusted network but no
  connection — compare A's local input latency / server CPU against
  LAN-disabled baseline: no measurable difference (a dormant browse + periodic
  network-fingerprint poll only).
- **Stalled consumer (PR-004)**: attach from B, freeze B's client, generate
  sustained output on A: A's local windows stay fluid and the LAN
  connection's memory stays bounded (reuses 013's queue ceiling).

## Exit checklist

- Scenarios pass on Linux↔Linux and (if hardware available) macOS↔Linux;
  note skipped legs. On macOS confirm no unexpected Location/TCC prompt from
  the network-fingerprint read, and that discovery works after granting Local
  Network access.
- (Optional) Fingerprint shown on A's approval prompt matches B's own
  fingerprint in B's Settings — the out-of-band identity check (optional in
  v1; the trusted-network gate is the primary MITM mitigation).
- Audit log shows approved/declined/revoked/accepted/refused/dormant lines
  matching the actions performed (FR-017).
- `lat.md` updated (protocol, server, client, settings) and `lat check`
  passes before completion.
