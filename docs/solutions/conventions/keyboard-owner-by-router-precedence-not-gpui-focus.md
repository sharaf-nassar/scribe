---
title: An overlay with no caret is not a focus bug when the router owns the keyboard
date: 2026-08-23
component: crates/scribe-client (search.rs find overlay, main.rs key router), tests/e2e/visual
tags: [gpui, focus, overlays, find, text-input, e2e, wire-tap, test-oracle, winit-port]
problem_type: bug
---

## Problem

"I can't tell when the find field is focused, and Ctrl+A does nothing in it."

Both symptoms point at focus, and the repo already has a documented focus trap
for exactly this surface
(`docs/solutions/conventions/focus-allowlist-revokes-new-overlay-controls.md`).
Neither symptom is a focus bug.

## Root cause

**The find overlay never uses GPUI focus to own the keyboard, and does not need
to.** `TerminalView::handle_overlay_key` claims every keystroke for the open
overlay by *router precedence* and returns `true`
(`crates/scribe-client/src/main.rs:9671-9674`), so the keys arrive regardless of
where GPUI focus sits. `open_find_overlay` (`main.rs:8890-8907`) never calls
`window.focus`; the overlay's `focus_handle` (`search.rs:410`) is focused only
by a pointer click on one of the three row controls (`search.rs:748-750`), which
is the fix scribe-2yw1 landed for a different problem.

So the missing "focused" affordance is not focus at all. It is two absences:
`FindOverlayView::render` paints the query as one static text child with no
caret element (`search.rs:653-662`), and `FindOverlayColors::from`
(`search.rs:346-358`) derives exactly one colour set, giving the typed query
`chrome.status_bar_text` — dimmer than the `chrome.tab_text_active` the far less
important `n/m` counter beside it already uses (`search.rs:352,355`).

The dead Ctrl keys are a second, independent mechanism.
`handle_find_overlay_key` computes `claimed_by_modifier` from
control/alt/platform and returns early for any such keystroke that is not
escape/enter/up/down/backspace/delete (`main.rs:8998-9000, 9010-9012`). Even if
the key arrived, `FindOverlayView` is a bare `query: String` with no caret index
and only end-anchored mutators (`search.rs:481-512`), so caret motion is not
expressible.

Both are original gaps, not regressions: the winit predecessor's
`handle_search_overlay_keyboard` (commit `528a932`) had the same table and the
same append-only model.

## What didn't work

**Reaching for focus plumbing.** The instinctive fix — focus the handle on open,
render the caret and the bright state off `is_focused(window)` — costs a
`&mut Window` threaded through `dispatch_key_action` (`main.rs:3546-3555`) and
`open_find_overlay`, which have `Context` only, and buys nothing: the overlay is
the keyboard owner from the moment it opens. The one state where it is open and
*not* the owner is a modal stacked over it (`dialog` and
`handle_modal_or_editor_key` are checked first, `main.rs:9555, 9656`), and a
modal visually covers the box anyway.

**Trusting the follow-up history as coverage.** Three beads polished this
overlay — scribe-1mpq (pointer controls), scribe-2yw1 (their focus and
tooltips), scribe-uu2y (the mouse-up pairing) — plus a full visual E2E with
eight phases. Every one of those oracles exercises opening, cycling, clicking,
and closing. None of them types more than one word, so nothing ever asked
whether the field is a text input.

**Assuming a GUI text field needs a pixel or OCR oracle.** It does not, here.

## Fix

Filed as **scribe-p82i** (caret, selection, clipboard, Ctrl key table),
**scribe-h3zf** (brighter chrome), and **scribe-00ym** (the scroll-to-match gap
found alongside them); all three unlanded as of this writing.

The shape that matters: **make the caret and the bright state unconditional
while the overlay is open**, with a `ponytail:` comment naming the
modal-stacked-over-find case as an accepted simplification. No `window.focus`,
no focus-state machine, no signature churn.

## Prevention

**Read a text surface's mutators before believing it is a text field.** A struct
with `push_char` / `pop_char` / `clear_query` and no index is a display label
with an append affordance. `command_palette.rs:404-455` has the identical shape
and the identical gap; the settings inputs say so out loud
(`settings/window.rs:1700-1703`: "Input is append-only ... the caret is at the
end").

**A wire-visible payload is a better text-field oracle than pixels.** Every
settled find query leaves the client as `SearchRequest` carrying the literal
string, so the share wire tap (`SCRIBE_SHARE_TAP=1`) reports exactly what the
field holds — no OCR, no crop diffing. Typing `eedle`, pressing Ctrl+A, then
typing `n` recorded `eedle` then `eedlen` where a working caret would have sent
`needle`. Prefer this oracle for any surface whose state reaches the wire; a
700 ms idle crop diff was still useful for the second half ("0 changed pixels"
proves no caret is drawn at all).

**A throwaway E2E probe costs one `docker run`, not an image rebuild.** The
recipes bind-mount `tests/e2e` read-only, so a new script under it runs against
the current image with no rebuild — see
`docs/solutions/environment/e2e-recipes-mount-tests-so-shell-only-changes-skip-the-image-build.md`.
Check `just e2e-visual-image-current` first, then copy the target script's rig
flags (`SCRIBE_SHARED_PANE=1 SCRIBE_SHARE_TAP=1` for find) into a direct
`docker run`. Write the probe in a throwaway worktree so the primary checkout
stays clean.

**A winit-to-GPUI port keeps the shape and drops the verbs.** The same triage
that found the dead Ctrl keys found `scroll_focused_pane_to_search_match()`
missing entirely: the winit client called it after every match cycle
(`528a932`), and the GPUI `next_match` / `prev_match` only move an index
(`search.rs:518-536`). When auditing a ported surface, diff the *call sites* of
the old handler against the new one, not just the key table.
