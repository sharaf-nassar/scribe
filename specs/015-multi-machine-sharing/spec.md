# Feature Specification: Multi-Machine Collaborative Window Sharing

**Feature Branch**: `015-multi-machine-sharing`

**Created**: 2026-07-22

**Status**: Draft

**Input**: User description: "Move Scribe beyond the single-controller takeover model (013/014) so multiple machines can attach to and share ONE window simultaneously, tmux/screen -x style: shared live view for every attached machine, input control that passes without disconnecting anyone, and full collaborative typing as the target."

## Clarifications

### Session 2026-07-22

- Q: Input-authorization model for shared windows? → A: A three-value sharing mode chosen in the Remote settings window: "Single controller" (today's exclusive takeover behavior), "Shared view, single typist" (everyone watches live, one control holder types, control passes without disconnecting), and "Collaborative free-for-all" (every attached participant types, tmux/screen -x style).
- Q: Can every machine simply keep its own independent terminal size? → A: Window (pixel) size is independent per machine, but live full-screen programs require one authoritative grid (rows × columns). Decided: smallest-attached-wins grid, with every client rendering that grid responsively inside its own window (centered/padded when larger; regrows when the smallest participant detaches).
- Q: Host-privileged action routing with multiple participants? → A: Acting machine + control holder: paste confirmation and link opening confirm and act on the machine that initiated them (unchanged, per participant); session-initiated clipboard requests route to the current control holder, falling back to the owning machine when no single holder exists (free-for-all) or control is unheld.
- Q: What happens to an active share when the owner changes the sharing mode? → A: The new mode applies immediately: switching to "Shared view, single typist" demotes all participants to viewers with control unheld (claimable); switching to "Single controller" detaches remote participants with an explicit notice (the legacy displaced experience).
- Q: How does a participant obtain input control in single-typist mode? → A: User-configurable via a Remote settings control-acquisition option: "Free claim" (any participant takes control instantly with an explicit action; default) or "Request and grant" (the current holder — or the owner — approves each request). The owning machine can always claim regardless of the option.
- Q: Cap on concurrent participants per shared window? → A: Configurable limit in Remote settings, defaulting to unlimited; when a limit is set, joins beyond it receive the existing busy-style refusal.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Watch a window live from a second machine (Priority: P1)

A user working across machines (desktop at the desk, laptop on the couch) attaches the laptop to a window the desktop is already using. Both machines show the same terminal live. Today this is either/or: the laptop must take the window over, and the desktop is left frozen on a dimmed last frame. With sharing, joining is additive — nobody is displaced, frozen, or disconnected.

**Why this priority**: This is the foundation every other story builds on, and it directly answers the user's original complaint ("only 1 machine can access the UI at a time"). A shared live view alone — even with input still held by one machine — is a complete, valuable MVP.

**Independent Test**: Attach a second machine to an in-use window in shared mode, run a command that produces continuous output on the first machine, and verify both machines render it live while the first machine keeps typing uninterrupted.

**Acceptance Scenarios**:

1. **Given** a window in active use on machine A, **When** machine B joins it in shared mode, **Then** B sees the live terminal and A keeps its live view and input without interruption.
2. **Given** A and B both attached, **When** the running program prints output, **Then** both machines display it in perceived real time.
3. **Given** B is on a slow or stalling network link, **When** heavy output streams, **Then** A and the underlying session experience no slowdown, and B catches up with a brief resync when its link recovers.
4. **Given** A and B both attached, **When** B disconnects or leaves, **Then** A's view and control are unaffected.

---

### User Story 2 - Pass input control without disconnecting anyone (Priority: P2)

In "Shared view, single typist" mode: while both machines watch the same window, the user picks up the laptop and starts driving — input control moves to the laptop, and the desktop keeps watching live instead of being kicked to a frozen "controlled by ..." screen. Later the user sits back down at the desktop and takes control back; the laptop stays attached as a live viewer.

**Why this priority**: Control hand-off is what turns a shared view into a usable multi-machine workflow. It replaces today's disruptive takeover-and-freeze cycle with a fluid role change.

**Independent Test**: With two machines attached, transfer control from one to the other and verify the new control holder's keystrokes reach the session, the previous holder keeps a live view, and transferring back works the same way.

**Acceptance Scenarios**:

1. **Given** A is driving and B is viewing, **When** control passes to B, **Then** B's keystrokes reach the session and A continues to see live output without disconnecting.
2. **Given** control was passed to B, **When** A takes control back, **Then** B remains attached as a live viewer.
3. **Given** B is a viewer without control, **When** B types, **Then** the input is not delivered to the session and B is shown who holds control and how to obtain it.
4. **Given** the control holder's machine disconnects abruptly, **When** remaining participants look at the window, **Then** the session keeps running, no one silently inherits control, and any remaining participant (including the owning machine) can claim it.

---

### User Story 3 - See who is attached and who is in control (Priority: P3)

Any participant — and, always, the machine that owns the window — can see the roster: which devices (and accounts) are attached, who currently holds input control, and notices when someone joins, leaves, or takes control.

**Why this priority**: Presence is what makes sharing trustworthy. The window's owner must never wonder who is watching a terminal that may display sensitive output, and participants need to know who is driving before they act.

**Independent Test**: With three machines attached, verify each shows the same roster, join/leave/control-change notices appear on all of them promptly, and the owning machine can always enumerate every attached device.

**Acceptance Scenarios**:

1. **Given** a shared window, **When** a new machine joins or a participant leaves, **Then** all attached machines see a notice and their roster updates.
2. **Given** a shared window, **When** control changes hands, **Then** all participants see who now holds it.
3. **Given** the owning machine, **When** the user checks the window, **Then** the full list of attached devices and accounts is visible without joining any remote flow.

---

### User Story 4 - Type together in one terminal (Priority: P4)

In "Collaborative free-for-all" mode, like `screen -x` or a shared tmux session: two people-at-machines (the same user, or the user pairing with themselves across desks) both type into the same shell — one runs a command while the other edits the next one — and everyone sees the same screen, cursor, and results.

**Why this priority**: This is the target experience, but it is an extension of stories 1–3 rather than a prerequisite for value. It depends on the input-authorization decision below.

**Independent Test**: With collaborative typing enabled on a shared window, type from two machines in quick alternation and verify both input streams reach the session in arrival order and every participant sees the identical resulting screen.

**Acceptance Scenarios**:

1. **Given** collaborative typing is enabled for a shared window, **When** two participants type concurrently, **Then** both input streams are delivered to the session in the order they arrive and echo to all participants.
2. **Given** collaborative typing is enabled, **When** any participant looks at the window, **Then** all participants see the same screen content and cursor position.

---

### Edge Cases

- Control holder's machine crashes or drops off the network: session keeps running, control is explicitly unowned until claimed, remaining participants are informed.
- A participant's link stalls long enough to overflow its output backlog: that participant alone is resynced with a catch-up replay; no other participant or the session itself is affected.
- A device's trust is revoked (LAN device removed, or transport disabled) mid-share: that participant is ejected immediately; the share continues for everyone else.
- A machine requests exclusive takeover while a share is active: the exclusive claim succeeds only with explicit intent, and every detached participant gets a clear notice — the legacy "displaced" experience applies only to this deliberate action.
- A machine running an older Scribe (pre-sharing protocol) connects: it must get a clean, explicit outcome (legacy single-controller behavior or a clear version refusal) — never a corrupted or half-joined share.
- A participant closes the window or session: closing is a control-holder action; all participants are notified when the window ends.
- The owning machine closes the window or shuts down: the share ends everywhere with notice.
- A participant reconnects after a network drop: it returns to the share as a viewer (or as a typist in free-for-all mode, where every participant types); reconnection must never silently seize single-typist control.
- The owner flips the sharing mode while a control request is pending (request-and-grant): the pending request is cancelled and the requester informed.
- Two participants resize at nearly the same time: the resize policy (below) must produce one deterministic authoritative size with no flapping.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: The system MUST allow multiple approved machines to be attached to the same window concurrently, each receiving the live terminal view.
- **FR-002**: When the sharing mode permits sharing, joining an in-use window MUST be additive: it MUST NOT disconnect, freeze, degrade, or displace any already-attached participant. (This supersedes feature 013's FR-007 "exactly one controller per window".)
- **FR-003**: Explicit exclusive takeover MUST remain available in every mode and keeps its existing semantics; ending an active share this way MUST notify every detached participant.
- **FR-004**: The owning machine MUST offer a sharing mode setting in the Remote settings window with three values governing whose input reaches a shared window's session: **Single controller** (today's exclusive takeover behavior, the default), **Shared view, single typist** (all participants see live output; exactly one control holder types; control passes without disconnecting), and **Collaborative free-for-all** (every attached participant's input is delivered, interleaved in arrival order, tmux/screen -x style).
- **FR-005**: Input control MUST be transferable between attached participants without disconnecting anyone; the previous holder remains a live viewer. How control is obtained in single-typist mode is a Remote settings option: **Free claim** (default — any participant takes control instantly with an explicit action) or **Request and grant** (the current holder, or the owner, approves each request). The owning machine can always claim control regardless of this option.
- **FR-006**: Input from a participant not currently authorized to type MUST be discarded safely, and that participant MUST be shown who holds control and how to request or claim it.
- **FR-007**: The owning machine MUST always be able to reclaim input control and to end sharing — for one participant or all of them — regardless of who currently holds control. Ending sharing for a *single* participant is accomplished by revoking that device's trust or severing its transport (see FR-011); v1 adds no separate owner-initiated eject affordance beyond that path. Ending sharing for *all* participants is accomplished by switching to Single controller (FR-017) or an explicit exclusive takeover (FR-003).
- **FR-008**: Every participant, and always the owning machine, MUST be able to see the roster of attached devices and accounts and the current control holder; joins, leaves, and control changes MUST be announced to all attached machines.
- **FR-009**: A slow or stalled participant MUST NOT degrade the session, the owning machine, or any other participant; a lagging participant is brought current via an individual resync rather than by slowing anyone else down.
- **FR-010**: Only machines that pass the existing remote trust gates (same-user tailnet identity for the tailnet transport; explicit device approval for the LAN transport) may join a share. Sharing MUST NOT introduce any new access path or weaken existing approval, refusal, or revocation behavior.
- **FR-011**: Revoking a device or severing a transport MUST immediately eject the affected participant(s) only; the share continues for the remaining participants.
- **FR-012**: The session MUST have exactly one authoritative terminal grid (rows × columns) at all times, sized smallest-attached-wins: the grid fits the smallest currently-attached participant's viewport and regrows when that participant detaches. Every client MUST render the authoritative grid responsively within its own window (centered/padded when the window is larger), so participants' pixel window sizes remain fully independent and never conflict.
- **FR-013**: Client-initiated privileged actions (paste confirmation, link opening) MUST confirm and act on the machine that initiated them, per participant, exactly as today. Session-initiated clipboard requests MUST route to the current control holder; when no single holder exists (collaborative free-for-all) or control is unheld, they MUST route to the owning machine.
- **FR-014**: Existing workflows MUST keep working unchanged: local single-machine use, and remote single-controller attach/takeover between current-version machines. A version mismatch between sharing-capable and older machines MUST resolve to an explicit, understandable outcome — never silent misbehavior.
- **FR-015**: Joins, leaves, control transfers, ejections, and share endings MUST be recorded in the same audit trail that existing remote-control events use.
- **FR-016**: When the control holder disconnects or is ejected, the session MUST remain running with no participant silently inheriting input control; remaining participants MUST be informed and able to claim control.
- **FR-017**: Changing the sharing mode MUST take effect immediately for active shares: switching to "Shared view, single typist" demotes all participants to viewers with control unheld; switching to "Single controller" detaches remote participants with an explicit notice (the legacy displaced experience). Every affected participant is informed of what changed.
- **FR-018**: The owning machine MUST offer a configurable participant limit per shared window in Remote settings, defaulting to unlimited. When a limit is set, joins beyond it MUST be refused with the existing busy-style refusal and MUST NOT disturb the active share.

### Key Entities

- **Window Share**: The state of one window being available to multiple machines at once — its participants, its current control holder, and its authoritative terminal size.
- **Participant**: One attached machine's membership in a share — the device identity and account shown in rosters, its role (viewing or driving), and its individually-managed output stream.
- **Control Holder (Driver)**: The participant (or, per FR-004's outcome, participants) whose input currently reaches the session. Distinct from mere attachment.
- **Sharing Mode**: The owning machine's Remote setting — Single controller, Shared view/single typist, or Collaborative free-for-all — governing how joins and input authorization behave for its windows.
- **Presence Event**: A join, leave, control change, or ejection announcement delivered to all participants and reflected in rosters.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: A second machine can join an in-use window and see live output within 2 seconds, with zero interruption (no freeze, no disconnect, no dropped keystrokes) on the first machine.
- **SC-002**: With three machines attached to one window, all three display the same output in perceived real time, and the control holder's keystroke echo latency adds no more than 5 ms over the single-machine baseline (typing experience indistinguishable from single-machine use).
- **SC-003**: Passing control between two attached machines completes in under 1 second and disconnects no one.
- **SC-004**: A participant whose network stalls entirely causes no measurable slowdown for the session, the owning machine, or other participants, and catches up automatically within 2 seconds of its link recovering.
- **SC-005**: The owning machine can enumerate every attached device at any moment, and rosters on all machines reflect a join or leave within 1 second.
- **SC-006**: All existing single-controller flows (local use, remote attach, takeover, reclaim) behave identically to the previous release when sharing is not used.
- **SC-007**: 100% of share membership changes (join, leave, control transfer, ejection) appear in the audit trail.

## Assumptions

- All participants are the same user's machines inside the existing trust domains (same-user tailnet identity, or LAN devices the user explicitly approved). Sharing with other people/accounts is out of scope for this feature.
- Two participant roles are sufficient: viewer and control holder; in collaborative free-for-all mode every participant is effectively a control holder. No finer-grained permission tiers in v1.
- Typical shares are small — two to five machines. The design does not need to target large audiences or broadcast scenarios.
- A brief catch-up resync (momentary screen replacement) is an acceptable experience for a participant recovering from a slow link, matching existing remote behavior.
- The sharing mode is chosen on the owning machine in Remote settings and defaults to Single controller, so no existing flow becomes shared unless the user changes the setting; connecting machines join according to the owning machine's mode.
- Control-gated interactions beyond typing (closing the session or window, search, focus reporting) follow the control holder role rather than being granted to all viewers; in Collaborative free-for-all mode, where no single control holder exists, these fall to the owning machine (the always-present local participant).
- The owning machine retains ultimate authority over its windows at all times; sharing never creates a state the owner cannot unilaterally end.
