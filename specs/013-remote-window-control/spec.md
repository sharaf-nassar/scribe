# Feature Specification: Remote Window Control over Tailscale

**Feature Branch**: `013-remote-window-control`

**Created**: 2026-07-03

**Status**: Draft

**Input**: User description: "we want to add a way to connect remotely to a scribe window and control it from another machine on our tailscale network. we can build this into scribe itself as the other machine will also have scribe installed. ensure to research what is the standard and best approach for this"

Pre-spec research on the standard approach (prior art, Tailscale-native
patterns, security posture) is recorded in [research.md](./research.md). The
industry-standard posture it establishes — remote access as an opt-in,
tailnet-only capability authenticated per connection by tailnet identity,
with the private network's built-in encryption as the transport trust
boundary — is assumed throughout this spec.

## Clarifications

### Session 2026-07-03

- Q: When machine B attaches to a window currently open in a client on
  machine A, what is the control model? → A: Takeover now, shared later —
  v1 ships single-controller takeover (remote attach detaches the local
  client with a visible handover and one-action reclaim); shared
  simultaneous control is an explicit named future extension the v1 design
  must not preclude.
- Q: What does the local client on machine A display while one of its
  windows is remotely controlled? → A: Dimmed frozen view — the window keeps
  showing the content from the moment control transferred, visually dimmed,
  under a prominent banner naming the controlling device with a one-action
  "take back control"; no live output flows to the side that lost control.
- Q: Can the remote user create a new window on the owning machine, or only
  attach to existing windows? → A: Attach + create new — the remote user can
  also start a brand-new window (essential when the owning machine has zero
  windows, e.g. after a reboot); same trust level as takeover.
- Q: After a network interruption, how does the remote client reconnect? →
  A: Automatically, with visible status — retry with backoff showing a clear
  "reconnecting" state (cancelable), converging to current state when the
  link returns.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Take control of a window from another machine (Priority: P1)

A user who has enabled remote access on their main machine sits down at a
second machine on the same Tailscale network, opens Scribe, chooses the main
machine, sees its windows described well enough to pick the right one, and
attaches. The window's complete state appears — every workspace, tab, pane,
scrollback history, and running session — and they continue working exactly
where they left off, with keystrokes, mouse interaction in terminal apps,
resizing, scrolling, and search all behaving as they would locally.

**Why this priority**: This is the entire point of the feature — continuing
work on live terminal sessions (including long-running AI agent sessions)
from any of the user's machines without losing state.

**Independent Test**: Enable remote access on machine A which has a window
with active sessions; from machine B, connect, attach to that window, run
commands, scroll history, and resize. Delivers full value on its own.

**Acceptance Scenarios**:

1. **Given** remote access is enabled on machine A and one of its windows has
   running sessions with scrollback, **When** the user on machine B selects
   machine A and attaches to that window, **Then** machine B renders the
   complete window state (all workspaces, tabs, panes, scrollback to the
   configured depth, cursor position, and screen modes) identical to what a
   local reattach on machine A would show.
2. **Given** machine B controls the window, **When** the user types, uses a
   full-screen terminal app with mouse support, resizes the window, scrolls,
   or searches scrollback, **Then** each interaction behaves as it would for
   a local client, and running programs observe the resize.
3. **Given** machine B controls the window, **When** a session produces
   output continuously, **Then** the remote view streams live and stays
   interactive (the user can still interrupt the running program).
4. **Given** remote access is disabled on machine A, **When** machine B
   attempts to connect, **Then** the attempt is refused before any window
   information is revealed, machine B shows a clear message naming the
   possible causes (machine offline, Scribe not running, or remote access
   turned off — indistinguishable by design per FR-001/FR-004), and nothing
   changes on machine A.
5. **Given** the window is currently open in a client on machine A,
   **When** machine B attaches to it, **Then** control transfers to machine
   B: the client on machine A stops accepting input for that window and
   shows the content frozen as of the transfer, visually dimmed, under a
   prominent banner naming the controlling device with a single-action way
   to take control back — never a silent takeover, and no live output
   continues to flow to the side that lost control.
