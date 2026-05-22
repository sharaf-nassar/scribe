# Research: IME Composition and Preedit Handling

**Phase 0 output for `008-ime-composition`.** All open questions identified
during plan drafting are resolved below with Decision / Rationale /
Alternatives.

---

## R1. winit IME contract and platform support

**Decision**: Use `winit`'s `WindowEvent::Ime(Ime::{Enabled, Preedit,
Commit, Disabled})` as the single platform-abstracted entry point, paired
with `Window::set_ime_allowed(bool)` for opt-in and
`Window::set_ime_cursor_area(Position, Size)` for popup placement.

**Rationale**:
- `winit` already abstracts macOS Cocoa IME, Linux X11 (XIM / IBus / Fcitx
  via `ibus-x11` and `fcitx-x11` bridges), Linux Wayland (`zwp_text_input_v3`),
  and Windows IMM/TSF behind a single event surface. Building anything below
  that line would mean reimplementing the abstraction Scribe already
  depends on for windowing, focus, and key input.
- `set_ime_allowed` is the published activation gate; per winit docs the
  default is `false` on every platform — explaining the audit's observation
  that *no IME events arrive today*.
- The four `Ime::*` variants form a complete state machine:
  - `Enabled` — IME is now active for this window.
  - `Preedit(text, cursor_range)` — in-progress composition; cursor range
    can be `Some(start, end)` for a caret hint inside preedit.
  - `Commit(text)` — finalized text the application should treat as
    "typed".
  - `Disabled` — IME no longer active; clear preedit state.

**Alternatives considered**:
- Direct platform APIs (Cocoa `NSTextInputClient`, XIM, IBus DBus, IMF):
  rejected. Triples the platform code, fights winit's existing input loop,
  and would need its own focus/lifecycle wiring.
- A third-party IME crate: none exist that abstract preedit on top of
  winit. Building one is out of scope for this feature.

**Notes**:
- On Linux X11, the IME event source depends on environment variables
  (`GTK_IM_MODULE`, `QT_IM_MODULE`, `XMODIFIERS`). winit reads these via
  the underlying X11 input bridge; Scribe takes no opinion.
- On Wayland the compositor's `zwp_text_input_v3` is required; some
  compositors do not implement it, in which case `Ime::Enabled` never
  arrives. This is a platform limitation, not a Scribe bug.

---

## R2. IME activation gating vs. the existing focus guard

**Decision**: IME enablement is gated on **the same predicate** the keyboard
input path already uses: `window_focused == true` AND the X11
`_NET_ACTIVE_WINDOW` check (when applicable). Reuse `x11_focus.rs`
unchanged.

**Rationale**:
- The audit's recurring "data on the wire but UI sink missing" pattern is
  also a focus-discipline pattern: the existing focus guard exists
  precisely to prevent unfocused overlay events from reaching the PTY.
  Letting IME bypass it would be a regression.
- Reusing the same guard means there is one place to fix focus bugs, and
  IME state cannot diverge from the keyboard path.

**Implementation**:
- When `App` transitions to focused: call `set_ime_allowed(true)` and
  immediately push a cursor-area update.
- When `App` transitions to unfocused (winit `Focused(false)` OR the X11
  guard reports inactive): call `set_ime_allowed(false)` and clear any
  in-progress preedit.

**Alternatives considered**:
- Parallel IME-specific focus tracking: rejected — duplicates state and
  creates the possibility of divergence.

---

## R3. Cursor rectangle reporting

**Decision**: Report the cursor cell's window-space rectangle (origin in
points, size = one cell) via `Window::set_ime_cursor_area` on three
triggers:
1. **Focus change** — once on transition to focused.
2. **Cursor cell movement** — driven by the same dirty signal that schedules
   a redraw. Each frame that would render a new cursor position also pushes
   the IME cursor area before `pre_present_notify`.
3. **Resize / DPI change** — `WindowEvent::Resized` /
   `WindowEvent::ScaleFactorChanged`.

**Rationale**:
- Frame-coupled updates keep the popup glued to the caret with no debounce
  needed and no risk of stale positions outliving a redraw.
- The cursor rect is already computed every frame for cursor rendering
  (`rendering.md#Cursor Rendering`). Forwarding it costs one extra winit
  call per dirty frame — well within budget.

**Alternatives considered**:
- Update only on commit / preedit start: rejected — the popup would lag
  when scrolling or alt-screen redraws move the cursor.
