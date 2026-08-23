---
title: A UI rewrite silently drops a scroll axis that lived in the element type
date: 2026-08-23
component: scribe-client (beads_board.rs A2 lanes, beads_board_a2.rs presentation model)
tags: [gpui, uniform_list, scroll, wheel, regression, beads-board, overlay]
problem_type: bug
---

## Problem

The workspace Beads board's A2 lanes stopped scrolling. A lane shows three
whole rows at the default 197px strip, marks the rest with a `⌄` cue, and the
wheel moves nothing — while the terminal pane behind the board scrolls instead.
The only way to reach a hidden row is to drag the strip's floor grip taller or
shrink the text scale.

Nothing in the board looks broken. There is no error, no warning, no failing
test, and the clipping is exactly what `specs/028-beads-board-contract.md`
A2-S6 asks for ("only whole rows show and `⌄` marks hidden rows"). The spec has
no A2 wheel row at all, so no contract check noticed the axis was gone.

## Root cause

The scroll axis was never written down in this repo's code. It came free with
the GPUI element type.

Before `8fdf68a` ("feat: replace the raised-card board with the A2 ledger
renderer") the lane body was a GPUI `uniform_list`, which sets
`base_style.overflow.y = Some(Overflow::Scroll)` on construction
(`crates/gpui/src/elements/uniform_list.rs:32`). That one line is what makes
`Interactivity::paint_scroll_listener` (`crates/gpui/src/elements/div.rs:3077`)
install a wheel handler. The lane scrolled because of the type, not because
anyone asked it to.

`8fdf68a` replaced the list with a fixed-height clipped div —
`.h(px(height)).overflow_hidden()` at `beads_board.rs:2862` — and moved the row
cap into the pure presentation model, where
`items.get(..items.len().min(visible_rows))` (`beads_board_a2.rs:327`) drops the
surplus rows before paint. Both halves compile clean: `overflow_hidden`
is a legitimate style, and a slice that discards its tail is a legitimate slice.
The behaviour that vanished had no symbol to go missing.

The one artefact that survived is a comment at `beads_board.rs:2276`, which
still explains that the Flow strip claims the wheel "rather than on the board
root because ... in lanes the same gesture belongs to the lane bodies
underneath". Those lane bodies stopped having an axis in the same commit; the
comment now names an owner that does not exist. Prose describing a behaviour is
not a check on it.

Fix filed as `scribe-2c6p`, unlanded as of this writing.

## What didn't work

Looking for the removal in the diff. `git log -S"overflow_y_scroll"` and
`-S"uniform_list"` on `beads_board.rs` return the commits that *added* the
scrolling and the commit that removed it, but `8fdf68a` is a 6000-line renderer
replacement whose message is entirely about the new visual grammar — the lost
axis is invisible in the summary and invisible in review. What actually located
it was grepping the whole client for `on_scroll_wheel` and finding exactly two
hits, neither in the A2 path.

Reading `specs/028-beads-board-contract.md` first also did not help, and was
briefly misleading: A2-R1 says "A2 never scrolls horizontally" and A2-S6
describes the whole-row clip, which together read as a deliberate no-scroll
design rather than a gap. The spec is silent on the vertical wheel, and silence
is not a decision.

## Prevention

- When a rewrite swaps one GPUI element type for another, list the styles the
  old type set implicitly. `uniform_list` and `overflow_y_scroll`/`_x_scroll`
  carry `Overflow::Scroll`, and that is the whole mechanism behind their wheel
  handling — `list`, `div`, and a plain `.children()` body carry nothing.
- A clipping container is a claim that content is unreachable on purpose. If
  the clip is paired with an overflow cue (`⌄`, an edge fade, a count), there
  must be some gesture that reaches what the cue points at, or the cue is
  advertising a dead end.
- Put the wheel in the contract. A2 had rows for geometry, colour, and every
  pointer gesture except this one, so the omission was structurally invisible
  to `just parity-inventory` and to the functional suite alike.
- If a row cap moves from the paint layer into a pure presentation model, the
  scroll offset has to move with it. Windowing in the model is the right call
  for perf (see `scribe-jfob`, which filed unvirtualised 200-row lanes as a P1
  perf bug), but a window with no offset input is a truncation.
