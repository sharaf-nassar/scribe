# Feature Specification: Keyboard Protocol & Command Awareness

**Feature Branch**: `005-keyboard-command-awareness`
**Created**: 2026-05-18
**Status**: Draft
**Input**: User description: "we want to go ahead with 1 and 2 above" — referring to the two
highest-usage missing capabilities from the Modern Terminal Audit: (1) full Kitty keyboard
protocol (CSI-u) outbound key encoding, and (2) command-awareness UI (exit-status visibility
and jump-to-command navigation).

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Protocol-aware apps receive unambiguous keys (Priority: P1)

A developer runs a modern terminal application that negotiates the enhanced keyboard
protocol — an editor, an AI coding tool, a fuzzy finder, a pager, a multiplexer. They press
key combinations the application has asked to receive precisely: `Ctrl`+letter, `Shift`+`Enter`,
`Alt`+key, bare `Esc`, `Ctrl`+`Shift`+key, and keys that should report when held or released.
Every one of those combinations reaches the application exactly as the protocol defines, so the
application's keymaps behave the way its own documentation promises.

**Why this priority**: This is exercised on essentially every keystroke inside a
protocol-aware full-screen application, which is the central workflow for Scribe's
AI-coding-focused audience. Today only one special-cased combination (`Enter` with modifiers)
is encoded correctly; everything else silently degrades to ambiguous legacy bytes, so editor
and AI-tool keybindings misfire constantly. It is the single most continuously hit gap.

**Independent Test**: Run an application that enables the protocol and a logging probe that
prints received byte sequences. Exercise the protocol's defined key+modifier matrix and
confirm each produces the protocol-conformant sequence, including release/repeat when the
application negotiates event reporting. Delivers value alone even if Story 2/3 are not built.

**Acceptance Scenarios**:

1. **Given** an application that has enabled the enhanced keyboard protocol, **When** the user
   presses `Ctrl`+`I`, `Shift`+`Enter`, `Alt`+`.`, and bare `Esc`, **Then** each is delivered
   as the distinct protocol-conformant sequence (no two collapse to the same legacy byte).
2. **Given** an application that negotiated event-type reporting, **When** the user holds and
   then releases a key, **Then** the application receives distinct repeat and release events.
3. **Given** an application that negotiated only a subset of enhancement flags, **When** keys
   are pressed, **Then** only the negotiated behaviors are applied and nothing beyond them.
4. **Given** an application that pushes an enhancement level and later pops it, **When** keys
   are pressed after the pop, **Then** encoding reverts to exactly the prior level.
5. **Given** no application has negotiated the protocol, **When** any key is pressed, **Then**
   the bytes sent are identical to today's legacy behavior (no regression).

---

### User Story 2 - Failed commands are immediately visible (Priority: P1)

A developer with shell integration active runs a sequence of commands. One fails. Without
scrolling back, hovering, or re-running anything, they can see at a glance which command
failed and that the most recent command's outcome was a failure.

**Why this priority**: Shell integration is injected by default, so the shell already reports
command boundaries and exit status and the data already reaches the client — it is then
discarded before display. Surfacing it is high-frequency (every command, every session) and
low-cost (completing existing plumbing, not a new subsystem). Equal P1 with Story 1 because
it is independently shippable and independently valuable.

**Independent Test**: With shell integration on, run a passing command then a failing command.
Confirm the failing command is visually distinguished from the passing one without scrolling
or hovering, and that an unknown/unreported outcome is not shown as a failure. Testable with
no dependency on Story 1 or Story 3.

**Acceptance Scenarios**:

1. **Given** shell integration is active, **When** a command exits non-zero, **Then** that
   command's boundary indicator is visually distinct from successful commands' indicators
   without any scroll or hover.
2. **Given** a command exits zero, **When** it completes, **Then** its indicator uses the
   success treatment (no false-failure styling).
3. **Given** a command completes but no exit status was reported, **When** it ends, **Then**
   it is shown with a neutral/unknown treatment, never as a failure.
4. **Given** scrollback is trimmed or shifted, **When** older content scrolls off, **Then**
   each surviving command's status indicator stays aligned to its originating command row.
5. **Given** two panes run independent command streams, **When** one pane's command fails,
   **Then** only that pane reflects the failure (status is per pane/session).

---

### User Story 3 - Jump straight to commands and failures (Priority: P2)

A developer scrolling a long output history wants to move between command boundaries with the
keyboard, and in particular jump straight to the most recent command that failed, instead of
scrolling or eyeballing.

**Why this priority**: High-value navigation that depends on the command-boundary/status data
made available by Story 2, so it is sequenced after it. Still independently demonstrable once
Story 2 exists and delivers a distinct, frequently used capability (failure triage in large
scrollbacks).

**Independent Test**: In a scrollback containing several commands including at least one
failure, use the keyboard to move to previous/next command boundary and to jump to the most
recent failed command; confirm the viewport lands on the correct boundary every time with no
mouse interaction.

**Acceptance Scenarios**:

