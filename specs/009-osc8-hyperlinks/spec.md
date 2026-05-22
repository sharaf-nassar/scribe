# Feature Specification: OSC 8 Explicit Hyperlinks

**Feature Branch**: `009-osc8-hyperlinks`
**Created**: 2026-05-21
**Status**: Draft
**Input**: User description: "OSC 8 explicit hyperlink support — parse OSC 8
escape sequences, attach hyperlink targets to terminal cells, and surface the
true URI on hover/click/copy distinct from the displayed label." Sourced as
the next remaining "parsed but not surfaced"-style gap from
`design/modern-terminal-audit-2026-05-18.md` (Trust & Safety / Workflow),
filling a known integration gap where today's heuristic URL detection cannot
distinguish a displayed label from its real destination.

## Clarifications

### Session 2026-05-21

- Q: Where is the OSC 8 URI surfaced on hover (since URI may differ from
  displayed text)? → A: Reuse the existing tooltip overlay
  (`crates/scribe-client/src/tooltip.rs` / `lat.md/client#Tooltip`) to render
  the URI above/below the hovered cell after a short hover dwell.
- Q: What does the user see when they activate an OSC 8 hyperlink whose
  scheme is outside the existing outbound URL allowlist? → A: Reuse the
  existing GPU dialog overlay (`lat.md/client#Dialogs`) to show a
  confirmation with the full URI, an explicit "scheme normally blocked"
  warning, an **Open Anyway** action that proceeds with the URI, and a
  **Cancel** action that dismisses without opening. No hard block; the user
  chooses.
- Q: What is the matching scope for OSC 8 `id=` cross-cell reconnection? →
  A: Per-open-sequence scope on a single pane. Cells share a span only
  when their `id` matches AND they were tagged inside the same open/close
  run (or immediately-adjacent open/close runs sharing the `id` with no
  intervening differently-opened OSC 8). A later, separately-opened OSC 8
  reusing the same `id` value starts a new span; this prevents cross-tool
  id-claim attacks where a malicious program reuses a previously-emitted
  `id` to redirect later activations.
- Q: Do OSC 8 URIs survive on *replayed* scrollback after server hot
  reattach (zero-downtime upgrade) and cold-restart restore? → A: Firm
  MUST for *live* (post-reattach) cells; replayed-scrollback fidelity is
  deferred to `/speckit-plan` after a brief look at the
  `crates/scribe-common/src/screen_replay.rs#SessionReplay` zstd ANSI
  byte buffer (lat.md/common#Session Replay, lat.md/client#Replay
  Restore) to confirm whether stored bytes preserve OSC 8 verbatim or
  strip unknown OSC; the resulting decision (full restoration vs.
  documented limitation) MUST be recorded in the plan.
- Q: How is OSC 8 URI storage bounded to avoid memory exhaustion from a
  pathological program emitting many unique URIs? → A: Intern URI strings
  per pane — every cell carries a small handle into a per-pane URI table
  and identical URI strings share one stored copy. No additional numeric
  cap on distinct URIs; the existing scrollback line cap is the natural
  backstop, and the per-URI FR-010 length cap still applies.

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Tool-emitted hyperlinks reach their real destination (Priority: P1)

A developer runs commands that emit OSC 8 hyperlinks — `ls --hyperlink=auto`
linking each entry to its absolute path, compiler error output linking
diagnostics to source locations, build tools linking dashboards, package
managers linking changelogs. They Ctrl+click a hyperlinked cell (or
right-click → Open URL) and the OS handler opens the URI the tool
*intended*, not a heuristic guess derived from whatever text was visible.

**Why this priority**: This is the user-facing behavior the standard exists
for. Without it, OSC 8-emitting tools are no more useful than plain text —
every Ctrl+click loses the destination the program supplied. The emitting
tools are widely used (coreutils since 8.30, git, gcc/clang, cargo, modern
build systems), so the gap is hit on common workflows. This is the largest
single user-facing payoff of the feature and the foundation US2 and US3 sit
on.

