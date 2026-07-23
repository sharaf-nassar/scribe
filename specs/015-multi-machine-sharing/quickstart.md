# Quickstart: Multi-Machine Collaborative Window Sharing — Validation Guide

**Feature**: `015-multi-machine-sharing` | **Date**: 2026-07-22
**Spec**: [spec.md](spec.md) | **Data model**: [data-model.md](data-model.md)
**Protocol**: [contracts/remote-protocol-v3.md](contracts/remote-protocol-v3.md)

Manual two/three-machine validation scenarios, mapped to user stories (US1–US4)
and success criteria (SC-001…SC-007). No implementation code. Each scenario
references the data-model and contract rather than restating message shapes.

## Prerequisites

- Two or three machines that already satisfy the 013/014 setup: same-user
  tailnet identity (tailnet transport) and/or explicit LAN device approval
  (LAN transport). Verify a plain 013/014 remote attach works first.
- All machines run the same sharing-capable build (`REMOTE_PROTOCOL_VERSION = 3`).
  Keep one older (v2) build available for the legacy-compatibility scenario.
- Machine **A** is the **owning machine** (owns the window under test). Machines
  **B** (and **C**) join it.
- Set A's Remote settings per each scenario:
  `sharing_mode`, `control_acquisition`, `participant_limit`
  (`crates/scribe-common/src/config.rs`, `RemoteConfig`; applied live, no restart).
- Do NOT restart the Scribe server at any point (repo policy; all settings apply
  live via `ConfigReloaded`).

Roles below: **owner** = A; **participant** = B/C.

---

## Scenario 1 — US1: shared live view (SC-001)

**Setup**: A running a window with a continuously-printing command (e.g. a log
tail). A's `sharing_mode = Shared view, single typist`.

**Steps**
1. On B, open the connect picker and join A's in-use window (additive join,
   `Hello { takeover: false }`; see data-model participant lifecycle).
2. On A, keep typing / keep the output streaming throughout.

**Expected**
- B shows the live terminal within 2 s (SC-001); A is never frozen, dimmed,
  disconnected, or made to drop a keystroke (FR-002).
- Both machines render new output in perceived real time (US1 scenario 2).
- A `ShareRoster` appears on both (A + B listed); A remains control holder
  (or unheld — see Scenario 3/4). This is the MVP: shared view even before
  control passes.

---

## Scenario 2 — stalled-participant isolation (SC-004, FR-009)

**Setup**: Continue from Scenario 1 (A + B attached, heavy output streaming).

**Steps**
1. Throttle or briefly sever B's network link (e.g. drop B's Wi-Fi) while A keeps
   producing heavy output past B's 4 MiB queue (`REMOTE_OUTPUT_QUEUE_BYTES`).
2. Restore B's link.

**Expected**
- A and the underlying session show **zero** measurable slowdown while B is
  stalled (SC-004) — the PTY reader never back-pressures (per-participant
  `RemoteSink`, research D5).
- On recovery, B catches up within 2 s via a single resync (`drop_pty_backlog`
  → `send_resync_replay`), not by slowing anyone (US1 scenario 3).
- No other participant is affected.

---

## Scenario 3 — US2: pass control in single-typist mode (SC-003, SC-002)

**Setup**: A + B attached, `sharing_mode = Shared view, single typist`,
`control_acquisition = Free claim` (default). A currently holds control.

**Steps (free claim)**
1. On B, take control (the claim affordance → `ControlClaim`, contract).
2. Type on B; then on A, take control back; type on A.

**Expected**
- B's keystrokes reach the session; **A stays attached as a live viewer** — not
  kicked to a frozen "controlled by…" screen (US2 scenarios 1–2, FR-005).
- Control pass completes in under 1 s, disconnecting no one (SC-003).
- A `ShareRoster` with the new `holder` reaches both machines (D8).
- While B holds control, A typing is discarded and A is shown who holds control
  and how to claim it (FR-006, US2 scenario 3).

**Request-and-grant variant**
1. Set A's `control_acquisition = Request and grant` (live).
2. On B (viewer), request control (`ControlRequest`).
3. On the holder (or A/owner), approve (`ControlGrant { accept: true }`).

**Expected**
- The holder/owner sees a `ControlRequested` notice; on approve, control moves to
  B and roster updates; on deny, B sees `ControlDenied` and control is unchanged.
- A (owner) can always claim directly regardless of the setting (FR-007).

**Holder-loss variant (FR-016)**
1. With B holding control, kill B's machine / drop it abruptly.

**Expected**
- Session keeps running; control becomes **unheld** (no silent inheritance);
  remaining participants (incl. A) are informed and any can claim (US2 scenario 4,
  FR-016).

**Control-holder echo latency, three machines (SC-002)**
1. Attach a third machine C so A + B + C are all in the single-typist share; give
   control to one holder (say B).
2. Type steadily on the holder (B) and measure keystroke→echo latency against a
   single-machine baseline (holder typing with no other participants attached).

**Expected**
- All three machines display the holder's output in perceived real time, and the
  holder's echo latency adds no more than **5 ms** over the single-machine
  baseline — typing is indistinguishable from single-machine use (SC-002). This
  is the single-typist measurement of SC-002's control-holder echo proxy;
  Scenario 5 covers the free-for-all real-time-parity side.

---

## Scenario 4 — US3: roster / presence with three machines (SC-005)

**Setup**: A + B + C, any shared mode.

**Steps**
1. Join C to A's window.
2. Pass control among the machines.
3. Leave C.
4. On A, check the window's attached-device list.

**Expected**
- Every machine shows the same roster; join/leave/control-change notices appear on
  all within 1 s (SC-005, FR-008) via full-state `ShareRoster` broadcasts.
