# Research: Paste Confirmation (Multiline / Control-Character)

Phase 0 decisions for spec 011. All spec Clarifications were resolved during
brainstorming; the items below resolve the *implementation* unknowns the spec
deferred to planning. Each is Decision / Rationale / Alternatives.

## R1 — Gate placement and unification of the two paste paths

**Decision**: Place the confirmation gate inside
`App::send_paste_data` (`crates/scribe-client/src/main.rs:6988`), immediately
after `prepare_paste_target()` resolves the `PasteTarget` (which already
carries `bracketed: bool`) and before `try_send_single_paste` /
`send_chunked_paste`. Refactor `perform_primary_paste`
(`main.rs:9164`) so that, after fetching the primary-selection text, it calls
`self.send_paste_data(&text)` instead of inlining its own bracketed-wrap +
single-`KeyInput` send. The gate then covers all three in-scope entry points
(keybinding, context-menu Paste, middle-click) through one code path.

**Rationale**: `send_paste_message` is called from 4+ sites (the paste tail
*and* drag-and-drop and others). Gating at `send_paste_message` would wrongly
catch drag-and-drop (FR-013 excludes it). Gating in `send_paste_data` catches
exactly the in-scope paths. Routing primary paste through `send_paste_data`
also (a) removes duplicated bracketed-mode logic and (b) fixes a latent bug —
`perform_primary_paste` currently sends the whole selection in a single
`KeyInput` with no chunking, so a primary selection larger than
`MAX_KEY_INPUT_CHUNK` (4 KiB) is not chunked today. Unification gives it
chunking for free (satisfies US3 acceptance #4 / SC-006).

**Alternatives considered**:
- *Gate both paths separately, no refactor* — smaller diff, but duplicates the
  classifier call + resume logic and leaves the two-path divergence (and the
  un-chunked primary-paste bug) in place. Rejected: violates the
  reuse/no-duplication intent of Constitution I when the cleaner option is
  right there.
- *Gate at `send_paste_message`* — rejected: over-broad, would catch
  drag-and-drop.

**Setup-phase verification (carry into tasks)**: confirm the other
`send_paste_message` call sites (~`main.rs:9473`, ~`10053`) are drag-and-drop /
intentionally ungated insertions, not a clipboard/selection paste that should
be gated. If any is an in-scope paste, route it through `send_paste_data` too.

## R2 — Resume mechanism (parking the paste)

**Decision**: Store the parked `String` content **and** the resolved
`PasteTarget` (`{session_id, bracketed}`) inside `PasteConfirmationDialog`. On
**Paste**, `take()` the dialog and call the existing send tail directly
(`try_send_single_paste` → else `send_chunked_paste`) with the stored target,
**bypassing the gate** (no re-classification). On **Cancel**/Esc, drop the
dialog and send nothing.

**Rationale**: The dialog captures all input while open, so the focused pane
cannot change mid-decision; snapshotting the target at request time makes the
resume deterministic (matches the spec's "bracketed state captured at request
time" edge case). `try_send_single_paste`/`send_chunked_paste` are associated
functions taking `(&tx, &target, raw)`, so they are callable from the action
handler without re-entering the gate. Bracketed wrapping is applied by the send
tail after confirm, so the classifier still sees raw text (correct).

**Alternatives considered**:
- *Re-call `prepare_paste_target()` on confirm* — would re-run scroll
  side-effects and re-resolve focus; unnecessary and slightly less
  deterministic. Rejected.
- *Store a resume closure* — heavier; the `(text, target)` pair is sufficient.

## R3 — Classifier definition (the trigger byte-set)

**Decision**: A pure free function
`classify_paste(text: &str) -> Option<PasteRisk>` where
`PasteRisk { has_line_break: bool, has_control: bool }`. Iterate chars:
`'\n' | '\r'` → `has_line_break`; `'\t'` → ignore; any other
`char::is_control()` → `has_control`. Return `Some(risk)` iff
`has_line_break || has_control`, else `None`.

**Rationale**: `char::is_control()` is exactly C0 (U+0000–U+001F) ∪ DEL
(U+007F) ∪ C1 (U+0080–U+009F). Matching `\n`/`\r`/`\t` *before* the
`is_control()` arm yields precisely the spec's set (FR-002/FR-004): line breaks
trigger "multiline"; tab is excluded; everything else control triggers
"control". Pure and allocation-free → trivially unit-testable and O(n).

**Alternatives considered**:
- *Byte scan instead of char scan* — equivalent for the ASCII control set but
  would misclassify multi-byte UTF-8 continuation bytes (0x80–0xBF look like C1
  at the byte level). `char` iteration is correct for C1 detection. Chosen.

## R4 — Preview rendering (caret-escape + truncation)

**Decision**: New helper in the same module renders the parked content for the
dialog body as `Vec<String>`:
- Split into lines on `\n`. Show at most `MAX_PREVIEW_LINES = 8` lines; if more,
  append a summary line `… (+N more lines)`.
- Within each shown line, replace control/escape bytes with **caret notation**:
  C0 byte `b` → `^` + `(b ^ 0x40)` (e.g. `ESC` → `^[`, `CR` → `^M`, NUL →
  `^@`), `DEL` → `^?`, C1 → `\u{NN}` form. Tabs shown as a small fixed run of
  spaces (legible, non-triggering). Never emit a raw control byte into the
  glyph stream (FR-005 / SC-008).
- Truncate each rendered line to `MAX_PREVIEW_COLS = 56` via the existing
  `truncate_for_display` head/tail-ellipsis helper pattern (matches
  disallowed-scheme dialog's `BODY_URI_MAX_COLS = 56`).
- The reason line summarizes counts, e.g. `12 lines · 3 control characters`.

**Rationale**: Reuses the proven `body_lines()` → `draw_body()` (`row = 4 + i`)
multi-line layout and `truncate_for_display`. Caret notation is the standard,
compact, unambiguous control-byte visualization and guarantees the preview
itself can't drive the terminal/dialog (the whole point of the trust case in
US2). Concrete caps avoid unbounded dialogs on huge pastes (Edge Cases /
PR-001).

**Alternatives considered**:
- *Unicode control pictures (U+2400 range)* — visually nice but depends on font
  glyph coverage (the audit flags Scribe's fallback gaps); caret notation is
  ASCII and always renders. Chosen.
- *Show full content* — rejected; unbounded dialog height + perf risk.

## R5 — Dialog template

**Decision**: New `PasteConfirmationDialog` + `PasteConfirmationAction
{ Paste, Cancel }` cloned from
`crates/scribe-client/src/disallowed_scheme_dialog.rs`: same `DialogColors`,
`DialogLayout`, `DialogRenderer`, `build_instances(ctx)` shape, `ButtonIndex`
two-button model with **Cancel = index 0 = default focus**, `focus_next/prev`,
`confirm`, `update_hover`, `click`. Body via `body_lines()` (reason +
caret-escaped preview from R4). Stored as
`App.paste_confirmation_dialog: Option<…>`; rendered next to the other dialogs
(`main.rs` ~5395) and gated in the window-event router (`main.rs` ~1723) by the
same `is_some()` pattern.

**Rationale**: Spec 009/010 set the precedent of reusing this exact chrome;
matches UX-001 and minimizes new surface. Cancel-as-default-focus mirrors the
disallowed-scheme dialog's safe-default convention.

**Alternatives considered**: A bespoke dialog — rejected (no reason to diverge;
Constitution III).

## R6 — Config field + settings wiring

**Decision**: Add `#[serde(default)] pub paste_confirmation: bool` to
`TerminalConfig` (after `clipboard_policy`) and `paste_confirmation: false` to
`impl Default for TerminalConfig`. Wire settings exactly like the spec-010
`terminal.clipboard.focus_gate_writes` toggle: add `"terminal.paste_confirmation"`
to the `apply_terminal_key` match arm → `apply_terminal_behavior_key` (parse
bool → assign); add a `<div class="toggle off" data-key="terminal.paste_confirmation">`
block on the Terminal page in `settings.html`; add
`setToggleValue("terminal.paste_confirmation", config.terminal?.paste_confirmation ?? false)`
in `settings.js`. The generic toggle click handler dispatches `sendChange`
automatically — no extra JS.

**Rationale**: `#[serde(default)]` on a bool yields `false` with no
`default_false` fn needed (confirmed). This is the established, lowest-surface
path; live reload via the existing file-watcher → `ConfigReloaded` round-trip
needs no new code (the gate reads `self.config.terminal.paste_confirmation` at
paste time).

## R7 — Compatibility decision (Constitution: config change)

**Decision**: The change is additive and backward compatible. Existing
`config.toml` files without `terminal.paste_confirmation` deserialize to
`false` (feature off = byte-for-byte today's behavior). **No migration, no
version bump, no protocol change.** Downgrade-safe: an older client simply
ignores the unknown key.

**Rationale**: Satisfies the constitution's config-change migration/
compatibility requirement while imposing zero upgrade cost. Default-off also
satisfies the explicit user requirement and guarantees no behavior change for
anyone who doesn't opt in (SC-005).

## R8 — No server / protocol involvement

**Decision**: The feature is entirely client-side. Paste originates from the
client's own clipboard/primary selection; the bracketed-paste signal is read
from the focused pane's `Term` mode in the client. No `ServerMessage` /
`ClientMessage` / `AutomationAction` variants are added or changed.

**Rationale**: Unlike OSC 52 gating (spec 010), where a PTY-side program
initiates and the server must mediate, here the client already holds both the
content and the decision signal. Adding a server round-trip would be pure
overhead and would widen the change surface for no benefit (Constitution I).
This is the single biggest scope reducer versus the OSC 52 precedent.