6. **Given** machine A is reachable with remote access enabled but has no
   windows (for example freshly rebooted), **When** the user on machine B
   connects, **Then** they can create a new window on machine A and begin
   working in it immediately.

---

### User Story 2 - Opt-in enablement and authorization (Priority: P1)

Remote access does not exist on the network until the user deliberately turns
it on. A user enables it in Scribe's settings on the machine to be
controlled, is told plainly who will be able to connect, and can turn it off
again at any moment with immediate effect. Only the user's own machines —
devices on the same private Tailscale network belonging to the same tailnet
account — are authorized; everything else is refused.

**Why this priority**: A terminal exposes everything the user can do on the
machine. The feature is only shippable if its security posture is exactly
right from the first release; enablement and authorization are therefore as
critical as attach itself.

**Independent Test**: On a fresh install, verify no remote exposure exists;
enable remote access and verify a same-account device can connect while a
device belonging to a different tailnet account is refused; disable and
verify active remote connections are severed immediately.

**Acceptance Scenarios**:

1. **Given** a fresh or upgraded Scribe installation, **When** the user takes
   no action, **Then** Scribe accepts no remote connections and exposes
   nothing on any network.
2. **Given** the user opens the settings surface for remote access,
   **When** they enable it, **Then** the setting explains in plain language
   who will be able to connect (their own devices on the same Tailscale
   network) before taking effect.
3. **Given** remote access is enabled on machine A, **When** a device on the
   same tailnet but owned by a different account attempts to connect,
   **Then** the connection is refused before any window or session data is
   revealed, and the refusal is recorded with the peer's identity.
4. **Given** remote access is enabled on machine A, **When** a device
   without a user identity (for example a tagged server node) attempts to
   connect, **Then** it is refused by default.
5. **Given** machine B is actively controlling a window on machine A,
   **When** the user disables remote access on machine A, **Then** machine
   B's connection is severed within 2 seconds, machine B states that remote
   access was disabled on machine A (a delivered sever notice, not an
   inference), and machine A's sessions continue running unaffected.
6. **Given** the Tailscale service on machine A is stopped or unavailable,
   **When** any machine attempts a remote connection, **Then** the
   connection is refused (fail closed) and local use of Scribe on machine A
   is completely unaffected.

---

### User Story 3 - Return to working locally (Priority: P2)

After controlling a window remotely, the user comes back to the machine that
owns the sessions and resumes working on that window locally without losing
any state, scrollback, or running programs. The remote side is informed that
control has moved rather than being left with a stale or misleading view.

**Why this priority**: Remote control is a round trip — the feature is
incomplete if coming home means hunting for sessions or discovering a stale
remote client silently eating input. It builds on Story 1 rather than
standing alone.

**Independent Test**: While machine B controls a window, reclaim it on
machine A; verify full state is present locally and machine B clearly shows
it no longer controls the window.

**Acceptance Scenarios**:

1. **Given** machine B controls a window whose sessions live on machine A,
   **When** the user reclaims that window on machine A, **Then** the local
   client shows the complete current state including everything done
   remotely, and running programs are undisturbed.
2. **Given** control has returned to machine A, **When** machine B's user
   looks at their Scribe, **Then** it shows the same losing-side treatment
   as any takeover — content frozen at the moment of transfer, dimmed,
   under a banner naming the current controller with a one-action reclaim —
   rather than a stale interactive view.

---

### User Story 4 - Survive network interruptions (Priority: P3)

Laptops sleep, Wi-Fi drops, and network paths between the two machines
change. When the link between the machines is interrupted, the sessions keep
running untouched on the owning machine; when the remote machine regains
connectivity, the user can get back to an accurate, current view without
duplicated or corrupted content.

**Why this priority**: Resilience polish on top of the core flows —
important for daily trust, but the feature is viable while reconnection is
manual.

**Independent Test**: While controlling a window remotely, forcibly cut the
network between the machines, generate output in a session, restore the
network, reconnect, and verify the view converges to the true current state.

