# Feature Specification: OSC 52 Clipboard Gating

**Feature Branch**: `010-osc52-clipboard-gating`
**Created**: 2026-05-22
**Status**: Draft
**Input**: User description: "OSC 52 clipboard read/write gating with system-clipboard bridge policy"

## Clarifications

### Session 2026-05-22

- Q: What should be the default clipboard policy for OSC 52 operations? → A: Match kitty/iTerm2 — writes silently allowed for both clipboard and primary selection; reads always prompt the user for permission. Full per-mode controls remain available via Scribe settings (per Story 2 and FR-014).
- Q: What should the maximum OSC 52 write payload size be? → A: Default cap is 16 MB. The setting is user-configurable via Scribe settings with an enforced upper bound of 512 MB (matching kitty's `clipboard_max_size` ceiling). The implementation must properly handle payloads up to the configured cap end-to-end — including multi-chunk OSC 52 sequences split across PTY read boundaries — and must reject (rather than silently truncate) anything that exceeds the active cap.
- Q: How granular should the clipboard policy be in the settings UI? → A: Two-axis — a separate "read" mode and "write" mode (each independently set to deny / allow / prompt), applied uniformly to both the clipboard and the primary selection. The four-axis kitty-style split (separate clipboard vs. primary toggles) is deferred as future granularity if a concrete need surfaces.
- Q: How should additional OSC 52 requests from the same pane be handled while a prompt is already open in that pane? → A: Reuse the user's decision for a short burst window. When the user resolves the visible prompt, apply the same answer to any other pending OSC 52 requests of the same operation type (read or write) from that pane. After the prompt resolves, continue applying that decision to fresh same-type requests until the pane goes idle for the burst window (~500 ms with no new request), after which a fresh prompt is required. Cross-pane independence is preserved — each pane has its own prompt and its own burst window. Rationale: kitty's "prompt every single time" UX is widely complained about (kitty discussions #9428, KiTTY #497); reuse-for-burst preserves tmux-style workflows where one user action triggers a flurry of OSC 52 ops without exposing the user to per-byte fatigue.
- Q: Should Scribe additionally adopt the emerging "focus-based gating" pattern (Wave Terminal) where OSC 52 writes silently fail unless the window is focused? → A: Add it as an opt-in setting (default off). Closes the background-clipboard-hijack vector for users who want defense-in-depth, but doesn't break legitimate background-script writes by default. The pattern is not yet adopted by kitty/iTerm2/xterm, so it's positioned as forward-looking rather than mainstream parity.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Block silent clipboard exfiltration by default (Priority: P1)

A program running inside a Scribe terminal pane — whether a local script, a
remote shell over SSH, or a compromised dependency — attempts to silently
exfiltrate the user's clipboard contents by issuing an OSC 52 read query.
Today the running shell receives the full base64-encoded clipboard buffer
back as input, where the script can capture it. After this feature ships,
every OSC 52 read attempt under the default policy surfaces a confirmation
prompt to the user; the program receives no clipboard content unless the
user accepts. Silent reads only happen if the user has opted into them via
settings.

**Why this priority**: This is the highest-severity remaining trust/safety
gap in the modern-terminal audit
(`design/modern-terminal-audit-2026-05-18.md`). Any PTY-side program can
already harvest the clipboard buffer; the data is on the wire today.
Closing this matches the default posture of kitty and iTerm2, which both
prompt the user on every OSC 52 read.

**Independent Test**: With Scribe at default settings and known text in the
clipboard, run a shell command that issues an OSC 52 read query
(`printf '\x1b]52;c;?\x07'`) in a pane that is **not** the focused/visible
pane (or simply do not accept the prompt). Inspect the bytes the shell
sees on stdin: the shell MUST observe either no reply, an empty payload,
or an explicit denial reply — never the actual clipboard contents — until
and unless the user accepts the prompt. No other feature flags or settings
are touched.

**Acceptance Scenarios**:

1. **Given** Scribe is running with its default clipboard policy and the
   user has text in the clipboard, **When** a PTY-side program issues an
   OSC 52 read query for the `c` (clipboard) selection and the user
   dismisses (or does not respond to) the resulting confirmation prompt,
   **Then** the PTY-side program does not receive the clipboard contents.
2. **Given** Scribe is running with its default clipboard policy, **When**
   a PTY-side program issues an OSC 52 read query for the `p` (primary)
   selection and the user dismisses the resulting confirmation prompt,
   **Then** the program does not receive the primary selection contents.
3. **Given** Scribe is running with its default clipboard policy, **When**
   a PTY-side program issues an OSC 52 write to update the clipboard,
   **Then** the operation is honored without prompting (writes remain
   silently allowed by default, matching peer terminals).

---

### User Story 2 - Configurable clipboard policy (Priority: P2)

A user wants finer control over which clipboard operations they trust
PTY-side programs to perform. They open Scribe settings and pick a policy
that matches their threat model: deny all OSC 52 ops; allow writes only
(the default); allow reads only; allow both; or prompt on each operation.
The chosen policy takes effect on the next clipboard request without
requiring a server restart, settings reload, or new terminal session.

**Why this priority**: Power users (security professionals, developers
running untrusted code) and a small set of legitimate read use cases (e.g.,
remote shells that round-trip values through the clipboard) both need a way
to override the default. Without this knob the feature is "secure but
unusable" for valid read flows.

**Independent Test**: Toggle the clipboard policy across "deny",
"write-only", "read-only", "allow", and "prompt". For each setting, issue
both a write and a read from a PTY-side program and confirm the observed
behavior matches the policy without a server restart.

**Acceptance Scenarios**:

1. **Given** the user sets the clipboard policy to "allow", **When** a
   PTY-side program issues an OSC 52 read query, **Then** the program
   receives the current clipboard contents.
2. **Given** the user sets the clipboard policy to "deny", **When** a
   PTY-side program issues an OSC 52 write, **Then** the clipboard is not
   modified.
3. **Given** the user changes the clipboard policy while a session is
   active, **When** the user saves the setting, **Then** the new policy
   applies to the very next OSC 52 operation without restarting the server.

---

### User Story 3 - Confirmation prompt for clipboard requests (Priority: P3)

A user has enabled "prompt" mode for the clipboard policy. A PTY-side
program issues an OSC 52 read or write that would otherwise be blocked.
Scribe displays a confirmation overlay in the affected pane identifying the
operation, the selection target, and — for writes — a head-and-tail
truncated preview of the data. The user picks "Allow once", "Deny", or
"Always allow / Always deny" with default focus on Deny. The PTY-side
program receives no reply until the user decides.

**Why this priority**: Prompt mode is the middle ground between silent
deny and unconditional allow. It is the documented behavior of iTerm2 and
maps cleanly onto kitty's per-operation `clipboard_control` keywords.
Without it, users who want to selectively allow a known operation must
toggle the global policy before and after each use.

**Independent Test**: Set the policy to "prompt". From a PTY-side program,
issue an OSC 52 read; confirm a confirmation overlay appears with Deny as
the default focus and Escape closes it as a deny. Accept once; confirm the
read succeeds. Re-issue the read; confirm a fresh prompt appears (no
implicit "always" decision).

**Acceptance Scenarios**:

1. **Given** the policy is "prompt", **When** a PTY-side program issues an
   OSC 52 read, **Then** a confirmation overlay appears in the originating
   pane, the program receives no reply, and the user can choose Allow,
   Deny, or Cancel.
2. **Given** a clipboard confirmation overlay is visible, **When** the user
   presses Escape or clicks outside, **Then** the overlay closes and the
   OSC 52 operation is denied.
3. **Given** the user selects "Always allow" or "Always deny" from the
   confirmation overlay for a particular operation type, **Then** the
   persisted clipboard policy is updated to reflect the choice and the
   overlay does not re-appear for that operation type in this or
   subsequent sessions.

---

### User Story 4 - Host clipboard bridge for allowed writes (Priority: P3)

A user copies text via a PTY-side program using an OSC 52 write — the
standard way `ssh`-side helpers like `yank`, `tmux set-clipboard`, and many
shell scripts push text out of remote sessions. Today an allowed OSC 52
write does not reach the host operating system clipboard, so the user
cannot paste the value into another application. After this feature ships,
an allowed OSC 52 write also updates the host clipboard, completing the
round-trip the protocol exists to support.

**Why this priority**: The audit explicitly flags this as an open evidence
gap — whether the current isolation between the OSC 52 buffer and the
host clipboard is a deliberate trust boundary or an incomplete bridge.
Closing the gap makes the feature observable in the way users expect from
peer terminals, but it depends on Stories 1 and 2: bridging without gating
would widen the exfiltration surface rather than close it.

**Independent Test**: With clipboard writes allowed by policy, run a
PTY-side program that issues an OSC 52 write of known text. Switch focus
to another application and paste; the pasted text MUST match the value the
PTY-side program wrote.

**Acceptance Scenarios**:

1. **Given** clipboard writes are allowed by policy, **When** a PTY-side
   program issues an OSC 52 write, **Then** the host system clipboard
   reflects the new contents and other applications can paste the value.
2. **Given** clipboard writes are denied by policy, **When** a PTY-side
   program issues an OSC 52 write, **Then** the host system clipboard is
   unchanged.
3. **Given** a clipboard write exceeds the configured maximum size, **When**
   the program issues the write, **Then** the write is rejected, the host
   clipboard is unchanged, and the PTY-side program receives no reply.

---

### Edge Cases

- **OSC 52 fallback chain (`cp`, `pc`, multi-character selection lists)**:
  Each selection in the chain is independently evaluated against the
  policy. The reply uses the first allowed selection or is denied if no
  selection in the chain is allowed.
- **Oversized write payloads**: A buggy or malicious program submits a
  payload larger than the configured size cap. The write is rejected at
  the cap, the host clipboard is unchanged, and the program receives no
  reply.
- **Empty or malformed base64 payloads**: Writes with invalid base64 are
  silently dropped without server error and the host clipboard is
  unchanged.
- **Policy change mid-prompt**: The user changes the clipboard policy
  while a confirmation overlay is open. The current overlay's outcome is
  honored as-is (the change does not retroactively allow or deny it);
  subsequent requests use the new policy.
- **Multi-pane / multi-window**: A confirmation overlay belongs to the
  pane that issued the request and is scoped per pane. Switching panes
  does not dismiss it. Other panes' OSC 52 requests are evaluated
  independently against the policy and surface their own overlays
  rather than auto-denying or queueing behind another pane's prompt.
- **Within-pane burst flurry**: A program issues many OSC 52 ops back
  to back (e.g., tmux on every selection). The first request opens a
  prompt; the user's decision applies to all pending and burst-window
  follow-up requests of the same operation type until the pane goes
  idle (per FR-016, FR-017, FR-018).
- **Focus loss mid-write with focus-gating enabled**: The user enables
  the opt-in "require window focus for writes" setting (FR-019). A
  PTY-side program issues an OSC 52 write. The window holds focus when
  the OSC 52 bytes start arriving but loses focus before the request
  is processed. The write is rejected silently and the host clipboard
  is unchanged. The reverse case (window unfocused at start, focused
  at decision time) is allowed if the policy otherwise permits.
- **Headless or unattached server**: If no GUI client is connected to
  display the prompt when policy is "prompt", the server cannot block
  waiting for a user that does not exist. Requests are treated as denied.
- **Cold-restart / handoff sessions**: After a server restart or upgrade
  handoff, replayed sessions inherit the current configured clipboard
  policy, not a snapshot of the policy that was active when the session
  was first created.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: The system MUST gate OSC 52 read requests from PTY-side
  programs against a user-configurable clipboard policy. The default
  policy MUST prompt the user for confirmation on every read of both
  the clipboard and primary selection (matching kitty's
  `read-clipboard-ask` / `read-primary-ask` posture).
- **FR-002**: The system MUST gate OSC 52 write requests from PTY-side
  programs against the same clipboard policy. The default policy MUST
  silently allow writes to both the clipboard and primary selection
  (matching kitty's `write-clipboard` / `write-primary` posture).
- **FR-003**: When an OSC 52 read is denied by policy, the system MUST
  NOT write any reply containing clipboard contents back to the PTY.
  Empty replies, denial replies, and no reply at all are all acceptable
  observable behaviors.
- **FR-004**: The clipboard policy MUST expose exactly two independent
  axes in the settings UI — a "read" mode and a "write" mode — each
  selectable from {deny, allow, prompt}. Both axes apply uniformly to
  the clipboard selection and the primary selection. The default
  configuration MUST be `read = prompt`, `write = allow`, matching
  kitty's `clipboard_control` default. Splitting the policy further
  across clipboard vs. primary selection is explicitly out of scope
  for v1.
- **FR-005**: In "prompt" mode the system MUST present a confirmation
  overlay to the user before honoring the OSC 52 operation. The default
  focus on the overlay MUST be the deny action, and Escape MUST dismiss
  the overlay as a deny.
- **FR-006**: The confirmation overlay MUST identify the requested
  operation (read vs. write) and selection target (clipboard vs.
  primary). For writes, the overlay MUST show a head-and-tail truncated
  preview of the data being written. For reads, no preview is shown.
- **FR-007**: The user MUST be able to choose "Always allow" or "Always
  deny" from the confirmation overlay; choosing this MUST update the
  persisted clipboard policy for the corresponding operation type.
- **FR-008**: When the policy allows an OSC 52 write, the system MUST
  update the host operating system clipboard so other applications can
  paste the value, subject to FR-009.
- **FR-009**: The system MUST enforce a user-configurable maximum
  payload size for OSC 52 writes. The default cap MUST be 16 MB. The
  user-exposed upper bound on the setting MUST be 512 MB. Writes
  exceeding the active cap MUST be rejected outright without modifying
  the host clipboard and without silently truncating the payload.
- **FR-015**: The system MUST accept OSC 52 write payloads up to the
  active size cap end-to-end, including sequences that arrive split
  across multiple PTY read boundaries. Multi-chunk accumulation MUST
  stop and reject the request once the running total would exceed the
  cap, rather than accumulating an oversize payload before deciding.
- **FR-016**: In prompt mode, when an OSC 52 request arrives for a pane
  while a clipboard confirmation overlay is already visible for that
  same pane, the new request MUST be deferred (not auto-denied or
  shown its own overlay) and MUST inherit the user's decision once the
  visible overlay is resolved. The decision MUST apply only to pending
  requests of the same operation type (read vs. write) from that pane.
- **FR-017**: After the user resolves a clipboard confirmation overlay,
  the system MUST continue to apply that same decision to subsequent
  fresh OSC 52 requests of the same operation type from the same pane
  for a bounded burst window. The burst window MUST close as soon as
  the pane goes idle (no new OSC 52 requests for the bounded duration)
  or after a hard ceiling, whichever comes first. The exact burst
  window duration is set in the plan phase; the spec requires only
  that it be short enough to not span unrelated user-initiated bursts
  and long enough to absorb a single tmux- or shell-script-driven
  flurry of OSC 52 ops triggered by one user action.
- **FR-018**: The burst-decision-reuse mechanism in FR-016 and FR-017
  MUST be scoped per pane. Other panes' OSC 52 requests MUST NOT
  inherit any decision made in another pane and MUST trigger their
  own prompts under the same rules.
- **FR-019**: The system MUST expose a user-toggleable "require window
  focus for clipboard writes" setting, default off. When enabled, OSC
  52 writes from PTY-side programs MUST be silently rejected (no host
  clipboard mutation, no error reply) whenever the originating pane's
  window does not hold the operating-system focus at the moment the
  write is processed. When disabled, OSC 52 writes follow only the
  policy in FR-002 / FR-004 with no focus gate. The setting MUST be
  independent of the read/write policy axes — flipping it MUST NOT
  change the default policy and vice versa.
- **FR-010**: Clipboard policy changes MUST take effect on subsequent
  OSC 52 operations without requiring a server restart, settings reload,
  or terminal-session restart.
- **FR-011**: The system MUST walk OSC 52 fallback-chain selection
  targets (`cp`, `pc`, multi-character lists) entry by entry, applying
  the relevant policy axis (read or write) to each. Since both clipboard
  and primary selection share the same policy axes under FR-004, every
  entry in the chain yields the same decision; the chain semantics
  remain consistent with xterm/kitty interpretation so existing
  client sequences behave as expected.
- **FR-012**: OSC 52 write payloads with invalid base64 MUST be silently
  dropped without server error or state corruption.
- **FR-013**: If no GUI client is connected to render the confirmation
  overlay when the policy is "prompt", the system MUST treat the request
  as denied.
- **FR-014**: The clipboard policy setting MUST be discoverable from the
  Scribe settings UI alongside other terminal trust/safety options.

### Quality, UX, and Performance Requirements

- **QR-001**: Implementation MUST preserve existing architecture
  boundaries and reuse the dialog/overlay infrastructure introduced in
  the OSC 8 disallowed-scheme work (spec 009) for the clipboard
  confirmation overlay rather than introducing a parallel modal
  subsystem.
- **QR-002**: Each user story MUST be verifiable by issuing an OSC 52
  sequence against a running Scribe instance and observing the resulting
  PTY-side reply or host-clipboard state. New automated test code MUST
  be requested explicitly in the plan phase; otherwise verification is
  via manual quickstart.
- **UX-001**: Confirmation overlay visuals, focus order, keyboard
  handling, and copy MUST match the established disallowed-scheme dialog
  so users encounter a single coherent consent-prompt pattern across
  hyperlink and clipboard surfaces.
- **UX-002**: When OSC 52 operations are denied by policy without a
  prompt, the denial MUST be silent to the PTY-side program (no visible
  error in the terminal stream) and MUST NOT pop a user-facing
  notification by default. Denials MAY be exposed via debug logging or a
  status-bar segment that can be inspected on demand.
- **PR-001**: OSC 52 gating decisions in non-prompt policies MUST add no
  observable latency to the existing OSC 52 store/load path
  (sub-millisecond per decision). Prompt-mode latency is bounded by user
  response time and is not in scope for performance targets.

### Key Entities

- **Clipboard policy**: A per-installation setting describing how OSC 52
  operations are handled. Holds exactly two policy axes — a read mode
  (deny/allow/prompt) and a write mode (deny/allow/prompt), applied
  uniformly to both the clipboard and the primary selection — plus a
  maximum write payload size in bytes (default 16 MB, upper bound
  512 MB) and an independent "require window focus for writes" opt-in
  toggle (default off, per FR-019).
- **Clipboard request**: A single OSC 52 operation originating from a
  PTY-side program. Carries the operation type (read or write), the
  selection target (clipboard, primary, or a fallback chain), and — for
  writes — the decoded payload.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: With the default policy and no user acceptance of the
  confirmation prompt, OSC 52 read queries from PTY-side programs
  return no clipboard contents in 100% of cases across local shells and
  SSH-context shells.
- **SC-002**: With clipboard writes allowed by policy, an OSC 52 write
  from a PTY-side program is observable in another application via
  paste within 100 ms of being issued.
- **SC-003**: The clipboard policy can be changed and observed to take
  effect on the very next OSC 52 operation without any restart, reload,
  or session-recreation step, verified across all five policy modes.
- **SC-004**: In prompt mode, the confirmation overlay is visible to the
  user within 100 ms of the OSC 52 request arriving and remains modal to
  the originating pane until the user decides.
- **SC-005**: The audit's highest-severity trust/safety finding for OSC
  52 (read-back exfiltration plus unconditional write) is closed, with
  the remaining audit trust/safety items (multiline-paste confirmation,
  control-character title sanitization, broader URL-open confirmation)
  unaffected.

## Assumptions

- The default policy — silently allow writes, prompt on every read — is
  taken verbatim from kitty's `clipboard_control` default
  (`write-clipboard write-primary read-clipboard-ask read-primary-ask`)
  and matches iTerm2's per-use-consent model. This closes the audit's
  silent-exfiltration finding without breaking the common SSH/yank/tmux
  write-to-clipboard workflows. The "deny reads silently" alternative
  was rejected as more restrictive than peer terminals and as creating
  a steeper UX cliff for legitimate read use cases.
- When writes are allowed by policy, the host operating system clipboard
  is the intended destination of the data. A server-internal-only buffer
  with no host bridge is treated as a non-feature because no user can
  observe it.
- The host clipboard bridge applies to the standard system clipboard
  target on each platform. X11 primary-selection bridging is in scope;
  Wayland primary-selection support and macOS-specific pasteboard
  variants follow the platform's default behavior and are not extended
  in v1.
- User-driven paste, copy-on-select, copy-via-context-menu, and
  AI-aware copy cleanup behavior are untouched. This feature gates the
  OSC 52 protocol path only.
- The confirmation overlay reuses the dialog component established by
  the OSC 8 disallowed-scheme dialog (spec 009); no new modal layer is
  introduced.
- Headless and remote-display scenarios (no GUI client attached to the
  server) are treated as untrusted environments for the "prompt"
  policy: the server cannot block waiting for human input, so prompts
  become denials.
- OSC 52 fallback-chain semantics (`cp`, `pc`, numbered selections
  `c0`-`c7`) follow the xterm and kitty interpretation: each selection
  in the chain is independently policy-evaluated and the first allowed
  selection wins.
- The default maximum OSC 52 write payload size is 16 MB — generous
  enough to cover power-user clipboard text (full code files, large
  diffs, formatted documents) while bounding per-session server memory
  in Scribe's multi-session model. The user-exposed upper bound on the
  setting is 512 MB, matching kitty's `clipboard_max_size` ceiling so a
  user migrating from kitty can preserve their existing limit verbatim.
  The implementation is expected to support the configured cap
  end-to-end (multi-chunk accumulation, no silent truncation, bounded
  memory growth at the cap).
- OSC 52 lacks an error-reply mechanism that PTY-side programs
  observe. Denied writes therefore appear as no-ops to the program,
  matching how peer terminals behave when the same policy denies the
  operation.
- The opt-in focus-based gating in FR-019 is positioned as
  forward-looking defense-in-depth rather than mainstream parity.
  kitty, iTerm2, xterm, and current WezTerm do not implement it;
  newer terminals like Wave Terminal adopt it as a default. Default
  off in Scribe preserves the SSH/yank/background-script
  write-to-clipboard workflows that users expect from peer terminals,
  while offering the modern security posture to users who opt in.