1. **Given** a scrollback with multiple commands, **When** the user invokes
   previous-command / next-command navigation, **Then** the viewport moves to the adjacent
   command boundary using the keyboard only.
2. **Given** at least one failed command exists in scrollback, **When** the user invokes
   jump-to-most-recent-failure, **Then** the viewport lands on that failed command in a
   single action.
3. **Given** the cursor is at the newest content, **When** the user requests next-command
   past the last one, **Then** the system stays at a sensible bound (no wrap surprise, no
   crash) consistent with existing scroll-navigation behavior.
4. **Given** no failed command exists, **When** the user invokes jump-to-failure, **Then**
   the system indicates "nothing to jump to" without disrupting the current view.

---

### Edge Cases

- Application negotiates an enhancement level, then a child process pushes a different level
  and later pops it (nested stack) — encoding must always reflect the current top of stack
  and revert cleanly on pop.
- Modifier-only press (e.g., bare `Ctrl`) while the application has requested all keys be
  reported — must encode per the protocol, not be swallowed.
- Bracketed paste, dead keys, and composed input must continue to work unchanged; this
  feature must not regress existing paste/input behavior (composed-input/IME support is out
  of scope and explicitly not addressed here).
- A command that never ends before the next prompt appears, extremely rapid back-to-back
  commands, and re-entrant prompts — boundary/status tracking must not mislabel or drop them.
- Shell integration absent or partial (no boundary or no exit status emitted) — the feature
  degrades to "no indicators / nothing to jump to," never to wrong indicators.
- Scrollback trim removes a command that was the current jump target or carried a status
  indicator — navigation and indicators must remain consistent and not point at stale rows.
- Client disconnect and reattach to a server-owned session, including cold restore and
  zero-downtime upgrade — keyboard-protocol state and command-status indicators must resolve
  to a correct state from the reattached/replayed session, not a misleading one.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: When a terminal application enables the enhanced keyboard protocol, the system
  MUST encode all key events — printable, named, and modified — as protocol-conformant
  sequences for every key-and-modifier combination the protocol defines, not only a single
  special-cased combination.
- **FR-002**: The system MUST report key-release and key-repeat events to the application
  when, and only when, the application has negotiated event-type reporting.
- **FR-003**: The system MUST honor exactly the progressive-enhancement flags the application
  negotiates, including push and pop of nested enhancement levels, reverting precisely to the
  prior level on pop.
- **FR-004**: When no application has negotiated the enhanced protocol, key encoding MUST be
  byte-for-byte identical to current legacy behavior.
- **FR-005**: Enhanced-protocol negotiation state MUST be tracked independently per terminal
  session so concurrent panes at different negotiated levels encode independently and never
  leak state across panes.
- **FR-006**: Users MUST be able to disable the enhanced keyboard protocol via configuration,
  enabled by default, consistent with how other terminal behaviors are configured.
- **FR-007**: The system MUST surface every completed command's success/failure outcome,
  derived from the shell-reported exit status, in the client UI.
- **FR-008**: Failed commands MUST be visually distinguishable from successful commands
  without scrolling or hovering, using a treatment consistent with the existing scrollback
  command-boundary indicator pattern. The scrollbar indicator differentiation MAY be
  color-only, but the colors MUST be theme-derived (the active theme's
  failure/success/neutral palette entries) so they inherit any high-contrast or accessible
  theme the user selects; the non-color authoritative cue is the always-visible status-bar
  indicator (FR-009).
- **FR-009**: The system MUST expose the most recent command's outcome at a fixed,
  always-visible location consistent with existing status-area conventions. This indicator
  MUST carry a non-color glyph cue (distinct success / failure / neutral-unknown symbols),
  serving as the accessible primary signal so the FR-008 scrollbar color is a redundant
  secondary hint, not the sole channel.
- **FR-010**: Users MUST be able to move by keyboard to the previous and next command
  boundary.
- **FR-011**: Users MUST be able to jump directly to the most recent failed command in a
  single action, with a clear, non-disruptive signal when there is none.
- **FR-012**: Commands whose exit status is unknown or unreported MUST be treated and
  displayed distinctly from failures (never shown as a failure).
- **FR-013**: Command-status indicators MUST stay aligned to their originating command rows
  when scrollback is trimmed or shifted, preserving existing indicator-alignment behavior.
- **FR-014**: Command status and command navigation MUST be scoped per pane/session and MUST
  resolve to a correct (non-misleading) state after client reconnect to a server-owned
  session, including cold restore and zero-downtime upgrade.

### Quality, UX, and Performance Requirements

- **QR-001**: Implementation MUST preserve existing architecture boundaries — the
  client/server split, per-session ownership, the existing key-translation priority chain,
  the existing shell-integration command-boundary pipeline, and the existing scrollback
  indicator/alignment mechanism — and MUST reuse those abstractions rather than introduce
  parallel ones, unless a divergence is justified during planning.
- **QR-002**: Each user story's independent verification path is its Independent Test above
  and is satisfied via manual quickstart scenarios. New automated test code is NOT requested
  by this spec (consistent with the project's test-only-on-explicit-request rule and the
  existing manual-quickstart precedent for recent features). The keyboard-protocol
  conformance matrix is noted as a strong future automated-test candidate; adding it requires
  explicit approval during planning and is out of scope here.