**Independent Test**: Run `ls --hyperlink=auto` in a directory of files whose
names contain spaces, regex-meta characters, or text that does not look like
a URL. Ctrl+click a filename and confirm the OS handler receives the
`file://` URI the OSC 8 sequence carried — not the bare filename, not
nothing. Delivers value alone, even if US2 and US3 are not built.

**Acceptance Scenarios**:

1. **Given** a tool emits OSC 8 hyperlinks, **When** the user Ctrl+clicks a
   cell inside a hyperlink span, **Then** the URI sent to the OS handler is
   exactly the URI from the OSC 8 sequence (not a heuristic re-scan of the
   cell's displayed text).
2. **Given** a hyperlinked span contains text that *also* matches the
   heuristic URL pattern (e.g., a label that contains a substring like
   "README" or "example.com"), **When** the user activates the span, **Then**
   the OSC 8 URI takes precedence and the heuristic match is ignored for that
   span.
3. **Given** an OSC 8 sequence closes (`OSC 8 ; ; ST`), **When** subsequent
   cells are emitted, **Then** those cells carry no hyperlink target and
   activation falls back to today's heuristic behavior.
4. **Given** OSC 8 uses an `id=` parameter to link non-contiguous cells
   (e.g., across a line wrap), **When** the user activates any cell in the
   span, **Then** they reach the same single destination.
5. **Given** the URI uses a scheme not on Scribe's outbound allowlist (e.g.,
   `javascript:`, `data:`), **When** the user activates the cell, **Then**
   the confirmation dialog (FR-015) appears with the full URI, a
   scheme-blocked warning, an Open Anyway action, and a Cancel action
   (default focus); cancelling dismisses without opening, Open Anyway
   proceeds with the URI through the existing OS-handler path.

---

### User Story 2 - Real destination visible before activation (Priority: P1)