**Acceptance Scenarios**:

1. **Given** machine B controls a window, **When** the connection between
   the machines drops for any reason, **Then** all sessions keep running on
   machine A exactly as if a local client had disconnected.
2. **Given** the link dropped while sessions kept producing output,
   **When** connectivity returns, **Then** machine B reconnects
   automatically and the rebuilt view matches the sessions' true current
   state — no missing final state, duplicated regions, or interleaved stale
   frames.
3. **Given** a sustained interruption, **When** machine B cannot reach
   machine A, **Then** machine B shows a clear, cancelable
   "reconnecting" state naming the peer rather than appearing frozen, and
   settles into a plain disconnected state only if the user cancels.

---

### Edge Cases

- Two of the user's machines attempt to attach to the same window at nearly
  the same time: exactly one obtains control; the other receives a clear
  refusal naming who holds control.
- The two machines run different Scribe versions: incompatibility is
  detected at connection time and refused with a message naming both
  versions and the remedy; a version mismatch must never corrupt sessions or
  produce a silently wrong view.
- A session floods output (for example a large build log) while the link to
  the remote controller is slow or stalled: sessions and other windows on
  the owning machine are unaffected, the owning machine's resource use for
  that remote link stays bounded, and the remote view catches up to the
  current state rather than replaying an ever-growing backlog.
- The remote client crashes or the machine sleeps mid-control: identical
  outcome to a clean disconnect — sessions continue, the window becomes
  attachable again.
- The owning machine is offline, asleep, or Scribe's background service is
  not running: the connecting side reports the combined connection-failure
  outcome (offline / not running / remote access off — FR-004), which is
  distinctly worded from "not authorized" and other typed refusals.
- Clipboard interactions while remotely controlled: programs reading or
  writing the clipboard through terminal capabilities interact with the
  controlling machine's clipboard, governed by the existing clipboard
  policy including its prompts; pasting on the controlling machine is
  subject to that machine's paste-confirmation setting.
