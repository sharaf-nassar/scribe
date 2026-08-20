---
title: A visual E2E click target needs a measured offset, not an ink-counting one
date: 2026-08-20
component: tests/e2e/visual, scribe-client overlays
tags: [e2e, visual-tests, gpui, click, xdotool, find, share-roster, z-order]
problem_type: convention
---

## Problem

Adding two click-driven phases to `tests/e2e/visual/find-overlay.sh` (scribe-1mpq:
click the find row's next/close controls, assert the same crop-diff phase 3/4
use for Enter/Escape) failed with `diff 0` — the click had no effect — even
though the coordinates were derived from the exact same named layout constants
(`BOX_MARGIN_TOP`, `ROW_PAD_Y`, `CONTROL_SIZE`, …) the production code uses.

## Root cause

Two separate issues, discovered by probing real pixel colours in the captured
screenshot with `convert img.png -format "%[pixel:p{x,y}]" info:` rather than
guessing:

1. The phase reused phase 5's `TITLEBAR_H=34` as "the pane's vertical offset
   from the OS window top." That constant is correct for `half_ink`, which
   only needs to exclude chrome bands from a broad pixel count and tolerates
   many pixels of slop — it is not the pane's true top edge. The real offset,
   measured off a captured frame, is 17px. Reusing a looser constant for a
   precision task (a 22px click target) silently produced a click ~17px below
   the actual control row.
2. Independent of that, the find box's right portion (the counter and all
   three controls) is *visually* hidden under the share-roster panel in this
   test's `SCRIBE_SHARED_PANE=1` rig, which always reports "2 attached."
   `roster_panel` (`crates/scribe-client/src/share.rs`) is anchored to the
   *window's* top-right (`top(px(44.0)).right(px(12.0))`) independent of any
   pane, is added to the tree after `grid` in `TerminalView::render`
   (`crates/scribe-client/src/main.rs`), and — critically — carries no
   `on_click`/`on_mouse_down`/`.id()` of its own. A click at the corrected Y
   still lands on the find control underneath: GPUI dispatches pointer events
   to whichever element in the hit path actually has a listener, not to
   whichever one painted last. The two coexisting facts (visually hidden, but
   not click-blocking) is not obvious in a screenshot alone.

## Fix

Measure once and hardcode the measured offset with a comment naming how it
was obtained, rather than a value borrowed from an approximate check:

```bash
control_center_y() {
    echo $(( WIN_Y + PANE_TOP_OFFSET + 14 + 1 + 6 + 11 ))
}
```

`PANE_TOP_OFFSET=17` came from `convert 07-next-clicked.png -format
'%[pixel:p{1090,$y}]' info:` swept over a `y` range to find the box's own
top border transition, then subtracting `BOX_MARGIN_TOP`. The remaining
arithmetic (border + row padding + half a control) mirrors
`crates/scribe-client/src/search.rs`'s named constants exactly, so only the
one empirically-measured input needed correcting.

## Prevention

When a visual E2E phase needs to click a specific on-screen element:

- Never reuse another phase's offset constant without checking what that
  phase actually needed it for (a broad ink count vs. a precise click target
  have very different tolerance for the same "chrome height" number).
- Probe real pixel colours from a captured frame to find an edge
  (`convert img.png -format "%[pixel:p{x,y}]" info:` swept over a coordinate
  range) instead of eyeballing a cropped screenshot — border anti-aliasing
  is visible in the raw values and pins the transition to within a pixel or
  two.
- A control rendering *underneath* another overlay in a screenshot does not
  mean it is unclickable — check whether the occluding element has its own
  pointer listeners before assuming z-order blocks the click. If it does
  not, GPUI's dispatch still reaches the covered control's own handler.
