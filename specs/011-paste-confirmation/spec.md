# Feature Specification: Paste Confirmation (Multiline / Control-Character)

**Feature Branch**: `011-paste-confirmation`
**Created**: 2026-05-28
**Status**: Draft
**Input**: User description: "Multiline / control-character paste confirmation —
an opt-in terminal setting (disabled by default) that pops a confirmation
dialog before risky pasted content reaches the PTY, so a user does not blindly
run commands they have not read." Sourced as the highest-leverage remaining
Trust & Safety item from `design/modern-terminal-audit-2026-05-18.md`, chosen
to continue the dialog-gated trust arc started by the shipped OSC 8 (spec 009)
and OSC 52 clipboard gating (spec 010) features, and reusing the same GPU
confirmation-dialog infrastructure.

## Clarifications

### Session 2026-05-28

- Q: Which pastes should trigger the confirmation dialog? → A: **Multiline or
  control characters.** Confirm when the pasted content contains at least one
  line break (`\n`/`\r`) OR at least one other control/escape byte (any byte
  `< 0x20` except tab/LF/CR, plus DEL `0x7f` and the C1 range `0x80–0x9f`).
  This matches the iTerm2 / kitty / GNOME Terminal baseline and covers both
  the auto-execute foot-gun and the escape-injection / social-engineering
  vector the audit cites.
- Q: Should the confirmation still fire when the focused application has
  bracketed-paste mode active? → A: **No — match modern shell behavior.**
  Bracketed-paste-aware apps (zsh, fish, bash 4.4+, vim, etc.) have explicitly
  opted into safe paste handling: pasted content waits at the prompt as
  literal input and does not auto-execute. The feature defers to that. The
  confirmation fires **only when bracketed-paste mode is off** — the genuinely
  dangerous legacy case (raw REPLs, `cat`, old shells, apps that do not
  negotiate bracketed paste).
- Q: What is the default state of the setting? → A: A new opt-in boolean
  terminal setting, **disabled by default**, per explicit user request. When
  disabled, paste behavior is byte-for-byte identical to today with no added
  latency.
- Q: Which paste entry points are gated, and what does the confirmation offer?
  → A: Gate every path that sends clipboard/selection content to the PTY — the
  paste keybinding, the right-click context-menu "Paste", and middle-click
  primary-selection paste. The dialog reuses the existing GPU confirmation
  overlay with two actions, **Cancel** (default focus, Esc-bound) and
  **Paste**, and a readable preview of the flagged content. The confirmation
  is allow/deny only — it never alters the pasted bytes. Drag-and-drop file
  insertion is out of scope (already shell-quoted, a distinct vector).

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Catch an accidental multi-line paste before it runs (Priority: P1)

A user copies a block of text — a multi-line snippet from a web page, chat, or
an AI assistant's reply — and pastes it into a shell or REPL that has *not*
enabled bracketed paste. Today every embedded newline submits a command the
user may not have read. With the setting enabled, a confirmation dialog appears
first, showing how many lines would run and a readable preview, with **Cancel**
focused by default. The user reads, then chooses **Paste** to proceed or
**Cancel** to abort — nothing reaches the shell until they decide.

**Why this priority**: This is the anti-foot-gun the feature exists for and the
audit's stated reason ("pasting multi-line text into a shell auto-executes
commands the user may not have read"). It is the single largest user-facing
payoff and the foundation US2 and US3 build on. It is independently shippable
and independently valuable on its own.

**Independent Test**: Enable the setting. In an application with bracketed
paste off (e.g., a bare `cat`, an old `sh`, or a REPL without bracketed-paste
support), paste a three-line block. Confirm the dialog appears *before* any
line executes, shows the line count and a preview, and that **Cancel** aborts
with zero bytes sent while **Paste** sends all three lines exactly as copied.

**Acceptance Scenarios**:

1. **Given** the setting is enabled and the focused app has bracketed paste
   off, **When** the user pastes content containing one or more line breaks,
   **Then** a confirmation dialog appears before any byte is sent, stating the
   reason (line count) and showing a readable preview, with **Cancel** as the
   default-focused, Esc-bound action.
2. **Given** the confirmation dialog is open, **When** the user chooses
   **Paste** (or presses Enter while Paste is focused), **Then** the original
   content is delivered to the PTY byte-for-byte identically to how it would
   have been pasted with the feature off.
3. **Given** the confirmation dialog is open, **When** the user chooses
   **Cancel** or presses Esc, **Then** the paste is aborted and zero bytes
   reach the PTY.
4. **Given** the setting is enabled and the focused app has bracketed paste
   *on*, **When** the user pastes the same multi-line content, **Then** no
   dialog appears and the content is pasted exactly as today (deferring to the
   app's bracketed-paste handling).
5. **Given** a single-line paste with no control bytes (e.g., a long URL or an
   API token), **When** the user pastes it into an unbracketed app, **Then**
   no dialog appears and the content is pasted directly.

---

### User Story 2 - Catch hidden control/escape characters in a paste (Priority: P2)

A user pastes text that *looks* like a harmless single line but carries hidden
control or escape bytes — a smuggled `ESC` sequence, embedded `\r` cursor
tricks, or other non-printing bytes from a crafted web page or untrusted
output. With the setting enabled (and bracketed paste off), the same
confirmation fires for control bytes even without a line break, and the preview
renders those bytes visibly (caret notation) so the user can see what is
actually in the clipboard before sending it.

**Why this priority**: This is the trust / anti-social-engineering half of the
audit item. It is sequenced after US1 because it broadens the same classifier
and reuses the same dialog US1 builds, but it delivers distinct value: a
single-line paste that hides escape sequences is exactly the case a naive
"multiline only" check would miss.

**Independent Test**: Enable the setting. In an unbracketed app, paste a
single-line string that contains an embedded `ESC` (e.g. a copied terminal
control sequence). Confirm the dialog still appears, the preview shows the
control byte in caret notation (e.g. `^[`) rather than emitting it raw, and the
choice still gates delivery.

**Acceptance Scenarios**:

1. **Given** the setting is enabled and bracketed paste is off, **When** the
   user pastes single-line content that contains a control/escape byte (other
   than tab), **Then** the confirmation dialog appears and names control
   characters as the reason.
2. **Given** the preview renders content that contains control/escape bytes,
   **When** the dialog is shown, **Then** those bytes are displayed in visible
   caret notation and are NOT emitted raw into the dialog's render path.
3. **Given** a paste whose only non-printing characters are tab(s), **When**
   the user pastes it, **Then** tabs alone do NOT trigger the dialog (tabs are
   common, legitimate whitespace).
4. **Given** a paste containing both line breaks and control bytes, **When**
   the dialog appears, **Then** the reason reflects both conditions and the
   preview makes the content legible.

---

### User Story 3 - Discoverable, live, and uniform across paste sources (Priority: P3)

A user finds the toggle on the Terminal settings page, reads plain-language
helper text explaining when it intervenes, and turns it on. It takes effect on
their next paste with no restart, and it behaves identically no matter how they
paste — keyboard shortcut, right-click "Paste", or middle-click
primary-selection paste.

**Why this priority**: Discoverability and consistency are necessary for an
opt-in safety feature to be trusted and used, but they layer on top of the core
gate (US1/US2). Sequenced last because the gate must exist before there is
anything to expose or to apply uniformly.

**Independent Test**: Open the Terminal settings page, confirm the toggle is
present, labeled, and off by default. Turn it on; without restarting, paste
risky content via the keybinding, then via the context menu, then via
middle-click — confirm the dialog appears for all three. Turn it off; confirm
the dialog stops appearing on the next paste.

**Acceptance Scenarios**:

1. **Given** a fresh configuration, **When** the user opens the Terminal
   settings page, **Then** the paste-confirmation toggle is present, has
   plain-language helper text, and is **off** by default.
2. **Given** the user flips the toggle on, **When** they next paste risky
   content (no restart), **Then** the confirmation fires; **and when** they
   flip it off, **Then** the next paste sends directly with no dialog.
3. **Given** the setting is enabled, **When** the user pastes risky content via
   the paste keybinding, the right-click "Paste" item, and middle-click
   primary-selection paste, **Then** the confirmation fires identically for all
   three entry points.
4. **Given** the setting is enabled and the user middle-click-pastes a
   primary-selection larger than the single-message paste limit, **When** they
   confirm, **Then** the content is delivered correctly (chunked as needed)
   with no truncation.

---

### Edge Cases

- **Single trailing newline only** (e.g. `ls\n`): the trailing `\n` counts as a
  line break — in an unbracketed app it would submit the command, so it
  triggers the confirmation. (In a bracketed app it is deferred — no dialog.)
- **Tabs / printable single line**: a single line with no control bytes other
  than tabs does NOT trigger (e.g. a pasted token, path, or URL).
- **Empty clipboard / empty paste**: no dialog; nothing is sent.
- **Very large paste** (multi-megabyte): classification and preview MUST stay
  responsive; the preview is truncated with an indication of total size; on
  **Paste**, the existing chunking path applies; parked content MUST be
  released when the dialog resolves (no retained-clipboard memory growth).
- **Bracketed-paste mode changes between clipboard read and send**: the
  bracketed-paste state captured at paste-request time governs the decision;
  behavior is deterministic.
- **Focus change / originating pane closed while the dialog is open**: the
  dialog captures input so focus cannot change via keyboard; if the originating
  session is gone when the user confirms, the paste is dropped safely — no
  crash and no delivery to a different pane.
- **Setting flipped off while a dialog is already open**: the open dialog still
  resolves normally (the in-flight decision is honored); only subsequent pastes
  are affected.
- **Another modal dialog already open**: paste gating follows the existing
  one-modal-at-a-time dialog conventions; behavior is documented and does not
  crash or stack incorrectly.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: The system MUST add an opt-in boolean terminal setting
  (`terminal.paste_confirmation`) that defaults to **disabled**. While
  disabled, paste behavior MUST be identical to today — no classification, no
  dialog, no added latency.
- **FR-002**: While enabled, before pasted clipboard content is sent to the
  PTY, the system MUST present a confirmation dialog if and only if BOTH (a)
  the focused application does NOT have bracketed-paste mode active, AND (b) the
  content contains at least one line break (`\n` or `\r`) OR at least one other
  control/escape byte (any byte `< 0x20` other than tab/LF/CR, DEL `0x7f`, or
  the C1 range `0x80–0x9f`).
- **FR-003**: While enabled, when the focused application HAS bracketed-paste
  mode active, the system MUST NOT present the dialog and MUST paste exactly as
  today, regardless of content (deferring to the app's bracketed-paste
  handling).
- **FR-004**: A line break anywhere in the content — including a single
  trailing newline — MUST satisfy the "multiline" condition. Tab characters
  MUST NOT, on their own, satisfy the "control character" condition.
- **FR-005**: The confirmation dialog MUST state why the content was flagged
  (e.g. the number of lines for multiline content, and/or the presence of
  control characters) and MUST show a readable preview of the content. Control
  and escape bytes in the preview MUST be rendered in visible caret notation
  (e.g. `ESC` as `^[`) and MUST NOT be emitted raw into the dialog's render
  path.
- **FR-006**: The dialog MUST offer exactly two actions — **Paste** (proceed)
  and **Cancel** (abort) — with **Cancel** as the default focus and Esc-bound,
  matching the existing Close / Update / disallowed-scheme dialog conventions;
  Enter activates the focused action.
- **FR-007**: Choosing **Paste** MUST deliver the original, unmodified content
  through the normal paste path (including bracketed-paste wrapping where
  applicable and chunking for large payloads). The confirmation MUST NOT alter,
  reorder, truncate, or sanitize the bytes that are actually pasted.
- **FR-008**: Choosing **Cancel** or dismissing via Esc MUST abort the paste
  entirely — zero bytes reach the PTY.
- **FR-009**: When the setting is enabled, the gate MUST apply uniformly to
  every paste entry point that sends clipboard or selection content to the PTY:
  the paste keybinding, the right-click context-menu "Paste", and middle-click
  primary-selection paste. No entry point may bypass the gate.
- **FR-010**: While the dialog is open it MUST capture input like the existing
  modal dialogs — keystrokes and clicks MUST NOT leak to the underlying PTY —
  and the parked paste content MUST remain associated with its originating
  session/target so confirming delivers to the correct pane.
- **FR-011**: The setting MUST be exposed on the Terminal settings page as a
  labeled toggle with plain-language helper text (including that it defers to
  bracketed-paste-aware apps), defaulting to off, and changes MUST take effect
  on the next paste with no client or server restart, consistent with other
  terminal toggles.
- **FR-012**: The classifier MUST treat clipboard content as opaque data and
  MUST NOT execute, interpret, open, or transform any part of it; its only
  output is "send directly" versus "confirm first".
- **FR-013**: Drag-and-drop file/path insertion MUST remain unchanged and MUST
  NOT be gated by this feature.
- **FR-014**: Copy-on-select, the existing copy paths, and OSC 52 clipboard
  writes (already gated by spec 010) MUST be unaffected by this feature.

### Quality, UX, and Performance Requirements

- **QR-001**: Implementation MUST preserve existing architecture boundaries and
  reuse existing abstractions — the GPU confirmation-dialog overlay
  (Close / Update / disallowed-scheme family), the client paste pipeline, and
  the terminal-config ⇄ settings-webview round-trip — rather than introduce
  parallel ones. The clipboard-paste and primary-selection-paste paths SHOULD
  converge through a single gated sender so the confirmation cannot be bypassed
  by one path, rather than duplicating the gate; any divergence MUST be
  justified during planning.
- **QR-002**: Verification is via manual quickstart scenarios, one per user
  story, consistent with the project's test-only-on-explicit-request rule and
  the manual-quickstart precedent set by specs 005, 007, 008, and 009. The pure
  paste classifier (content → needs-confirmation decision) is the natural
  unit-test seam; adding automated tests requires explicit approval during
  planning and is out of scope here.
- **UX-001**: The dialog chrome, button conventions, keyboard handling, and the
  settings toggle MUST match existing Scribe dialog and settings patterns — no
  new visual paradigm. The settings helper text MUST make the bracketed-paste
  deferral understandable in plain language so users do not perceive the
  feature as broken when it stays silent at a normal modern-shell prompt.
- **PR-001**: When enabled, classification MUST be linear in paste length with
  no perceptible delay before the dialog appears for typical pastes; when
  disabled, the classifier path MUST be skipped entirely with zero added cost.
  Parked paste content MUST be released when the dialog resolves so there is no
  retained-clipboard memory growth.

### Key Entities

- **Paste request**: A single paste in flight — its target session/pane, the
  raw content to be pasted, and the focused app's bracketed-paste state captured
  at request time. (The triggering entry point — keybinding, context menu, or
  primary selection — is not retained; it does not affect classification or
  delivery.)
- **Paste risk classification**: The decision derived from a paste request's
  content and bracketed-paste state — whether it contains a line break, whether
  it contains a non-tab control/escape byte, and the resulting
  "needs confirmation" verdict, plus a short human-readable reason (line count,
  control-character presence) for display.
- **Paste confirmation dialog**: The modal that parks a paste request and its
  classification while awaiting the user's choice. Presents the reason, a
  legible (caret-escaped) preview, and exactly two actions with **Cancel**
  default-focused; on resolve it either resumes delivery of the parked content
  unchanged or drops it.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: With the setting enabled and bracketed paste off, 100% of pastes
  containing a line break or a non-tab control/escape byte present the
  confirmation before any byte reaches the PTY (today: 0% — paste is sent
  unconditionally).
- **SC-002**: Choosing **Paste** delivers byte-identical content to what would
  be pasted with the feature off — 0 byte differences across multiline, large
  (>4 KiB), and bracketed-wrapping cases.
- **SC-003**: Choosing **Cancel** (or Esc) results in 0 bytes reaching the PTY
  in 100% of cases.
- **SC-004**: With bracketed-paste mode active, 0 confirmation dialogs appear —
  the feature defers to the shell, matching modern-shell behavior.
- **SC-005**: With the setting disabled (default), paste behavior and latency
  are unchanged — 0 dialogs and no measurable added latency versus today.
- **SC-006**: The gate covers 100% of paste entry points (paste keybinding,
  context-menu "Paste", and middle-click primary-selection paste) — no route
  bypasses it when enabled (today, primary-selection paste is a separate code
  path).
- **SC-007**: Toggling the setting takes effect on the next paste with no
  client or server restart in 100% of cases.
- **SC-008**: Control/escape bytes shown in the preview are rendered in caret
  notation in 100% of cases — 0 raw control bytes are emitted into the dialog
  render path.

## Assumptions

- The setting is **off by default** per explicit user request; it is an opt-in
  safety net, not a default behavior change.
- "Match modern shell behavior" means deferring to bracketed paste: conformant
  shells/TUIs (bash 4.4+, zsh, fish, vim, etc.) enable bracketed-paste mode,
  which already prevents pasted newlines from auto-executing, so the
  confirmation is unnecessary there and fires only when bracketed paste is off.
- The trigger is multiline OR control characters, where "control characters"
  means any byte `< 0x20` except tab/LF/CR, plus DEL (`0x7f`) and the C1 range
  (`0x80–0x9f`). Tab is excluded as common, legitimate whitespace.
- The dialog preview is truncated for very long pastes (a bounded number of
  lines / characters) with an indication of total size; exact truncation limits
  are fixed during planning. Control bytes in the preview are caret-escaped.
- The dialog is a two-button allow/deny control with no per-dialog
  "Don't ask again" persistence — the global Terminal-settings toggle is the
  persistence mechanism.
- The confirmation is allow/deny only. This feature does NOT strip, sanitize,
  or transform pasted content. Control-character sanitization of other surfaces
  (e.g. window titles) is a separate Modern Terminal Audit item and is out of
  scope here.
- This feature is entirely client-side: paste originates from the client's own
  clipboard/selection and the bracketed-paste signal is client-side, so no
  server round-trip or protocol change is required (unlike OSC 52 gating, where
  a PTY-side program initiates).
- Drag-and-drop insertion, copy-on-select, and OSC 52 clipboard writes are out
  of scope.
- Other Modern Terminal Audit items (inline images, robust scrollback search,
  keyboard pane resize, SSH/tmux integration, programmatic input injection,
  plugin/scripting, i18n, accessibility, URL-open confirmation for
  allowed schemes) are explicitly NOT part of this feature.

## Dependencies

- The existing client paste pipeline — clipboard read → bracketed-paste
  wrapping → chunking → key-input delivery to the PTY — and the separate
  primary-selection (middle-click) paste path that this feature unifies behind
  the gate.
- The focused pane's bracketed-paste mode signal, already resolved at paste
  time, which drives the deferral decision (FR-003).
- The existing GPU dialog overlay infrastructure (the Close / Update /
  disallowed-scheme dialog family) into which the paste-confirmation dialog is
  added as a new variant.
- The existing terminal-config ⇄ settings-webview round-trip (terminal config
  → settings-page toggle → apply/persist → live config reload) through which
  the new opt-in setting is exposed and hot-reloaded.
- The existing per-session/per-pane targeting so a parked paste resumes into
  the correct pane after confirmation.