- Update on a timer: rejected — adds latency and wakes the redraw loop.

**Edge case**: When the window is occluded (existing AI-indicator
convention from `client#AI Indicator#Pulse Envelope#Occlusion Gating`), the
redraw loop sleeps. IME updates also pause — there is no useful work to do
for a popup that cannot be seen.

---

## R4. Preedit rendering approach

**Decision**: Render preedit as a **transient client-local overlay** drawn
*after* the terminal grid but *before* other chrome (search overlay,
dialogs). Use the existing `cosmic-text` shaping path for glyphs and
`chrome.rs#solid_quad` for the underline. No changes to the wgpu pipeline.

**Rationale**:
- Preedit text is not "real" terminal cells — it's a hint that disappears
  on commit/cancel. Modeling it as cells would either pollute scrollback
  or require a never-persisted shadow grid. An overlay is the right
  shape.
- Existing render layers (grid → preedit → chrome → dialogs) give a clean
  z-order. The search overlay and dialogs already sit above the grid;
  preedit slots just under them.
- `cosmic-text` already handles the script/font fallback needed for CJK
  glyphs (subject to the separate audit item on configurable fallback —
  out of scope here; cosmic-text's implicit fallback is acceptable for
  v1).

**Visual treatment**:
- Underline under the preedit text (theme foreground colour, 1px or
  hi-DPI equivalent).
- Background defaults to the same cell background; if the preedit
  contains a caret range from `Ime::Preedit`, the caret cell uses an
  inverted background to indicate the active insertion point inside
  multi-character composition.
- The terminal cursor itself remains drawn at the composition start cell.

**Alternatives considered**:
- A separate wgpu render pass: rejected — overkill for short transient
  strings; reuses no existing infra.
- Writing preedit as real cells with a "transient" flag: rejected —
  contaminates scrollback and replay logic; would also need new server
  protocol fields to suppress preedit from snapshot-replay.

---

## R5. PTY write path for committed text

**Decision**: On `Ime::Commit(text)`, push the UTF-8 bytes through the
existing `KeyInput` write path **bypassing** `translate_key`,
`translate_key_kitty`, and `translate_numpad_app_keypad`. Commit text
becomes a `ClientMessage::KeyInput { bytes: text.into_bytes() }` directly.

**Rationale**:
- IME commits are not "keys" — they are finalized text. Encoding them
  through CSI-u or legacy modifier sequences would corrupt multi-byte
  UTF-8 and double-encode modifiers the IME has already consumed.
- The audit specifically required preserving byte-identical encoder
  behavior for the non-IME path; bypassing rather than modifying the
  encoder satisfies this on the strongest possible reading.
- Bracketed paste is *not* applied because commits are not pastes — they
  represent the user's keystrokes after IME processing, which the shell
  expects to see byte-identical to native IME-aware terminal input.

**Alternatives considered**:
- Wrap commits in bracketed-paste markers: rejected — diverges from every
  other terminal's behavior and breaks shells expecting CJK at the prompt.
- Encode commits via the level-4 encoder: rejected — see rationale above.

---

## R6. IME-consumed keystroke suppression

**Decision**: When `WindowEvent::Ime(Ime::Preedit/Commit/Enabled)` fires for
a given OS-key sequence, the corresponding `WindowEvent::KeyboardInput`
events for that sequence MUST NOT be dispatched to the level-4 encoder.

**Implementation**:
- Track a small flag `ime_active: bool` set by `Ime::Enabled` and cleared
  by `Ime::Disabled`.
- In the keyboard handler, when `ime_active` is true AND the OS reports
  the keystroke was consumed (winit `KeyEvent.text` semantics +
  `Ime::Preedit` arrival on the same event-loop iteration), skip the
  encoder branch entirely.

**Rationale**:
- winit's contract is that `Ime::Preedit`/`Commit` events arrive on the
  same event-loop iteration as the consumed `KeyboardInput`. The
  application sees both and must decide which to honor.
- Without suppression, pressing Enter to commit a Pinyin selection would
  ALSO fire a Scribe Enter shortcut or send `\r` to the PTY — duplicating
  input or stealing the IME's confirmation.

**Alternatives considered**:
- Time-based deduplication: rejected — fragile across platforms.
- Trusting winit `KeyEvent.text` alone to mean "non-consumed": rejected —
  platform-specific reporting differs (X11 may emit synthetic key events
  for dead-key composers even after IME has consumed them).

