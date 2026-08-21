---
title: GPUI layout and hover chains fail silently, and only measurement catches them
date: 2026-08-21
component: scribe-client (beads_board.rs collapsed-lane drawer, beads_flow.rs Flow strip)
tags: [gpui, layout, absolute, relative, occlude, hover, hitbox, refresh, e2e, screenshots]
problem_type: bug
---

## Problem

Three defects landed in one board renderer, survived unit tests, code review,
and a full `just ready`, and were each caught only when a rewritten E2E suite
measured real screenshots against the approved mock. All three are GPUI style
or hit-test chains that compile, run, and do the wrong thing without any error.

1. **A trailing `.relative()` silently overrides an earlier `.absolute()`.**
   The hover drawer set `.absolute().top().bottom().right().w()` and then
   `.relative()` five lines later. GPUI's generated style methods each assign
   `position`, so the last one wins: the drawer fell back into the lanes' flex
   row and `right` became an offset from its static position. It painted at
   x≈917 on a 1008px window instead of the contract's 460..912 box.

2. **`occlude()` makes ancestor hitboxes report not-hovered.** `occlude()` sets
   `HitboxBehavior::BlockMouse`, and every hitbox behind it — including the
   board's own — then reports `is_hovered() == false`. A hovered board read the
   pointer entering its own drawer as the pointer leaving the board, and took
   the drawer down mid grace transfer. The drawer now also reports hover for
   the board (`beads_board.rs:3144`, using `HoverSource::Control`, because
   `Board` would be cleared again by the shell's own leave in the same pointer
   move).

3. **Mutating state in an `on_hover` without asking for a frame.** Both
   collapsed-lane hover callbacks changed lane state and never called
   `window.refresh()`, so the tab went hot (pure CSS hover) while the drawer
   waited for some unrelated repaint. Their sibling `on_click` handlers already
   refreshed, which is why click-to-pin worked and hover-to-open did not.

## The compounding failure

Defects 1 and 2 stacked. While the drawer painted off-screen, the pointer never
landed on it, so its occluding hitbox was never under the cursor and defect 2
could not fire. Fixing the bounds is what exposed it. Expect a layout fix to
uncover the hit-test bug that was hiding behind it.

Defect 1 also produced a paint-versus-hit-test split: `beads_board_a2::queue_at`
resolved the drawer at the correct contract bounds the whole time, so the drag
target was right and only the paint was wrong. A drop over the visible drawer
resolved to the tab underneath. When paint and hit-testing disagree, check
which one is wrong before "fixing" either.

## The same class, one layer up

`FlowRender` carried a `rect` and re-applied the board's absolute origin on top
of the strip slot that already had it, so every board except the one at x=0
drew its graph one region-width away. The fix was not to subtract the offset at
the call site but to delete the field: the render input now takes only
`viewport_width` (`beads_flow.rs:804`) and its root fills the slot it is given,
which makes a second origin unrepresentable rather than merely unused. Prefer
making the bad state impossible over correcting it at each caller.

## Why unit tests miss all of this

Every one of these is a property of the composed, rendered frame — final
position, hitbox occlusion, whether a frame was requested. A pure layout model
can assert the numbers it computes, and this repo has a good one
(`beads_board_a2.rs`), but the model was CORRECT in all four cases. The bug was
always in the translation from model to painted element.

What actually caught them: E2E suites that measure painted geometry from real
screenshots and compare against a generated contract manifest, plus a headless
GPUI probe that reads back an element's real `Bounds`. A bounds probe is what
settled whether the Flow strip was never built or built in the wrong place —
the two look identical from a region screenshot, and reasoning from source
alone had previously left that exact question open and unverified.

When adding or moving absolutely-positioned chrome, assert its measured bounds,
not that it exists.