A developer sees a cell whose displayed text is an innocuous label ("click
here", "anthropic.com", a project name) inside output they did not author —
remote SSH session, log file, AI-generated content. Before clicking, they
can see the real OSC 8 destination, distinct from the displayed label, and
decide whether to open it.

**Why this priority**: This is the trust-and-safety reason OSC 8 exists as a
standard at all. Without surfacing the true target, the feature inverts
Scribe's existing trust posture: heuristic detection at least matches what
the user sees, while opaque OSC 8 hides the real destination behind any
label the emitter chose. Independently shippable once US1 has parsed the
sequence, and independently valuable: users get the security signal even
before deciding whether to enable OSC 8 in untrusted contexts. Equal P1 with
US1 because either alone is a shippable, valuable increment.

**Independent Test**: Emit a hyperlink where displayed text and URI differ
obviously (label "anthropic.com", URI `https://evil.example.com`). Hover or
right-click the cell and confirm the real URI is visible to the user before
any open action fires. Testable with no dependency on US3.

**Acceptance Scenarios**:

1. **Given** a cell carries an OSC 8 URI, **When** the user hovers the cell
   for a brief dwell, **Then** the existing tooltip overlay appears above
   or below the cell and shows the full OSC 8 URI verbatim, before any
   activation is possible.
2. **Given** the user opens the right-click context menu over an OSC 8
   hyperlink, **When** the menu renders, **Then** the "Open URL" item
   references the OSC 8 URI (visible truthfully to the user, not the
   displayed label).
3. **Given** the context menu is open over an OSC 8 hyperlink, **When** the
   user picks the new "Copy hyperlink address" entry, **Then** the OSC 8 URI
   is copied verbatim to the system clipboard — distinct from the existing
   "Copy" item which continues to copy the displayed text selection.
4. **Given** the displayed text and the URI differ, **When** the user copies
   a selection across the hyperlink span via the existing copy path,
   **Then** the copied text is the displayed text (unchanged behavior — only
   the new "Copy hyperlink address" entry surfaces the URI).
5. **Given** the URI uses a disallowed scheme (`javascript:`, `data:`,
   etc.), **When** the user hovers, **Then** the real URI is still shown
   in the tooltip (trust signal) — independent of the per-activation
   confirmation gate (FR-015) that fires on click.

---

### User Story 3 - Hyperlinks survive scrollback, wrapping, and reattach (Priority: P2)

A developer scrolls back through a session — possibly across a hot reattach
or cold restart — to an older OSC 8 hyperlink on a line that wrapped across
multiple visual rows. They can still hover and activate it, and the
destination is the URI the tool emitted, not lost or corrupted.

**Why this priority**: High value, but depends on US1/US2 already binding
URIs to cells. Without US1 there's nothing to preserve; with US1 the
preservation cost is mostly riding the existing scrollback / replay paths.
Sequenced after US1/US2.

**Independent Test**: Emit an OSC 8 hyperlink that wraps across visual rows.
Scroll it into history. Force a hot reattach (server upgrade) and a
cold-restart restore. After each, hover and activate the hyperlink and
confirm the same URI fires. Independently demonstrable once US1+US2 exist.

**Acceptance Scenarios**:

1. **Given** an OSC 8 hyperlink that wraps across visual rows via the
   standard `id=` reconnection inside a single open/close run, **When**
   the user activates any cell in the span, **Then** all cells in the
   span resolve to the same single URI. **And given** a later,
   separately-opened OSC 8 reuses the same `id` value with a different
   URI, **Then** that later span is treated as a new span (no
   retroactive merge with the prior span).
2. **Given** an OSC 8 hyperlink that scrolled into history, **When** the
   user scrolls back to it and activates a cell, **Then** the URI fires
   correctly (no loss of the cell-bound URI through scrollback storage).
3. **Given** an OSC 8 hyperlink emitted *after* the client reattaches
   (post-reattach live cells), **When** the user activates a cell in the
   span, **Then** the URI fires correctly with no regression versus a
   freshly-started session. (Hyperlinks reconstructed from *replayed*
   scrollback follow the planning-time decision recorded under FR-012;
   the chosen outcome is verified separately.)
4. **Given** scrollback trim removes a cell that was inside an OSC 8
   hyperlink, **When** the trim completes, **Then** the hyperlink tracking
   state remains consistent (no dangling URI, no crash, no memory leak).
5. **Given** two panes each receive independent OSC 8 streams, **When** one
   pane closes or detaches, **Then** the other pane's hyperlink state is
   unaffected.

---

### Edge Cases

- A hyperlink never closes before the pane is closed or EOF arrives — open
  URI state MUST NOT bind to all subsequent cells indefinitely; per-pane
  natural lifecycle MUST keep state bounded.
- Nested OSC 8 (open inside an already-open hyperlink with a different URI)
  — behavior MUST be deterministic; the chosen rule (replace vs. ignore) is
  documented (see Assumptions).
- Extremely long URI (multi-kilobyte or unbounded) — MUST be capped to a
  documented length to prevent memory exhaustion; URIs over the cap are
  rejected and the affected cells carry no hyperlink target.
- Malformed OSC 8 (missing `;`, missing URI, garbage params) — MUST NOT
  crash the PTY parser or the client, MUST NOT bind any URI, and MUST leave
  any previously-open hyperlink in a deterministic state.
- URI scheme outside the existing allowlist (`javascript:`, `data:`, custom
  schemes) — MUST still be visible in hover (FR-006) and the context menu;
  on activation, the system MUST present the FR-015 confirmation dialog
  giving the user an explicit Open Anyway / Cancel choice, defaulting to
  Cancel.
- Heuristic URL detection finds a different URL inside an OSC 8 span — OSC
  8 MUST win for any cell inside the OSC 8 span; outside the span,
  heuristic detection applies as today.
- Bracketed paste containing OSC 8 — bracketed paste already neutralises
  escape-sequence interpretation in payloads; this feature does not change
  that.
- Smart-selection actions over an OSC 8 hyperlink span — MUST surface and
  activate the OSC 8 URI, consistent with US1 precedence.
- A program emits OSC 8 but the cell content is whitespace or empty — the
  hyperlink still binds and activates on the underlying cell row/column;
  zero-width hit-testing edge cases are documented but do not regress
  existing whitespace-cell behavior.

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: The system MUST parse OSC 8 sequences (`OSC 8 ; <params> ;
  <URI> ST`) and recognise the close form (`OSC 8 ; ; ST`).
- **FR-002**: Every cell emitted between an OSC 8 open and its matching
  close MUST carry the OSC 8 URI as an associated target — including cells
  inside wraps and cells that subsequently scroll into history.
- **FR-003**: When the user activates a cell that carries an OSC 8 URI —
  via Ctrl+click, the right-click "Open URL" item, or any smart-selection
  action that today opens a URL — the URI sent to the OS handler MUST be
  the OSC 8 URI, NOT a heuristic re-scan of the cell's displayed text.
- **FR-004**: An OSC 8 URI MUST take precedence over heuristic URL
  detection on the same cell.
- **FR-005**: A non-empty `id=` parameter MUST link cells with matching
  `id` into a single conceptual hyperlink span when those cells were tagged
  inside the *same* OSC 8 open/close run on the same pane, or inside
  immediately-adjacent open/close runs sharing the `id` with no
  intervening differently-opened OSC 8. A later, separately-opened OSC 8
  reusing the same `id` value with a *different* URI MUST start a new
  span; the system MUST NOT retroactively merge it with the prior span
  carrying that `id`. This narrows the merge surface to the wrapped-line
  use case the standard exists to solve, while resisting cross-tool
  id-claim redirection.
- **FR-006**: The system MUST expose the OSC 8 URI to the user before
  activation through both (a) the existing tooltip overlay
  (`crates/scribe-client/src/tooltip.rs`, anchored above or below the
  hovered cell) after a short hover dwell, and (b) the right-click context
  menu, so the displayed label cannot mask the real destination. The
  tooltip MUST show the full URI verbatim (subject only to the FR-010
  length cap) so the user sees exactly what would be activated.
- **FR-007**: A "Copy hyperlink address" context-menu entry MUST be
  available when the right-click target is a cell carrying an OSC 8 URI,
  copying the URI verbatim to the system clipboard. The existing "Copy"
  path for selections MUST remain unchanged.
- **FR-008**: OSC 8 close (`OSC 8 ; ; ST`) MUST terminate the active
  hyperlink so subsequent cells carry no URI.
- **FR-009**: URI scheme validation MUST reuse the existing outbound URL
  scheme allowlist for the *unprompted* open path: allowed-scheme OSC 8
  URIs activate directly (same path as today's heuristic URLs). OSC 8 URIs
  whose scheme is OUTSIDE the allowlist MUST NOT open silently and MUST
  NOT be hard-blocked silently either — instead, the system MUST present a
  confirmation dialog (FR-015) that gives the user an explicit choice to
  proceed or cancel. Disallowed-scheme URIs MUST remain visible to the
  user per FR-006 regardless of the user's eventual choice.
- **FR-015**: The system MUST present an in-app confirmation dialog,
  reusing the existing GPU dialog overlay infrastructure
  (`lat.md/client#Dialogs`), whenever the user activates an OSC 8 URI whose
  scheme is outside the existing outbound URL allowlist. The dialog MUST
  show (a) the full URI verbatim subject only to the FR-010 length cap,
  (b) an explicit "scheme normally blocked" warning naming the scheme, (c)
  an **Open Anyway** action that proceeds with the URI through the
  existing OS-handler open path, and (d) a **Cancel** action (default
  focus) that dismisses without opening. The dialog MUST be dismissible by
  the same keyboard conventions as the existing Close/Update dialogs (Esc
  cancels). No per-activation persistence ("Don't ask again") is required
  by this spec.
- **FR-016**: OSC 8 URI storage MUST be interned per pane: every cell
  carries only a small handle into a per-pane URI table, and identical
  URI strings share one stored copy. The system MUST NOT store the full
  URI string verbatim on every cell. No additional numeric cap on the
  number of distinct URIs per pane is required by this spec — the
  existing scrollback line cap is the natural backstop, and FR-010 still
  caps each URI's length. The URI table MUST evict entries whose last
  referencing cell has been trimmed from scrollback, so the table size
  tracks the pane's live URI usage rather than growing monotonically.
- **FR-010**: URI length MUST be bounded to **2048 bytes (2 KiB)**,
  matching the kitty-style de facto cap adopted by peer terminals. The
  primary enforcement path is upstream `alacritty_terminal`'s OSC 8
  parser; if upstream applies an equal-or-smaller cap, the Scribe-side
  pass MUST NOT relax it. If upstream is uncapped or applies a larger
  cap, the OSC 8 cell-walk pass MUST treat URIs longer than 2 KiB as
  absent (the affected cells carry no URI in the `UrlSpan` cache and do
  not activate). Spec checklist for the implementation: verify upstream
  behavior at the first task (Setup) and record the inherited or
  Scribe-enforced cap value in `lat.md/client.md`.
- **FR-011**: Hyperlink parser/state MUST be per pane/session — no
  cross-pane leakage of open-hyperlink state.
- **FR-012**: *Live* hyperlinks emitted by the PTY *after* a client
  reattach (zero-downtime upgrade) or cold-restart restore MUST work
  without regression — full hover/context-menu/activation behavior on the
  reattached session. This is a firm MUST and is verifiable via the US3
  quickstart. Hyperlinks on *replayed* scrollback (cells reconstructed
  from the stored `SessionReplay` zstd ANSI byte buffer in
  `crates/scribe-common/src/screen_replay.rs`) are NOT preserved by this
  spec — the byte buffer emits chars + SGR only and does not re-emit
  OSC 8 open/close around hyperlinked runs (resolved option (b) per
  `research.md` decision 3). This limitation MUST be documented in
  `lat.md/client.md` at completion so future readers know it is
  intentional and where the follow-up improvement lives (extending
  `snapshot_to_ansi` to re-emit OSC 8 around hyperlinked cell runs, in a
  separate spec that owns the resulting protocol/storage version bump).
- **FR-013**: Malformed or oversized OSC 8 sequences MUST be ignored
  without crashing the PTY parser or the client, and MUST leave
  previously-open hyperlink state in a deterministic, documented state.
- **FR-014**: Existing heuristic URL detection MUST continue to function
  unchanged for cells that do NOT carry an OSC 8 URI (no regression on
  plain `https://…` substrings).

### Quality, UX, and Performance Requirements

- **QR-001**: Implementation MUST preserve existing architecture boundaries
  — the PTY OSC interception layer, the client URL-detection cache, the
  context-menu surface, the scrollback replay path, and the per-session
  ownership of terminal state — and MUST reuse those abstractions rather
  than introduce parallel ones unless a divergence is justified during
  planning.
- **QR-002**: Verification is via manual quickstart scenarios, one per user
  story (consistent with the project's test-only-on-explicit-request rule
  and the manual-quickstart precedent set by specs 005, 007, and 008). A
  future automated OSC 8 conformance suite (open/close, `id=` reconnection,
  malformed payloads, scheme allowlist, scrollback survival) is a strong
  candidate; adding it requires explicit approval during planning and is
  out of scope here.
- **UX-001**: All new surfaces — hover affordance for the real URI, the
  right-click "Open URL" wording over OSC 8 spans, and the new "Copy
  hyperlink address" entry — MUST match existing Scribe context-menu,
  hover, and configurable-shortcut conventions; no new visual paradigm.
  The minimum baseline for hyperlink visibility matches today's hover
  affordance for heuristic URLs; any always-on visual treatment (persistent
  underline / accent color) is OUT OF SCOPE for this spec and may be a
  follow-on.
- **PR-001**: OSC 8 parsing MUST add no measurable per-frame render cost
  beyond the existing OSC processing path, and per-hover/click URI lookup
  MUST remain at most O(log n) in scrollback size at the configured
  scrollback cap. The per-pane URI table (FR-016) MUST keep memory usage
  proportional to the number of *distinct* URIs in the pane's live
  scrollback, not to the number of cells — pathological programs emitting
  many unique URIs are bounded by the existing scrollback line cap rather
  than blowing memory linearly in cell count.

### Key Entities

- **Hyperlink span**: A set of terminal cells associated with a single URI
  emitted by an OSC 8 open/close pair, optionally reconnected across line
  wraps by a shared `id` parameter, scoped per FR-005 to the same (or
  immediately-adjacent same-`id`) open/close run on the same pane. The
  span references the URI via a per-pane URI table handle (FR-016) rather
  than storing the URI string per cell.
- **Per-pane URI table**: A pane-scoped intern table mapping URI handles
  to URI strings. Each cell that carries an OSC 8 hyperlink stores only a
  handle; the table dedupes identical URIs so repeated hyperlinks (e.g.,
  thousands of `ls --hyperlink` rows pointing at the same parent dir)
  share storage. Entries are evicted when their last referencing cell is
  trimmed from scrollback.
- **Parser state (per pane)**: Tracks whether an OSC 8 hyperlink is
  currently open, the active URI, and the active `id` (if any), so
  subsequently-emitted cells can be tagged. Bounded in size by the URI
  length cap (FR-010) and reset on close.
- **Activation request**: Originates from Ctrl+click, the right-click
  "Open URL" item, or a smart-selection action. Carries the resolved URI
  (OSC 8 if the target cell carries one, otherwise the heuristic-detected
  URL) and the originating cell row/column so trust-signaling surfaces
  have the truthful destination.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: 100% of OSC 8 hyperlinks emitted by `ls --hyperlink=auto` in
  a representative directory listing activate to the emitted `file://`
  URI on Ctrl+click (today: 0% — OSC 8 is ignored).
- **SC-002**: When displayed label and OSC 8 URI differ, 100% of users
  can see the real URI before activation, via the hover/context-menu
  surface (today: 0% — the OSC 8 URI is not displayed anywhere).
- **SC-003**: 100% of OSC 8 spans that wrap across visual rows via `id=`
  activate to a single, consistent URI from any cell in the span (today:
  0% — the span is invisible to the user).
- **SC-004**: 0 regressions in heuristic URL detection on cells that do
  NOT carry an OSC 8 URI — Ctrl+click on a plain `https://…` substring
  continues to work exactly as today.
- **SC-005**: URI activation on Ctrl+click adds no perceptible delay
  versus today's heuristic path (target: no more than one render-frame
  interval of additional latency, well below the threshold a user can
  perceive).
- **SC-006**: Disallowed-scheme URIs (`javascript:`, `data:`, anything
  outside the existing allowlist) require the FR-015 confirmation dialog
  before activation in 100% of cases, default-focus Cancel, while remaining
  visible in the hover/context-menu surface for trust signaling.
  Allowed-scheme URIs MUST NOT trigger the confirmation dialog (no
  regression in the activation latency for the common case).
- **SC-007**: Across hot reattach (zero-downtime upgrade) and cold-restart
  restore, *live* (post-reattach) OSC 8 hyperlinks function with 0
  regressions — firm MUST. Replayed-scrollback hyperlink fidelity is
  decided at `/speckit-plan` per FR-012 after inspecting
  `crates/scribe-common/src/screen_replay.rs#SessionReplay`; the
  committed outcome (full restoration vs. documented limitation) MUST be
  honoured 100% of the time.
- **SC-008**: 0 cross-pane leakage — two panes receiving independent OSC
  8 streams never see each other's open-URI state.

## Assumptions

- The "OSC 8 hyperlink standard" referenced is the de facto standard
  documented at the widely-cited specification gist (`OSC 8 ; <params> ;
  <URI> ST`, params as semicolon-free `key=value` pairs — notably `id=`
  — close as `OSC 8 ; ; ST`) and adopted by kitty, WezTerm, iTerm2,
  Ghostty, GNOME Terminal / VTE, Windows Terminal, and coreutils.
  "Conformant" means matching that documented format.
- The hover affordance for OSC 8 hyperlinks is the existing tooltip overlay
  (`crates/scribe-client/src/tooltip.rs`, "small dark box with light text
  above or below an anchor rect" per `lat.md/client#Tooltip`), anchored to
  the hovered cell and triggered after a brief hover dwell. A persistent
  always-on visual treatment on the cells themselves (underline, accent
  color) is intentionally OUT OF SCOPE for this spec and may be a follow-on
  UX iteration.
- The URI length cap is in the same order of magnitude as established
  practice in mature terminals (the kitty-style 2 KB cap is a sensible
  default); the exact value is fixed during planning and recorded in the
  implementation rather than re-litigated per session.
- Nested OSC 8 (an open inside an already-open hyperlink with a different
  URI) follows the "replace" rule — the new open ends the prior span and
  starts a new one. This matches the standard's documented behavior in
  current peer terminals; planning may revise after a brief survey if
  peer terminals diverge.
- The existing outbound URL scheme allowlist (`https`, `http`, `ftp`,
  `file`, `mailto`, `ssh`, `telnet`) is reused unchanged for the
  unprompted-open path of OSC 8 activation. Extending the allowlist is
  OUT OF SCOPE. Disallowed-scheme URIs route through the FR-015
  confirmation dialog rather than being silently blocked. Whether
  *allowed*-scheme URIs ever require confirmation (the audit's separate
  "URL-open confirmation" item) is intentionally NOT broadened here.
- "Copy hyperlink address" appears in the context menu *only* when the
  click target is a cell carrying an OSC 8 URI; for heuristic-detected
  URLs the existing copy behavior is unchanged.
- Server-side OSC 8 parsing lives alongside the existing OSC 0/2/7/133/1337
  path in the PTY metadata layer; client-side carriage of the URI on
  cells reuses whatever attribute path is most consistent with existing
  per-cell state (colors, styles). The choice between (a) re-enabling the
  upstream emulator's existing hyperlink feature on cells vs. (b) a
  Scribe-owned parallel hyperlink span layer is an implementation
  decision deferred to planning.
- Other Modern Terminal Audit items (OSC 52 clipboard gating, URL-open
  confirmation, robust scrollback search, IME, inline images,
  plugin/scripting, pane resize) are explicitly NOT part of this feature.

## Dependencies

- Existing OSC interception and metadata pipeline (the `scribe-pty` OSC
  interceptor and the `MetadataParser::process_osc` style dispatch already
  handling OSC 0/2/7/133/1337).
- Existing client-side URL detection and hover-hit-test cache (where OSC
  8 spans must take precedence over heuristic detection).
- Existing context-menu surface (right-click "Open URL", "Open File",
  "Copy"), to which the new "Copy hyperlink address" entry is added.
- Existing per-session terminal state ownership and the scrollback / hot
  reattach / cold-restart replay paths through which cell attributes
  survive.
- Existing outbound URL scheme allowlist and the activation path through
  which URIs reach the OS handler.
- Existing GPU dialog overlay infrastructure (`lat.md/client#Dialogs` —
  the Close Dialog / Update Dialog family) into which the FR-015
  disallowed-scheme confirmation is added as a new dialog variant.