---

## R7. Per-pane state vs. window-global state

**Decision**: IME state is **window-global**, not per-pane. Preedit state
travels with the focused pane (cleared on focus change). Activation
(`set_ime_allowed`) is a window-level winit call — there is no per-pane
toggle to invent.

**Rationale**:
- Only one pane has keyboard focus at a time. The OS IME is one stateful
  resource per OS-window.
- Modeling per-pane IME would require carrying preedit cells across pane
  switches, which the spec explicitly rejects (focus loss cancels
  composition).

**Implementation**:
- `App` holds a single `Option<PreeditState>`.
- `Pane::focused()` is the implicit owner; on focus change the option is
  cleared.

---

## R8. Macros / shortcuts during composition

**Decision**: When `ime_active` is true and preedit is non-empty, Scribe
shortcuts (palette, split, copy/paste, etc.) MUST NOT fire for keystrokes
the IME consumed. Keystrokes the IME did not consume (e.g., the user
presses a chord that bypasses the IME on their platform) continue to fall
through to the shortcut layer normally.

**Rationale**:
- IMEs already swallow the keys they need (Enter, Space, arrow keys for
  candidate navigation). Allowing those to also trigger Scribe actions is
  the most common cross-terminal IME bug.
- Non-IME chords (e.g., Cmd+T to open a tab) are typically not consumed by
  the IME on any platform, so they keep working.

---

## R9. Bracketed paste during composition

**Decision**: Bracketed paste is unaffected. Pasting clipboard text into a
pane while preedit is non-empty:
1. Commits / cancels the current preedit first (platform-default; winit
   delivers `Ime::Disabled` or a synthetic `Commit` for most IMEs when
   the user invokes paste).
2. Sends the paste bytes through the existing `perform_paste` path,
   wrapped in bracketed-paste markers per current behavior.

**Rationale**:
- The two pipelines (paste vs. IME) never interleave at the byte level
  because paste is initiated by a Scribe action (Ctrl+Shift+V / context
  menu) that flushes any in-flight composition first.

---

## R10. Cosmic-text cold-cache cost on first CJK glyph

**Decision**: Accept the cold-cache shaping cost on the first frame
containing a previously-unseen glyph; do not prewarm. cosmic-text caches
shaped runs across frames, so steady-state typing is hot-path.

**Rationale**:
- Prewarming a CJK glyph atlas at startup adds tens of MB of memory and
  measurable startup latency for users who never type CJK.
- The visible cost is one frame of slightly higher render time on the
  *first* time a glyph appears. For composition, this is during preedit
  display — well within human perception tolerance and indistinguishable
  from any other first-render of a new glyph in the terminal grid.

**Alternatives considered**:
- Prewarm an East-Asian glyph set on startup: rejected for the reasons
  above.
- Async upload off the GPU thread: orthogonal — the existing atlas path
  is synchronous and acceptable for v1.

---

## R11. Verification surface

**Decision**: Verification is **manual**, exercised against `quickstart.md`,
on each supported platform the developer has access to. The existing
122-test input-pipeline regression suite (`cargo test -p scribe-client
--lib input`) pins the non-IME byte path and must stay green.

**Rationale**:
- IME requires a real OS input method to exercise. Faking `WindowEvent::Ime`
  events in a unit test is possible but would only verify that the
  *handler* runs — not that real OS IMEs actually trigger it. The latter
  is the audit's actual concern.
- A future test that synthesises `WindowEvent::Ime(Ime::Preedit/Commit)`
  events could verify the routing logic (focus gating, encoder bypass,
  preedit-cell clearing) — this is documented as a follow-up but not in
  scope for v1.

---

## Resolved unknowns checklist

- [x] R1 — winit IME contract and platform coverage
- [x] R2 — focus-guard interaction
- [x] R3 — cursor-rect update strategy
- [x] R4 — preedit rendering approach
- [x] R5 — PTY commit routing
- [x] R6 — consumed-keystroke suppression
- [x] R7 — per-pane vs. window-global state
- [x] R8 — shortcut interaction
- [x] R9 — bracketed-paste interaction
- [x] R10 — cosmic-text cold-cache acceptance
- [x] R11 — verification surface

No NEEDS CLARIFICATION items remain. Ready for Phase 1.