- A (owner) can enumerate every attached device at any moment without joining any
  remote flow (US3 scenario 3, FR-008) — the local owner is always a share member
  (data-model).

---

## Scenario 5 — US4: free-for-all interleaved typing (SC-002)

**Setup**: A + B (+ C) attached, `sharing_mode = Collaborative free-for-all`.

**Steps**
1. Type from A and B in quick alternation (one runs a command while the other
   edits the next).

**Expected**
- Both input streams reach the session in arrival order and echo to all
  participants (US4 scenario 1, FR-004).
- All participants see identical screen content and cursor position (US4
  scenario 2); with three machines attached, all three show the same output in
  perceived real time and the typing experience is indistinguishable from
  single-machine use (SC-002).

---

## Scenario 6 — smallest-wins grid + regrow (FR-012)

**Setup**: A + B attached (shared mode). A's window is large; B's is smaller.

**Steps**
1. Join B with a small window; observe the authoritative grid.
2. Detach B; observe regrow.
3. Resize A and B at nearly the same instant.

**Expected**
- The grid shrinks to B's smaller `min(rows, cols)` (data-model
  `AuthoritativeGrid`); A renders that grid centered/padded inside its larger
  window (FR-012); B shows it full.
- When B detaches, the grid regrows to A's size.
- Near-simultaneous resizes settle to one deterministic size (250 ms debounce,
  min-over-snapshot) with no flapping (spec Edge Case).

---

## Scenario 7 — mode change applies immediately (FR-017)

**Setup**: An active share with A + B (+ C).

**Steps**
1. With participants attached, on A switch `sharing_mode`:
   a. → `Shared view, single typist`.
   b. → `Single controller`.

**Expected**
- (a) All participants demote to viewers, control unheld/claimable; every
  participant informed (roster) (FR-017).
- (b) Remote participants are detached with the legacy displaced notice
  (`WindowTakenOver` + `ShareEnded { reason: ModeChangedToSingleController }`);
  A retains sole control (FR-017, FR-003).
- Any pending control request is cancelled and the requester informed (spec Edge
  Case).

---

## Scenario 8 — participant limit refusal (FR-018)

**Setup**: A sets `participant_limit = 1` (one remote participant). B already
attached.

**Steps**
1. Attempt to join C.

**Expected**
- C's join is refused with the busy-style refusal (`RemoteRefusal::Busy` /
  `LanRefusal::Busy`, contract); the active A+B share is undisturbed (FR-018).
- The local owner (A) is never counted against the limit (FR-007).

---

## Scenario 9 — legacy compatibility (SC-006, FR-014)

**Single-controller parity**
1. Leave A's `sharing_mode = Single controller` (default). Perform a 013-style
   remote attach and takeover/reclaim between two current-version machines.

**Expected**
- Behavior is identical to the previous release: takeover freezes the displaced
  machine under the banner, reclaim works, local single-machine use is
  pixel-identical (SC-006).

**Old-version remote**
1. Point the older (v2) build at A (v3) and attempt to connect.

**Expected**
- A clear `IncompatibleVersion` refusal naming both versions — never a corrupted
  or half-joined share (FR-014, contract compatibility matrix); or, against a v2
  server with no listener, a distinct "unreachable/disabled" outcome.

**Exclusive takeover ends an active multi-participant share (FR-003)**
1. Set A's `sharing_mode = Shared view, single typist` (or `Collaborative
   free-for-all`) and attach B and C so an active three-participant share is
   running (A + B + C).
2. From a fourth machine D (or from B via an explicit takeover action), issue an
   exclusive claim — `Hello { takeover: true }` — against the shared window.

**Expected**
- The exclusive claim succeeds only on explicit intent and ends the share for
  **every** attached participant: A, B, and C are each detached and shown the
  legacy displaced notice (`WindowTakenOver`), and the claimer holds sole control
  — no participant is left silently half-attached (FR-003, spec Edge Case; see
  data-model shared-mode `Hello { takeover: true }` transition row).

---

## Scenario 10 — host-privileged action routing (FR-013)

**Setup**: A + B attached, shared mode, B holds control.

**Steps**
1. On B, trigger a paste-confirm and a link-open.
2. From the session, trigger an OSC 52 clipboard write.
3. Repeat the OSC 52 write with control unheld, and again in free-for-all mode.

**Expected**
- Paste confirmation and link opening confirm and act on the **acting machine**
  (B), per participant, exactly as today (FR-013).
- Session-initiated OSC 52 prompt routes to the **control holder** (B); with
  control unheld or in free-for-all, it routes to the **owning machine** (A)
  (FR-013, research D7).

---

## Scenario 11 — trust-gate on join + revoke mid-share (FR-010, FR-011)

**Setup**: A + B attached (shared mode); B holds control. Have an
unapproved / never-approved device **D** available (no same-user tailnet
identity, no explicit LAN device approval).

**Steps**
1. From D, attempt to join A's shared window.
2. With A + B still sharing, on A revoke B's device (or sever B's transport)
   mid-share.

**Expected**
- D's join is refused by the existing 013 WhoIs / 014 device-approval gate —
  sharing opens no new access path, so an unapproved or revoked device can never
  join a share (FR-010); the active A+B share is undisturbed.
- Revoking B ejects **only** B immediately (`ShareEnded` / roster update); the
  share continues undisturbed for every remaining participant (FR-011). Because B
  held control, the holder-loss transition applies — control becomes unheld and
  claimable, with no silent inheritance (FR-016).

---

## Audit spot-check (SC-007, FR-015)

After the scenarios, inspect A's server audit log: every join, leave, control
transfer, ejection, and share-end must appear on the existing 013 remote-audit
surface (100% of membership changes, SC-007).
