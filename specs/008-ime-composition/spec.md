# Feature Specification: IME Composition and Preedit Handling

**Feature Branch**: `008-ime-composition`
**Created**: 2026-05-21
**Status**: Draft
**Input**: User description: "let's go ahead with the IME composition feature above" — implementing
IME composition / preedit handling for CJK and dead-key input, identified in
`design/modern-terminal-audit-2026-05-18.md` as the audit's highest-severity
gap.

## User Scenarios & Testing *(mandatory)*

### User Story 1 — Compose and commit text via the system IME (Priority: P1)

A user with a system input method (Pinyin, Wubi, Hangul, Kana/Romaji,
Vietnamese Telex, dead keys, X11 Compose, macOS Press-and-Hold, etc.) activates
a Scribe pane and types text in their language. The OS candidate popup appears
near the cursor, the user picks a candidate (or completes the composition),
and the resulting characters appear in the shell as if they had been typed.

**Why this priority**: Without P1, an entire user population — every CJK
typist, every European typist who uses dead keys / Compose, every diacritic
user — literally cannot type into any pane. Audit calls this the "single
largest gap"; this is a correctness fix, not a feature.

**Independent Test**: With an OS IME enabled (Pinyin or French dead keys), open
a Scribe pane, type a multi-codepoint sequence, confirm the resulting text
appears at the shell prompt and `cat`/`echo` round-trip it correctly. Verifies
end-to-end without preedit rendering being polished.

**Acceptance Scenarios**:

1. **Given** a macOS user with Pinyin enabled and a focused Scribe pane,
   **When** they type `nihao` and select 你好 from the candidate popup,
   **Then** 你好 appears at the shell prompt and is delivered to the PTY as
   the UTF-8 bytes for those code points.
2. **Given** a Linux X11 user with IBus + the Chewing Zhuyin input method,
   **When** they compose 注音 and press space/enter to commit,
   **Then** 注音 appears in the PTY input stream.
3. **Given** a user with macOS Press-and-Hold or Linux Compose,
   **When** they enter the accented sequence for `é` (`Compose ' e` or
   long-press `e`),
   **Then** `é` appears at the prompt as a single committed character.
4. **Given** a pane is not focused,
   **When** the user types via the IME,
   **Then** no IME activity is captured by Scribe and no PTY bytes are
   produced.

---

### User Story 2 — See composition in-line at the cursor while typing (Priority: P2)

While composing, the user sees the in-progress sequence rendered at the cursor
position with a visual treatment distinguishing it from committed cells (e.g.,
underline). This gives an anchor inside the terminal grid in addition to the
OS-provided candidate popup, matching the convention in Alacritty / Kitty /
WezTerm / Ghostty.

**Why this priority**: Without P2, users still see the candidate popup and can
still commit text (P1 works), but the in-line anchor improves orientation and
matches expectations of every modern terminal. Independent value on top of P1.

**Independent Test**: With IME composing, observe the typed sequence appearing
inline at the cursor cell with a visible visual treatment, before any commit.
On cancel, the preedit cells visually clear with no residue.

**Acceptance Scenarios**:

1. **Given** a focused Scribe pane and an IME mid-composition,
   **When** the user types raw characters,
   **Then** the composing characters render at the cursor cell with a
   distinguishing visual treatment (e.g., underline) and the cursor remains
   anchored at the composition start.
2. **Given** a non-empty preedit,
   **When** the user presses Escape (or otherwise cancels composition),
   **Then** the preedit cells clear and no characters are written to the PTY.
3. **Given** a non-empty preedit,
   **When** the user commits the composition,
   **Then** the preedit cells are replaced by the committed characters as
   delivered through normal PTY output.

---

### User Story 3 — IME state survives workflow events (Priority: P3)

The IME experience holds up under normal Scribe workflows: switching panes,
resizing splits, scrolling, alt-screen-using apps (vim, less, AI TUIs), and
window-focus loss. Composition state never desynchronizes from the PTY view.

**Why this priority**: Polish on top of P1+P2; without P3 the basic typing
case works but unusual workflows surface bugs (orphaned preedit, popup at
wrong location, stuck IME state). Important for daily-driver use, but not
required for the MVP.

**Independent Test**: Run a sequence of pane focus changes, scrolls, resizes,
and a vim session, each time triggering a composition; verify no orphaned
preedit and the popup follows the cursor across all events.

**Acceptance Scenarios**:

1. **Given** a pane is mid-composition,
   **When** the user clicks a different pane,
   **Then** the prior pane's preedit cancels (no implicit commit, no carry),
   and the newly focused pane is IME-ready immediately.
2. **Given** a pane is mid-composition,
   **When** the window loses focus (compositor overlay, screenshot tool, alt-tab),
   **Then** the preedit cancels and IME is disabled until focus returns.