- **UX-001**: All new surfaces — command-status indicators, the most-recent-outcome display,
  and the navigation/jump actions — MUST match existing Scribe terminal, scrollbar,
  status-area, and configurable-shortcut conventions; no new visual paradigm, and the jump
  actions MUST be bindable like other Scribe shortcuts.
- **PR-001**: Enhanced key encoding MUST add no perceptible per-keystroke latency (target:
  well under a single 60 fps render frame; no added input lag versus legacy encoding).
  Command-status indicator rendering MUST not measurably reduce scroll or render frame rate
  at the configured scrollback cap.

### Key Entities

- **Key event**: A user keypress as the application should perceive it — logical key,
  active modifiers, physical key identity, event kind (press / repeat / release), and any
  associated text. Distinct from the legacy single-byte view.
- **Keyboard-protocol state**: The set of enhancement flags currently negotiated for a
  terminal session, including the nested push/pop stack, owned per session.
- **Command record**: One shell command's lifecycle as reported by shell integration —
  prompt boundary, command-execution boundary, completion, resulting exit status
  (success / failure / unknown), and its position in scrollback so indicators and
  navigation can locate it after trims/shifts.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: 100% of the enhanced keyboard protocol's defined key-and-modifier combinations
  produce the protocol-conformant sequence when an application negotiates it (today: one
  combination), verified against the published protocol's key matrix.
- **SC-002**: Key-release and key-repeat events are delivered for 100% of qualifying keys
  when event reporting is negotiated (today: 0%).
- **SC-003**: Legacy applications observe a 0% change in received input bytes — the existing
  legacy-encoding behavior is preserved exactly with the feature enabled.
- **SC-004**: A user can identify the most recent command's success or failure in a single
  glance with zero scrolling and zero hovering, 100% of the time when shell integration is
  active.
- **SC-005**: A user can reach the most recent failed command in a 10,000-line scrollback in
  exactly one keyboard action (today: manual scroll/search, typically many actions).
- **SC-006**: Commands with no reported exit status are never displayed as failures (0%
  false-failure rate).
- **SC-007**: After scrollback trim/shift, 100% of surviving command-status indicators remain
  aligned to their originating command row (0 drift), matching existing indicator behavior.
- **SC-008**: Across two or more concurrent panes with different negotiated protocol levels
  and independent command streams, there is 0 cross-pane leakage of key encoding or command
  status.

## Assumptions

- The enhanced keyboard protocol referenced is the published Kitty keyboard protocol and its
  progressive-enhancement levels (disambiguate escape codes, report event types, report
  alternate keys, report all keys as escape codes, report associated text); "conformant"
  means matching that published specification.
- Legacy encoding remains the default transport; the enhanced protocol activates only on
  application negotiation. A configuration opt-out exists and defaults to enabled.
- Scope of "command awareness" for this spec is exit-status visibility plus
  command-boundary / failed-command navigation. Per-command output folding, per-command
  output selection/copy, and command-region grouping UI are explicitly OUT OF SCOPE and
  left to a separable future feature.
- Shell integration (the existing command-boundary mechanism) remains the sole source of
  command boundaries and exit status. Shells or sessions without it simply yield no
  command-status indicators and nothing to jump to — unchanged behavior, never wrong
  indicators.
- New indicators and the most-recent-outcome display reuse the existing scrollback-indicator
  and status-area surfaces; navigation/jump actions reuse the existing configurable-shortcut
  mechanism.
- Decision (scrollbar differentiation channel): color-only differentiation on the ~2px
  scrollbar tick is accepted as a *secondary* affordance because the always-visible
  status-bar indicator (FR-009) carries a non-color glyph as the authoritative cue.
  Scrollbar colors are theme-derived so accessible/high-contrast themes apply. A dedicated
  non-color scrollbar marker is intentionally NOT added (a ~2px tick cannot carry a legible
  glyph; the status bar is the accessible path). This records an explicit decision rather
  than leaving the channel ambiguous.
- Per-pane/session ownership and the existing scrollback trim/shift alignment behavior are
  reused as-is.
- Server-owned session survival semantics are unchanged; on reattach the indicators reflect
  whatever the replayed scrollback provides and protocol state is re-derived from the
  reattached session's terminal mode.
- Other Modern Terminal Audit items (robust scrollback search, OSC 8 hyperlinks, IME,
  inline images, etc.) are explicitly NOT part of this feature.

## Dependencies

- Existing shell-integration injection and command-boundary/exit-status reporting pipeline.
- Existing per-session terminal-mode tracking that already answers enhanced-protocol
  negotiation queries (negotiation is already handled; only outbound key encoding and the
  client-side command-status surfacing are completed by this feature).
- Existing scrollback command-boundary indicator surface and its trim/shift alignment.
- Existing status area and configurable-shortcut systems.
- Existing client/server session protocol and per-session message routing.
