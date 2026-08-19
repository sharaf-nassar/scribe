---
title: Terminal scrollbar does not appear under a pointer that never moved
date: 2026-08-18
component: scribe-client (main.rs pointer wiring, scrollbar.rs), tests/e2e/visual
tags: [gpui, hover, mouse-move, scrollbar, edge-triggered, level-triggered, xdotool, e2e]
problem_type: bug
---

## Problem

Hovering the right-edge strip of a terminal pane sometimes reveals the overlay
scrollbar and sometimes does nothing. The obvious hypothesis — hover-to-reveal
was never wired — is wrong: moving the pointer into the hit zone reveals the
bar reliably, in a single pane (4800px of right-edge strip change) and on both
halves of a side-by-side split (4539px inner edge, 619px outer edge).

What fails is the case where the pointer is *already* in the zone when the
scrollbar becomes revealable. Park the pointer at the pane's right edge with no
scrollback, inject scrollback over the wire so the mouse is never touched, and
the strip changes 0px. One pixel of mouse motion, still inside the same zone,
and it jumps to 3171px.

## Root cause

`ScrollbarState::hover` is an edge-triggered cache. It is written only by
`TerminalView::update_scrollbar_hover`
(`crates/scribe-client/src/main.rs:7417`), whose sole caller is
`move_over_grid` (`main.rs:7224`), registered as the grid band's
`on_mouse_move` listener (`main.rs:9321`).

The value it caches is a *level* condition over three inputs that all change
without the pointer moving:

- `metrics.history_size` — `hit_test_scrollbar` returns false at zero
  (`crates/scribe-client/src/scrollbar.rs:572-574`), so a pane that gains
  scrollback under a resting pointer keeps `hover=false`.
- the pane's painted rect, read through `scrollbar_layout` (`main.rs:7391`) —
  splits, pane closes, resizes and zoom move the hit zone under a stationary
  pointer.
- `scrollbars.panes` membership — a new session mints
  `ScrollbarState::default()` with `hover: false` (`main.rs:6471`).

The same mechanism fails in the opposite direction, and that half is worse than
cosmetic. Leave the window through the hit zone and the last in-window motion
was still inside it, so nothing clears the flag. `tick_fade_at` short-circuits
to `opacity = 1.0; return true` while `hover` is set
(`scrollbar.rs:324-328`), so the overlay never fades *and*
`poll_scrollbar_fades` (`main.rs:7368`) calls `cx.notify()` on every 16 ms idle
tick forever — the window repaints at 60 fps with the pointer parked on another
application. Measured: 2928px still lit four seconds after the pointer left,
against a 1.5 s idle delay plus a 0.3 s ramp.

`update_jump_hover` (`main.rs:7449`) is the same hand-rolled pattern on the same
call site and goes stale identically. `prompt_hover` is not affected — it routes
through gpui's own level-triggered `on_hover` (`main.rs:9155`), which is the
shape the other two should have had.

## What didn't work

Reading the code to find the defect. Every path in `update_scrollbar_hover`,
`hit_test_scrollbar`, `build_scrollbar_render` and `tick_fade_at` is correct in
isolation, and the state transitions trace cleanly from `on_hover_enter` to a
painted thumb. Time went into ruling out plausible-looking mechanisms that were
all dead ends: stale per-session bounds in `pane_at` (retired correctly by
`prepare_pane_bounds`, `main.rs:823`), an occluding hitbox swallowing the move
(`HitboxBehavior::BlockMouse` appears nowhere in the terminal window), and a
one-frame-stale hit test (`dispatch_mouse_event` recomputes it against the
current pointer position before dispatch). The bug is not in any single
function; it is in *when* the correct function is called.

The existing oracle hides it by construction: `tests/e2e/visual/scrollbar.sh`
only ever hovers *after* a `shift+Prior`, and its own header explains why —
hover is used there to pin the overlay open so captures are not racing the
fade. Hover-as-reveal is never asserted, so the level/edge distinction has no
coverage.

Two e2e traps cost real time:

- `xdotool mousemove` warps the pointer. It emits one motion event at the
  destination and none along the path, so "move the pointer out of the window
  to the left" does not generate the intermediate in-window motion a real mouse
  would, and a control built that way reports a stuck hover that a human user
  would never see. Only the exit direction that genuinely ends inside the hit
  zone — out the right edge — is a faithful repro.
- Opening a fresh tab with `ctrl+shift+t` and typing filler into it via
  `xdotool type` produced no scrollback at all, so the phase's own control
  failed and the run was inconclusive. Injecting through
  `scribe-test send "$SESSION"` against the shared-pane rig is what makes the
  "mouse is never touched" condition both true and observable.

## Fix

Make the hover pass level-triggered instead of motion-triggered: cache the last
pointer position on `PointerState` (`main.rs:1471`), clear it from a grid-band
`on_mouse_exit` listener so a pointer that leaves the window drives every pane's
`on_hover_leave`, and re-run the pass from the existing 16 ms idle tick at the
top of `poll_scrollbar_fades` before `tick_scrollbar_fades`. The idle tick
already visits every scrollbar state, so this needs no second timer and no new
state machine.

Filed as `scribe-re54` (P2), with `scribe-jjbm` (P3) depending on it for the
jump-chip cache that reuses the same refresh site.

Landed in `b65b5c4` for `scribe-re54`, exactly as described above. Two details
the approach did not anticipate, both worth knowing before attempting
`scribe-jjbm` against the same site:

- The regression phases had to be placed immediately after phase 1 of
  `tests/e2e/visual/scrollbar.sh`, because that is the only point at which the
  shared pane still has zero scrollback. The former phase 9 control lost that
  shared tab as a result and now opens and fills its own. Phase numbers in that
  script therefore no longer match source order, which is documented inline.
- Adding the `on_mouse_exit` listener inline pushed `render_grid` to 81 lines
  against this repo's 80-line clippy ceiling. The handler is a named
  `forget_pointer_position` method for that reason, not for style. See
  `docs/solutions/conventions/lint-suppression-allowlist-is-counted.md`.

`scribe-jjbm` remains open and can reuse `PointerState::last_position` and the
`on_mouse_exit` listener directly.

## Prevention

A hover flag is a cache of a predicate, not an event log. When the predicate
reads anything other than the pointer — geometry, content size, collection
membership — an `on_mouse_move`-only writer is wrong by construction, and the
failure is invisible in any test that moves the mouse last. Either drive it
from a level-triggered source (gpui's own `on_hover`, or a per-frame
recomputation on a tick that already runs) or assert the stationary-pointer
case explicitly.

For visual coverage of any hover affordance, the reveal phase must move the
pointer *before* the condition becomes true, not after. A phase that hovers and
then makes the condition true is testing the mouse-move handler; a phase that
hovers, then changes the world, is testing the affordance.