3. **Given** the user scrolls the scrollback or the alt-screen redraws,
   **When** the cursor moves to a new row,
   **Then** the OS candidate popup follows the new cursor cell on the next
   frame.
4. **Given** a pane is resized via divider drag or layout action,
   **When** composition is active,
   **Then** the preedit and candidate popup reposition to the new cursor cell.

---

### Edge Cases

- Bracketed-paste mode is active during composition — preedit text MUST never
  be wrapped in paste markers; only committed text reaches the PTY.
- The negotiated Kitty keyboard protocol is active — IME commits route as
  plain UTF-8 text, not as CSI-u sequences, regardless of progressive-
  enhancement flags. The level-4 encoder is bypassed for IME-origin bytes.
- IME consumes a keystroke that would otherwise be a Scribe shortcut (e.g.
  Enter mid-composition) — the IME wins; the shortcut MUST NOT fire.
- IME emits `Ime::Disabled` while composition is non-empty — preedit MUST
  clear; no orphan cells remain.
- Window goes from focused → hidden → focused without losing IME state — on
  refocus the next composition starts cleanly with empty preedit.
- Search overlay or a modal dialog (`update_dialog`, `close_dialog`) is open —
  IME MUST be disabled for those surfaces in v1; PTY panes only.
- Multiple panes split-view, all visible — only the currently focused pane is
  IME-eligible at any time.
- Preedit longer than the visible terminal width — clip to row; do not wrap or
  scroll on the user's behalf (preedit is a transient overlay, not real cells).

## Requirements *(mandatory)*

### Functional Requirements

- **FR-001**: System MUST request OS-level IME enablement when a Scribe pane
  receives keyboard focus, and MUST disable IME when no pane has focus.
- **FR-002**: System MUST report the focused pane's current cursor cell
  rectangle to the OS so that IME candidate popups appear adjacent to the
  insertion point.
- **FR-003**: System MUST update the reported IME cursor rectangle whenever
  the cursor cell changes due to PTY output, scrolling, alt-screen redraw,
  pane resize, or focus change — within one frame of the change.
- **FR-004**: System MUST route IME-committed text into the focused pane's
  PTY as UTF-8 bytes, bypassing the level-4 key encoder so that no CSI-u or
  legacy modifier encoding is applied to commit-origin bytes.
- **FR-005**: System MUST suppress synthetic per-key dispatch for keystrokes
  the OS reports as consumed by the IME, so the same keystroke is not double-
  applied as both a Scribe shortcut and IME input.
- **FR-006**: System MUST render in-progress preedit text at the cursor cell
  with a visual treatment distinguishing it from committed grid cells
  (underline by default — matching Alacritty/Kitty convention) without
  altering the underlying terminal grid contents.
- **FR-007**: System MUST clear preedit rendering and any IME state when
  composition completes (commit), is cancelled (Escape / OS-reported cancel),
  or when focus is lost.
- **FR-008**: System MUST cancel any in-progress composition when the
  focused pane changes or when the window loses focus — no implicit commit,
  no carry across panes.
- **FR-009**: System MUST preserve byte-identical PTY output for every
  non-IME keystroke path (legacy and Kitty CSI-u encoders unchanged). ASCII
  and existing key encoder behavior MUST be unaffected.
- **FR-010**: System MUST NOT write preedit text to the PTY, scrollback, or
  any persisted session state — preedit is a transient client-local overlay
  until commit.
- **FR-011**: System MUST gate IME activation on the existing focus guard
  (winit `window_focused == true` plus X11 active-window check) so that
  unfocused compositor overlays cannot inject composition events.
- **FR-012**: System MUST NOT enable IME for the search overlay or modal
  dialog surfaces in v1 — those surfaces continue to consume raw key events
  only.

### Quality, UX, and Performance Requirements

- **QR-001**: Implementation MUST preserve existing input-pipeline boundaries.
  The level-4 key encoder (`translate_key`, `translate_key_kitty`,
  `translate_numpad_app_keypad`) MUST NOT be changed except to add an
  IME-active short-circuit that returns before encoding when the keystroke
  was consumed by the IME.
- **QR-002**: Each user story MUST be verifiable via the manual quickstart
  flow described in the User Stories section. No new automated test code is
  requested in this spec; if maintainers add tests during planning, they
  SHOULD live alongside the existing input-pipeline tests in
  `crates/scribe-client/src/input.rs` and use synthetic
  `WindowEvent::Ime(Ime::Preedit/Commit)` events.
- **UX-001**: Preedit visual treatment MUST coexist legibly with the three
  cursor styles (block, beam, underline) — preedit cells render with their
  own treatment regardless of cursor style, and the cursor itself remains
  visible at the composition start.
- **UX-002**: Preedit MUST visually clear in the same frame as a commit /
  cancel — no flicker, no one-frame residue.
