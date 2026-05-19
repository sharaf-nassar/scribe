# Quickstart: Manual Verification

Per QR-002 / Constitution II, each user story is verified manually (no new automated test
code requested). Each story is independently testable. Run against a locally built
`scribe-client` (do **not** restart the user's live server — start a separate dev instance if
needed, with user approval).

## US1 — Protocol-aware apps receive unambiguous keys (P1)

**Setup**: a key-reporting probe — `kitty +kitten show_key -m kitty`, or Neovim/Helix with a
keylog, or a tiny script that enables `CSI = N u` and prints received bytes.

1. **Disambiguate (flag 1)**: enable `CSI = 1 u`. Press `Ctrl+I`, `Tab`, `Shift+Enter`,
   `Alt+.`, bare `Esc`, `Ctrl+Shift+A`. ✅ Each yields a *distinct* CSI-u sequence; `Ctrl+I`
   ≠ `Tab` (no collapse to `0x09`). (FR-001, SC-001)
2. **Event types (flag 2)**: enable `CSI = 2 u`. Hold a key, release it. ✅ Repeat events
   carry `:2`, release carries `:3`; press carries `:1`/none. (FR-002, SC-002)
3. **Subset**: enable only flag 1. ✅ No event-type/alternate/text fields appear. Enable
   `CSI = 21 u` → alternate + associated text fields appear. (FR-003)
4. **Push/pop**: app pushes flag 1, runs a child that pushes flag 31, child exits (pop). ✅
   After pop, encoding reverts exactly to flag-1 behavior. (FR-003)
5. **Legacy non-regression**: in a plain shell (no negotiation) capture input bytes for a
   broad keymap; toggle `keyboard_protocol_enhanced=false` and repeat. ✅ Byte-identical to a
   pre-feature build and identical between true/false when nothing negotiates. (FR-004,
   FR-006, SC-003)
6. **Multi-pane**: pane A negotiates flag 31, pane B legacy shell. Type in each. ✅ No
   cross-pane leakage; B stays legacy. (FR-005, SC-008)
7. **Non-regression**: bracketed paste, a dead-key/compose sequence, and the Codex
   `Alt+Enter` newline still behave exactly as before. (Edge cases)

## US2 — Failed commands are immediately visible (P1)

**Setup**: a shell with Scribe shell integration active.

1. Run `true` then `false`. ✅ Scrollbar tick for `false` is visually distinct (failure
   color) from `true` (success color); no scroll/hover needed. (FR-007, FR-008, SC-004)
2. ✅ Status bar shows the latest outcome as a non-color **glyph** (✓ / ✗ / neutral) — the
   authoritative cue — and it flips success→failure→success as commands run; confirm the
   glyph itself changes (not only its color). (FR-009)
3. Start a long-running command (`sleep 30`); while running ✅ its record shows neutral, not
   failure. Run a command whose shell integration omits the exit code (or `Ctrl+C` at the
   prompt) ✅ shown neutral/unknown, never red. (FR-012, SC-006)
4. Fill scrollback past the cap so old rows trim. ✅ Surviving status ticks stay aligned to
   their original command rows (0 drift). (FR-013, SC-007)
5. Two panes, fail a command in one. ✅ Only that pane reflects failure. (FR-014 isolation,
   SC-008)
6. Detach the client and reattach (and separately, cold-restart restore). ✅ Historical rows
   show neutral/unknown (no fabricated status); new commands accumulate normally. (FR-014)

## US3 — Jump to commands and failures (P2)

**Setup**: scrollback seeded with several commands incl. ≥1 failure, scrolled to top.

1. Invoke prompt-jump up/down. ✅ Viewport moves between command boundaries, keyboard-only,
   same as before. (FR-010)
2. Invoke `jump_to_failure`. ✅ Viewport lands on the most recent failed command in one
   action. (FR-011, SC-005)
3. Next-command past the newest entry. ✅ Sensible bound, no wrap surprise, no crash.
4. Remove the only failure (trim it off by overflowing scrollback), invoke `jump_to_failure`.
   ✅ Non-disruptive "nothing to jump to"; with no failures at all, same. (FR-011)

## Performance check (PR-001)

- **Input latency**: in a Kitty-negotiating full-screen app, run a scripted keystroke flood
  (e.g. `xdotool`/macOS equivalent) and observe frame pacing / input responsiveness vs. a
  legacy app. ✅ No perceptible added latency; no dropped frames attributable to encoding.
- **Render**: scroll a 10,000-line scrollback containing many command records. ✅ Scroll
  frame rate unchanged vs. a pre-feature build (scrollbar loop stays O(marks) + one enum
  compare).
- Record the exact command/observation used in the completion report.

## Pre-completion gate

- [ ] All US1/US2/US3 scenarios pass on Linux; spot-check on macOS.
- [ ] `lat.md` updated (client.md Key Translation Priority + Scrollbar Prompt Mark
      Indicators + IPC Client; pty.md OSC 133 cross-ref) and `lat check` passes.
- [ ] Completion report names the verification commands run + residual risk.
