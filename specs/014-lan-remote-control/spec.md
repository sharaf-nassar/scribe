# Feature Specification: LAN Remote Window Control (without Tailscale)

**Feature Branch**: `014-lan-remote-control`

**Created**: 2026-07-08

**Status**: Draft

**Input**: User description: "Add a way to connect to and control a Scribe window on another machine on the same local network without requiring Tailscale, so users only need Tailscale when away from home. Discover peers on the local network automatically, authorize each new device explicitly the first time it connects and remember it, encrypt the link, prefer the direct LAN path and fall back to Tailscale when off-LAN, off by default, local-first."

This feature extends [remote window control (013)](../013-remote-window-control/spec.md).
It reuses that feature's session protocol, single-controller takeover
model, window controls, and host-action gates unchanged. It adds three
things: a **local-network transport** that needs no Tailscale, **peer
discovery** on the LAN, and **device-approval authorization** to replace
the tailnet-identity check that is unavailable off-tailnet. Where this
spec says "control a window," the behavior is exactly as defined in 013.

## Clarifications

### Session 2026-07-08

- Q: How strong must first-time device pairing be? → A: Approve-on-sight
  (trust-on-first-use) for the device prompt, with the hostile-network
  risk mitigated at a different layer — LAN remote access is only active on
  networks the user has explicitly marked trusted (a new "Trusted Networks"
  list in the remote settings). On an unknown/untrusted network the machine
  is dormant (not discoverable, not listening), so simple device approval
  is safe on the trusted networks where it applies.
- Q: Is LAN access a separate opt-in from the existing Tailscale remote
  access? → A: Separate LAN opt-in — the user can run tailnet-only,
  LAN-only, or both independently.

### Analysis resolutions (2026-07-08)

Cross-artifact analysis surfaced design gaps; resolved as:

- First-connect MITM (FR-006): v1 keeps approve-on-sight. The trusted-network
  gate (FR-018) is the primary mitigation; the approval prompt and each
  machine's Settings both DISPLAY the device fingerprint so a user MAY
  compare out of band, but the compare is optional. The residual
  on-trusted-LAN first-pair MITM risk is explicitly ACCEPTED for v1, with an
  out-of-band verification-code (SAS) / SPAKE2 "secure pairing" mode as a
  documented future hardening.