- **UX-003**: User-facing behavior for ASCII typists MUST be indistinguishable
  from current behavior.
- **PR-001**: Preedit MUST render within one display frame (~16ms at 60Hz) of
  the first composing keystroke; cursor-rectangle updates MUST happen within
  one frame of any cursor movement. Non-IME key latency MUST NOT regress
  (target: zero measurable change in legacy/Kitty encoding throughput).
- **PR-002**: IME enablement and cursor-rect reporting MUST NOT keep the
  redraw loop alive on hidden windows (mirror the existing AI-indicator
  occlusion-gating convention).

### Key Entities

This feature is pure input-pipeline plumbing — no new persistent or wire-
level entities are introduced. The only new client-local state is a per-
focused-pane preedit record holding the current composing text and its caret
offset, owned by the input subsystem alongside the existing `TerminalMode`
bundle.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: A user with a CJK input method can type and commit a 4-
  character word into any focused Scribe pane within typical typing rhythm
  (under 3 seconds end-to-end including candidate selection), matching the
  experience in Alacritty / Kitty / iTerm2 on the same OS.
- **SC-002**: A user on macOS Press-and-Hold or Linux Compose can produce
  every standard accented Latin character (á, é, í, ó, ú, ñ, ü, ç, ß, etc.)
  via the OS sequence, with the resulting code point arriving in the PTY.
- **SC-003**: Existing ASCII typing throughput, byte-level output, and the
  full 122-test input-pipeline regression suite remain unchanged. Zero
  regressions.
- **SC-004**: Preedit visual feedback appears within one display frame of the
  first composing keystroke (perceived as instant) on a 60Hz display.
- **SC-005**: Across pane switches, window focus changes, scroll, resize, and
  alt-screen redraws, no in-progress composition leaves orphan preedit cells
  or stuck IME state visible to the user.
- **SC-006**: Audit item "IME composition / preedit handling (CJK and other
  input methods)" can be struck from
  `design/modern-terminal-audit-2026-05-18.md` with the same "Update —
  Shipped" annotation pattern used for Kitty CSI-u.

## Assumptions

- **v1 uses the OS-provided candidate popup**: Scribe does not render a
  custom in-window candidate list in v1. The popup is positioned via the
  cursor-rectangle hint reported to the OS. In-window candidate rendering
  (iTerm2-style) is a substantially larger lift and is out of scope here.
- **All winit-supported platforms are in v1**: macOS Cocoa IME, Linux X11
  (XIM / IBus / Fcitx), Linux Wayland, Windows IMM/TSF — all via the
  `WindowEvent::Ime(Ime::{Enabled, Preedit, Commit, Disabled})` abstraction
  winit provides. Platform-specific quirks (IME-consumed-key reporting on
  X11 vs. Wayland) are handled at the winit seam.
- **Preedit visual default is underline**: Matches the convention in
  Alacritty, Kitty, WezTerm, and Ghostty. Configurable styling is out of
  scope for v1; theme colours apply but no new appearance keys are added.
- **Search overlay and modal dialogs are IME-disabled in v1**: PTY panes
  only. Adding CJK search inside the search bar is a follow-up.
- **Focus loss cancels composition**: No implicit commit, no carry to a new
  pane. Mirrors the conservative behavior of Kitty / Alacritty.
- **Bracketed-paste mode is unaffected**: Preedit never enters the paste
  pipeline; only committed text reaches PTY input.
- **Kitty CSI-u and legacy encoders are bypassed for commit-origin bytes**:
  IME commits are plain UTF-8 text, not "keystrokes" to encode. The existing
  encoders remain untouched for the keystroke path.
- **Accessibility tree exposure of preedit is out of scope**: The terminal
  grid is already invisible to AccessKit (separate audit item); preedit
  surfacing for screen readers is deferred to that future work.
- **Existing focus guard remains authoritative**: IME activation is gated on
  `window_focused == true` AND the X11 `_NET_ACTIVE_WINDOW` guard (when
  applicable), reusing `x11_focus.rs` rather than introducing a parallel
  gate.
- **No new config keys in v1**: IME is always-on per platform conventions.
  An opt-out can be added later if a user reports an interaction issue.

## Out of Scope (v1)

The following items are recognized adjacent gaps but are explicitly NOT
included in this feature; they remain on the audit's open list:

- In-window IME candidate-list rendering (iTerm2-style).
- IME support for the search overlay or modal dialogs.
- Per-app preedit position offset / custom preedit anchor.
- East Asian ambiguous-width policy configurable toggle (separate audit
  item: "Unicode width controls").
- Configurable font-fallback chain for emoji / CJK / Nerd Font glyphs
  (separate audit item).
- AccessKit / screen-reader exposure of preedit or the terminal grid
  (separate audit item).
- A reduced-motion preference covering preedit (current motion budget is
  unchanged; no animation is added by this feature).