- A window with no connected client anywhere (for example after the owning
  machine's UI was closed, sessions still alive): remote attach works the
  same as a local reattach would.
- The user closes the window or quits Scribe from the remote side: the
  outcome is identical to performing the same action locally, and it is
  treated as a deliberate action on live sessions.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: Remote access MUST be off by default and MUST only activate
  after an explicit, deliberate user action on the machine to be controlled.
  While off, Scribe MUST NOT listen on or expose anything over any network.
- **FR-002**: When enabled, remote connectivity MUST be reachable only over
  the machine's private Tailscale network — never over the local LAN, other
  interfaces, or the public internet.
- **FR-003**: Every incoming connection MUST be authenticated to a tailnet
  identity before any window or session information is revealed. The default
  authorization policy MUST be: only devices belonging to the same tailnet
  account as the owning machine are accepted; devices without a user
  identity and devices of other accounts MUST be refused.
- **FR-004**: The connecting machine MUST let the user reach a peer machine
  by its network name, SHOULD list the user's own eligible machines for
  selection, and MUST present distinct, actionable outcomes for:
  not-authorized, identity-unavailable, version-incompatible, busy, and
  connection-failure. Because a machine with remote access disabled exposes
  nothing on the network (FR-001), a cold connection attempt cannot
  distinguish "disabled" from "unreachable": the connection-failure message
  MUST name all three possibilities (machine offline, Scribe not running,
  or remote access turned off). A live connection severed by disablement
  MUST report the disabled cause distinctly (FR-016).
- **FR-005**: After authorization, the connecting user MUST be able to see
  the owning machine's windows with enough context to choose the intended
  one (such as workspace names, session counts, and whether a window is
  currently in use), and MUST be able to create a new window on the owning
  machine — including when it currently has none.
- **FR-006**: Attaching MUST deliver the window's complete state — all
  workspaces, tabs, and panes, scrollback to the configured depth, screen
  contents, cursor, and terminal modes — matching what a local reattach on
  the owning machine would show.
- **FR-007**: Exactly one controller MUST hold a window at any time
  (takeover model). Attaching from another machine MUST transfer control
  with a visible handover on the side losing control — never silently — and
  the losing side MUST be offered a single-action way to reclaim control.
  Either machine can take control back at any time. The side that lost
  control MUST display the window's content frozen as of the transfer,
  visually dimmed, under a banner naming the current controller; live
  output MUST NOT continue streaming to a non-controlling client in v1.
  The v1 design MUST NOT preclude a future shared simultaneous-control
  mode (explicitly out of scope for v1; see Assumptions).
- **FR-008**: While controlling a window, the remote user MUST be able to
  perform the full set of window interactions available to a local client —
  typing, terminal-app mouse interaction, resizing, scrolling, scrollback
  search, and creating, closing, and rearranging sessions, tabs, and panes
  within that window.
- **FR-009**: The owning machine MUST make it visible (a) that remote access
  is enabled, and (b) when a window is under remote control, including which
  device and account holds control.
- **FR-010**: Remote connect, disconnect, or failure MUST NOT terminate,
  restart, or corrupt any session; a remote disconnect MUST leave sessions
  in the same state as a local client disconnect.
- **FR-011**: After an interruption, the remote client MUST attempt
  reconnection automatically with backoff, showing a clear cancelable
  reconnecting status, and on success MUST converge to the sessions' true
  current state without duplicated, missing, or stale content. If a
  different controller took the window during the interruption, automatic
  reconnection MUST NOT seize control back; the reconnecting client
  presents the standard lost-control state (FR-007) and control moves only
  on an explicit user action.
- **FR-012**: Version compatibility MUST be verified when a connection is
  established. Incompatible pairs MUST be refused with a message naming both
  versions and the remedy; compatible version skew MUST NOT change or
  corrupt behavior. (Protocol-compatibility decision required by the
  constitution.)
- **FR-013**: A slow or stalled remote link MUST NOT degrade sessions,
  rendering, or input handling for local use of the owning machine, and the
  owning machine's per-connection resource use MUST stay bounded by catching
  the remote view up to current state instead of accumulating backlog.
- **FR-014**: All existing data-protection gates MUST apply with the
  controlling machine treated as the data endpoint: clipboard capability
  policy (including prompts and size limits) governs session clipboard
  access bridged to the controlling machine's clipboard; paste confirmation
  is evaluated on the controlling machine; links open on the controlling
  machine.
- **FR-015**: If tailnet identity cannot be verified (for example the
  Tailscale service is down), remote connections MUST be refused (fail
  closed) while local operation continues unaffected.
- **FR-016**: Disabling remote access MUST take effect within 2 seconds of
  the setting applying: active remote connections are severed with a
  delivered notice naming the cause on the remote side and the standard
  indicators clearing on the owning side, and no new connections are
  accepted.
- **FR-017**: The owning machine MUST record remote-access lifecycle events
  — accepted connections with peer identity, refusals with reason, and
  disconnects — so the user can audit who connected and when.

### UX Requirements

- **UX-001**: The connect flow, indicators, and settings surface MUST follow
  Scribe's established interaction patterns — settings live in the existing
  settings UI, status indicators match existing status-bar language, and
  confirmations match established dialog chrome.
- **UX-002**: Every failure mode (connection failure with its combined
  offline / not-running / disabled explanation, live-disable sever,
  unauthorized, identity unavailable, busy, version mismatch, control taken
  elsewhere) MUST produce a distinct, plain-language, actionable message.
- **UX-003**: Enabling remote access MUST present, at the moment of
  enablement, a plain-language statement of who will be able to connect.

### Performance Requirements

- **PR-001**: With a direct network path between the machines, remote typing
  MUST feel responsive: 95% of keystrokes visible on the remote display
  within 100 ms end-to-end. Over relayed paths the session MUST remain
  usable, degrading in latency only.
- **PR-002**: Attaching to a typical window (up to 8 sessions at default
  scrollback) over a direct path MUST reach first full render within 2
  seconds.
- **PR-003**: Remote access enabled but idle MUST have no measurable impact
  on local startup, input latency, or rendering.
- **PR-004**: Local sessions MUST show no measurable slowdown attributable
  to a stalled or slow remote consumer (verified while a session produces
  sustained multi-MB/s output).

### Key Entities

- **Remote Peer**: One of the user's machines on the private Tailscale
  network, identified by device name and owning tailnet account; may be
  reachable or unreachable; runs its own full Scribe.
- **Remote Access Policy**: Per-machine state on the owning side — the
  enable/disable switch plus the authorization rule (same-account devices
  only by default) that every incoming connection is checked against.
- **Remote Attachment**: A live control relationship between a peer's client
  and exactly one window on the owning machine, from establishment through
  release, reclaim, or disconnection. Exactly one attachment controls a
  given window at any moment (FR-007).
- **Window** (existing concept): The unit a remote peer attaches to; owns a
  tree of workspaces, tabs, and panes whose sessions live on the owning
  machine.
- **Session** (existing concept): A running terminal process owned by the
  owning machine's background service; its lifetime is independent of any
  local or remote client.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: With remote access already enabled, a user starting from
  Scribe freshly opened on a second machine reaches full control of the
  intended window in under 30 seconds and at most 5 interactions.
- **SC-002**: In side-by-side comparison, 100% of remote attaches render
  state equivalent to a local reattach — same scrollback depth, screen
  content, cursor, and layout (verifies FR-006).
- **SC-003**: 95% of keystrokes appear on the remote display within 100 ms
  over a direct path, and local sessions on the owning machine drop zero
  output and show no interaction lag while a remote peer is attached or
  stalled (verifies PR-001, PR-004, FR-013).
- **SC-004**: Zero session terminations or state corruption across at least
  100 consecutive attach/detach/interruption cycles, including forced
  network drops mid-output (verifies FR-010, FR-011).
- **SC-005**: 100% of unauthorized connection attempts — different account,
  identity-less device, remote access disabled, or identity service down —
  are refused before any window or session data is revealed (verifies
  FR-003, FR-015).
- **SC-006**: Whenever a window is remote-controlled, the owning machine
  displays controller identity for the entire duration — including for
  windows that never had a local client (for example remotely created ones)
  — and in a check with three first-time viewers, each locates the
  indicator unprompted (verifies FR-009).

## Assumptions

- Both machines have Scribe installed and are signed into the same Tailscale
  network; connect-by-name uses the network's standard device naming.
- The private network's built-in encrypted transport and device identity are
  the trust boundary; no additional transport-level secrecy is layered on in
  this feature's first release. This matches the accepted standard for
  tailnet-native services, including Tailscale's own remote-access products
  (see [research.md](./research.md)).
- Authorization is at machine-account granularity: a multi-user host shares
  one tailnet identity, and distinguishing OS users on the connecting
  machine is documented as out of scope for v1.
- v1 targets the user's own devices (same tailnet account). Granting other
  people access is a future extension with its own policy surface.
- Versions on the two machines are expected to be close but not identical;
  FR-012 makes skew explicit rather than assumed away.
- Machines are expected to be reachable most of the time; discovery of
  powered-off peers or wake-on-LAN is out of scope.
- Out of scope (v1): browser-based access, access from machines without
  Scribe, transports other than the private Tailscale network, file
  transfer, port forwarding, and latency-masking predictive echo (noted in
  research as a possible later enhancement).
- Shared simultaneous control — two machines mirroring and typing into the
  same window at once — is an explicit, named future extension (per
  clarification). It is out of scope for v1, but v1's design must not paint
  itself into a corner that makes it impossible.

## Dependencies

- The Tailscale service must be installed, running, and signed in on both
  machines; when it is not, the feature is cleanly unavailable (fail closed)
  rather than degraded.
- Relies on Scribe's existing session-continuity model — sessions owned by a
  background service that survive client disconnects, and full-state
  restoration on attach.
- [research.md](./research.md) records the surveyed prior art and the
  recommended architecture direction for the planning phase.