- "Identity changed" is not a distinct outcome: trust is keyed by device
  identity (FR-005), so a reinstalled peer is a normal unknown device
  requiring fresh approval (US2 #4). The prompt shows an informational
  name-collision hint only.
- LAN↔tailnet dedup (FR-008) is a UX convenience matched by machine
  name/hostname, not cryptographic identity (the two transports use
  different identity namespaces).

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Control a nearby machine with no Tailscale (Priority: P1)

A user at home has two machines on the same local network, neither
signed into Tailscale (or with Tailscale not running). On the connecting
machine they open Scribe, see the other machine offered as a discovered
peer, connect, and control its window exactly as they would over the
tailnet — full state, live control, takeover — with nothing routed
through the internet and no account required.

**Why this priority**: This is the core value — local remote control that
works purely on the LAN, without depending on Tailscale, an account, or
internet connectivity (local-first). Everything else supports it.

**Independent Test**: With Tailscale absent/stopped on both machines and
LAN remote access enabled and the device approved, connect from one to the
other over the local network and drive a window; delivers full value on
its own.

**Acceptance Scenarios**:

1. **Given** two machines on the same LAN with no Tailscale running, LAN
   remote access enabled on machine A, and machine B already an approved
   device, **When** the user on B selects A from the discovered-peer list
   and attaches to a window, **Then** B renders and controls the window
   identically to the tailnet path (per 013), with no internet dependency.
2. **Given** the same setup, **When** the user drives the window (type,
   terminal-app mouse, resize, scroll, search, create/close sessions and
   tabs), **Then** every interaction behaves as it does locally and over
   the tailnet.
3. **Given** A is not discoverable automatically (a network that blocks
   local discovery), **When** the user enters A's local address directly,
   **Then** the connection proceeds the same way.
4. **Given** LAN remote access is disabled on A, **When** B attempts to
   connect, **Then** the attempt is refused before any window or session
   data is revealed, with a clear message, and A is unaffected.

---

### User Story 2 - Approve a new device (Priority: P1)

The first time an unfamiliar device tries to connect to a machine over the
LAN, the owning machine asks its user to approve that specific device
before anything is shared. Once approved, the device is remembered and
connects without prompting again. Approval is deliberate, identifies the
device clearly, and can be declined.

**Why this priority**: On a plain LAN there is no tailnet identity to
authenticate against, so device approval is the entire trust boundary. The
feature is only shippable if this gate is exactly right — as critical as
the connection itself.

**Independent Test**: From an unapproved device, attempt to connect to an
enabled machine; verify the owning machine prompts to approve/decline
before any data flows, that declining refuses the connection, and that
approving both connects and is remembered for next time.

**Acceptance Scenarios**:

1. **Given** LAN remote access is enabled on A and B is an unknown device,
   **When** B attempts to connect, **Then** A shows an approval prompt
   identifying the requesting device before any window or session data is
   revealed, and B is held pending until the user on A decides.
2. **Given** the approval prompt is showing on A, **When** the user on A
   declines, **Then** B's connection is refused with a clear message and B
   is not remembered.
3. **Given** the approval prompt is showing on A, **When** the user on A
   approves, **Then** B connects and is added to A's remembered trusted
   devices, and a later connection from B does not prompt again.
4. **Given** a device was approved earlier, **When** it presents a new
   identity (for example after a reinstall), **Then** A does not silently
   trust it — it is treated as a normal unknown device and must be approved
   again before any data flows, and the approval prompt shows an
   informational hint that an already-trusted device shares its name.
5. **Given** the owning machine A is unattended (no one to answer the
   prompt), **When** B attempts to connect, **Then** B receives a clear
   "waiting for approval / not yet approved" outcome and no data is
   revealed, rather than gaining access while the prompt is unanswered.
6. **Given** the LAN opt-in is on but A is on a network the user has not
   marked trusted, **When** any device attempts to connect (or discover A),
   **Then** A is dormant — undiscoverable and not listening, no approval
   prompt and no data — and only after the user adds the current network as
   trusted does the normal approval flow apply.

---

### User Story 3 - Prefer local, fall back to Tailscale (Priority: P2)

A user moves between home and away. When their two machines are on the
same local network, control uses the direct LAN path automatically. When
they leave and the machines are no longer on the same network, the same
"connect to my machine" action keeps working over Tailscale. The user does
not have to choose a transport by hand.

**Why this priority**: This is the "only need Tailscale when not home"
outcome. It builds on US1 and the existing tailnet path (013) rather than
standing alone, so it is valuable but secondary to getting LAN control and
approval right.

**Independent Test**: With both transports available, connect at home and
confirm the direct LAN path is used; then simulate leaving (peer no longer
on the LAN but reachable over the tailnet) and confirm the same action
connects over Tailscale.

**Acceptance Scenarios**:

1. **Given** a peer reachable both directly on the LAN and over the
   tailnet, **When** the user connects, **Then** the direct LAN path is
   used, and the same peer is not shown or attempted twice.
2. **Given** a peer reachable only over the tailnet (not on the LAN),
   **When** the user connects, **Then** the tailnet path (013) is used.
3. **Given** a peer reachable only on the LAN (no Tailscale), **When** the
   user connects, **Then** the LAN path is used.
4. **Given** an active session over one transport drops because the network
   changed, **When** the peer is still reachable over the other transport,
   **Then** reconnection behavior follows 013's model and the user is told
   which path is in use.

---

### User Story 4 - Review and revoke trusted devices (Priority: P2)

A user can see which devices they have approved for LAN control of a
machine and remove any of them. A revoked device loses access immediately
and must be approved again to reconnect.

**Why this priority**: Approval without revocation is a one-way door;
users need to remove a device they no longer trust (lost, sold, or
mistakenly approved). Important for trust, but the feature is usable before
it while the approved set is small.

**Independent Test**: Approve a device, confirm it appears in the trusted
list, revoke it, and verify it can no longer connect without re-approval
and any active connection from it is ended.

**Acceptance Scenarios**:

1. **Given** one or more approved devices, **When** the user opens the
   trusted-devices list on the owning machine, **Then** each approved
   device is shown with enough detail to identify it (name and approval
   time).
2. **Given** a device is approved and currently connected, **When** the
   user revokes it, **Then** its connection ends promptly and a later
   connection attempt is treated as unknown (must re-approve).

---

### Edge Cases

- Two unknown devices request approval at nearly the same time: each is
  presented distinctly and approved or declined independently; approving
  one does not implicitly trust the other.
- A device on the LAN advertises the same name as the user's machine
  (name collision or impersonation attempt): discovery names are treated
  as hints only; trust is bound to the device's verified identity, not its
  advertised name, so an impersonator with a different identity cannot
  inherit an approved device's trust.
- The local network blocks or does not support automatic discovery: manual
  address entry still works; the feature degrades to direct connection.
- A peer is reachable on both the LAN and the tailnet: it appears once, and
  the direct LAN path is preferred.
- The two machines run different Scribe versions: incompatibility is
  detected at connection time and refused with a message naming both
  versions (same policy as 013); a mismatch never corrupts sessions.
- On a network the user has not marked trusted (e.g. café Wi-Fi): the
  machine stays dormant for LAN access even with the LAN opt-in on — not
  discoverable, not listening (FR-018) — so there is no LAN exposure until
  the user explicitly trusts that network. The tailnet path (013) is
  unaffected and can still be used there.
- The owning machine sleeps or its background service is not running: the
  connecting side reports the peer as unreachable, distinct from
  disabled/not-approved.
- All existing 013 protections (single-controller takeover, dimmed
  lost-control view, host-action gates routed to the controlling machine,
  flow control, audit) apply unchanged over the LAN transport.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: LAN remote access MUST be off by default and only active
  after an explicit opt-in on the machine to be controlled. While off, the
  machine MUST NOT be discoverable and MUST accept no LAN connections.
- **FR-002**: The feature MUST function with no Tailscale, no account, and
  no internet connectivity — discovery, approval, connection, and control
  MUST all work purely on the local network (local-first, Constitution VI).
- **FR-003**: When enabled, the connecting machine MUST be able to discover
  eligible peers on the local network automatically and MUST also allow
  connecting by entering a peer's local address directly.
- **FR-004**: The first connection from a device the owning machine does
  not already trust MUST require explicit approval on the owning machine
  before any window or session data is revealed. Approved devices MUST be
  remembered so subsequent connections do not prompt again.
- **FR-005**: Device trust MUST be bound to a verifiable device identity,
  not to an advertised name or address. A device whose identity does not
  match what the owning machine remembers MUST be treated as unknown
  (re-approval required), never silently trusted.
- **FR-006**: First-time device pairing MUST use approve-on-sight
  (trust-on-first-use): the owning user approves the requesting device from
  the prompt, and the device identity is bound (pinned) on approval so a
  later connection presenting a different identity is a new, unknown device
  (FR-005). The approval prompt MUST display the requesting device's identity
  fingerprint, and each machine MUST expose its own fingerprint (in Settings)
  so a user CAN compare them out of band; this comparison is OPTIONAL in v1.
  The first-connect network-impersonation risk is mitigated primarily by
  FR-018 (LAN active only on user-trusted networks); the residual risk that
  an attacker already present on a trusted LAN could impersonate a device on
  its very first pairing is explicitly ACCEPTED for v1. The design MUST NOT
  preclude a future mandatory out-of-band verification-code (SAS) or SPAKE2
  "secure pairing" mode that closes this window.
- **FR-007**: The LAN link MUST be encrypted end to end between the two
  machines (the local network provides no equivalent to the tailnet's
  transport encryption).
- **FR-008**: When a peer is reachable both directly on the LAN and over
  the tailnet AND is confidently matched as the same machine, the system
  MUST prefer the direct LAN path and present it once. Matching is a
  best-effort UX convenience by machine name/hostname (not a trust check,
  since the LAN device identity and the tailnet identity are different
  namespaces); when a match is not confident, the peer MAY appear once per
  transport, each entry clearly labeled with its transport, and either
  connects correctly.
- **FR-009**: The same "connect to my machine" action MUST use the LAN path
  when the peer is on the local network and fall back to the existing
  tailnet path (013) when it is not, without the user choosing a transport
  by hand; the user MUST be able to see which path is in use.
- **FR-010**: The owning machine MUST let the user review its trusted
  devices and revoke any of them. Revocation MUST end an affected device's
  active connection promptly and force re-approval on its next attempt.
- **FR-011**: Control over the LAN transport MUST reuse feature 013's
  behavior unchanged — single-controller takeover with visible handover and
  one-action reclaim, dimmed lost-control view, full window controls, and
  host-action gates (clipboard, paste confirmation, link opening) routed to
  the controlling machine.
- **FR-012**: LAN remote access MUST be a SEPARATE opt-in from the existing
  Tailscale remote access (013). The user MUST be able to enable
  tailnet-only, LAN-only, or both independently, so tailnet access can be
  kept without making the machine discoverable or connectable on the local
  network.
- **FR-013**: Version compatibility MUST be verified at connection time and
  incompatible pairs refused with a message naming both versions, matching
  013's policy.
- **FR-014**: All failure and refusal modes MUST produce distinct,
  plain-language outcomes: not-approved / pending approval, declined,
  revoked, peer-unreachable, disabled, and version-incompatible.
- **FR-015**: If the device-approval or encryption prerequisites cannot be
  satisfied, the connection MUST be refused (fail closed); local use of
  both machines MUST be unaffected.
- **FR-016**: Disabling LAN remote access MUST take effect promptly: the
  machine stops being discoverable, active LAN connections are ended with
  notice, and no new LAN connections are accepted.
- **FR-017**: The owning machine MUST record LAN-access lifecycle events —
  approvals, declines, revocations, accepted connections, and refusals with
  reason — so the user can audit device access over time.
- **FR-018**: LAN remote access MUST be active only while the machine is on
  a network the user has explicitly marked trusted. On an unknown or
  untrusted network the machine MUST be dormant even when the LAN opt-in is
  on — not discoverable, not listening, accepting no LAN connections — so an
  untrusted network (e.g. café Wi-Fi) presents no LAN attack surface. A
  newly joined network MUST be untrusted until the user marks it trusted.
- **FR-019**: The remote settings surface MUST let the user view trusted
  networks, add the current network as trusted, and remove any trusted
  network. Removing the network the machine is currently on MUST make it
  dormant on that network promptly (as in FR-016).

### Security & Privacy Requirements

- **SEC-001**: No window, session, or trusted-device data may be revealed
  to a connecting device before it is an approved, identity-verified,
  trusted device — the check precedes any 013 attach flow.
- **SEC-002**: Approval is a deliberate, human-in-the-loop action on the
  owning machine; a device MUST NOT gain access by connecting alone.
- **SEC-003**: A machine with LAN remote access disabled MUST be
  indistinguishable on the network from one without the feature (no
  advertisement, nothing listening). The same MUST hold on any network the
  user has not marked trusted, even when the LAN opt-in is on (FR-018).

### UX Requirements

- **UX-001**: Discovery, approval, the trusted-devices list, and the
  transport indicator MUST follow Scribe's established interaction patterns
  and match the 013 remote-control surfaces where they overlap.
- **UX-002**: The approval prompt MUST clearly identify the requesting
  device and MUST make approve and decline equally obvious; if a
  verification code/fingerprint is used (FR-006), it MUST be easy to
  compare across the two machines.
- **UX-003**: Enabling LAN remote access MUST state, at the moment of
  enablement, what becomes possible (the machine becomes discoverable to
  and controllable by devices the user approves on trusted networks).
- **UX-004**: The remote settings surface MUST make the trusted-networks
  list and the "add current network" action clear, and MUST make visible
  whether LAN access is currently active or dormant because the current
  network is not trusted.

### Performance Requirements

- **PR-001**: On a local network, remote typing MUST feel responsive —
  95% of keystrokes visible on the remote display within 100 ms end to end
  (the LAN path SHOULD be at least as responsive as a direct tailnet path).
- **PR-002**: Discovering an eligible peer already on the network MUST
  surface it in the connect UI within 5 seconds of opening it.
- **PR-003**: LAN remote access enabled but idle MUST have no measurable
  impact on local startup, input latency, or rendering.
- **PR-004**: Local sessions MUST be unaffected by a slow or stalled remote
  LAN consumer (same guarantee as 013).

### Key Entities

- **Local Peer**: Another machine running Scribe reachable on the same
  local network; identified for display by a device name and for trust by a
  verifiable device identity; may also be reachable over the tailnet.
- **Device Identity**: A stable, verifiable identity a Scribe install
  presents to peers, used to bind and check trust independent of network
  name or address.
- **Trusted Device**: An approved (device identity, name, approval time)
  record on the owning machine; the LAN trust boundary. Revocable.
- **Approval Request**: A pending first-time connection from an unknown
  device awaiting the owning user's approve/decline decision; reveals no
  session data while pending.
- **LAN Attachment**: A live control relationship over the LAN transport;
  once established, behaves exactly as a 013 remote attachment (single
  controller, takeover, etc.).
- **Transport Selection**: The per-connection choice of direct-LAN vs
  tailnet path for a given peer, preferring LAN when available.
- **Trusted Network**: A local network the user has explicitly marked safe
  for LAN remote access. LAN discovery, listening, and device approval are
  active only while the machine is on a trusted network (FR-018); a newly
  joined network is untrusted by default.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: With LAN remote access enabled and the device already
  approved, and with Tailscale absent on both machines, a user connects to
  and controls a window on a nearby machine in under 30 seconds and at most
  5 interactions — with no internet connectivity present.
- **SC-002**: 100% of connection attempts from unapproved or revoked
  devices are refused before any window, session, or trusted-device data is
  revealed.
- **SC-003**: In side-by-side comparison, LAN-path control renders and
  behaves identically to tailnet-path control (per 013) — same state
  fidelity, takeover, and host-action gate routing.
- **SC-004**: For a dual-reachable peer whose LAN and tailnet entries are
  confidently name-matched, 100% of connections use the direct LAN path and
  the peer appears once; a peer reachable only over the tailnet connects over
  the tailnet, and one reachable only on the LAN connects over the LAN. (When
  names don't confidently match, per-transport listing with clear labels is
  acceptable — see FR-008.)
- **SC-005**: 95% of keystrokes appear on the remote display within 100 ms
  on the LAN, and local sessions on the owning machine show no interaction
  lag while a remote LAN peer is attached or stalled.
- **SC-006**: A revoked device cannot reconnect without a fresh approval in
  100% of attempts, and revocation ends any active connection from it
  within a few seconds.
- **SC-007**: On a network the user has not marked trusted, 100% of LAN
  discovery and connection attempts against the machine find nothing and
  are refused (the machine is dormant), with no approval prompt shown;
  marking the network trusted activates the normal flow.

## Assumptions

- Both machines run a compatible version of Scribe; the LAN transport uses
  the same exact-match protocol-version policy as 013.
- "Local network" means the machines can reach each other directly by local
  address; automatic discovery uses standard local-network service
  discovery, and manual address entry is always available as a fallback.
- A device the user chooses to approve is trusted by the user's deliberate
  action; distinguishing which OS user is behind an approved device is out
  of scope for v1 (approval is per-device, like pairing a peripheral).
- Trust records (approved devices and trusted networks) are stored per
  owning machine and are not synced between machines in v1.
- A newly joined network is untrusted by default; LAN access activates only
  after the user marks a network trusted. Identifying a network well enough
  to remember it as trusted uses standard local-network characteristics.
  Dormancy on leaving a trusted network is enforced promptly by detecting
  network changes (not only on a settings change), so a roam to an untrusted
  network stops advertising and listening without user action (FR-018).
- The LAN owning-side (generating the device identity, showing the approval
  prompt, sealing the private key in the OS keyring) requires an interactive
  desktop session. A headless machine cannot be an owning-side LAN host in
  v1; if the keyring is unavailable, LAN owning-side MUST fail closed with a
  clear message rather than store the key unprotected. The connecting side
  has no such requirement beyond a running Scribe.
- LAN access has its own connection and pending-approval limits, independent
  of the tailnet transport's, so LAN activity cannot exhaust tailnet
  admission and a device left pending approval cannot hold a slot
  indefinitely.
- On macOS, automatic discovery may require the OS "Local Network" permission
  and reading Wi-Fi SSID may be unavailable to a background service; the
  feature degrades to manual address entry and gateway-MAC/subnet network
  identity when those are unavailable.
- The existing 013 tailnet transport, session-continuity model, takeover
  model, and host-action gates are reused unchanged; this feature adds a
  parallel transport, discovery, and authorization only.
- Out of scope (v1): syncing trust across a user's machines, access for
  people other than the owning user's approved devices, browser-based
  access, transports other than direct-LAN and the existing tailnet, and
  wide-area discovery beyond the local network.

## Dependencies

- Builds on **feature 013 (remote window control)** — reuses its session
  protocol, single-controller takeover, window controls, host-action
  gates, flow control, and audit surface. 013 must be present for this
  feature to layer onto.
- Requires the two machines to be on the same local network for the LAN
  path; the tailnet fallback requires Tailscale as in 013 (only when
  off-LAN).
- No account, cloud service, or internet dependency for the LAN path
  (Constitution VI).
