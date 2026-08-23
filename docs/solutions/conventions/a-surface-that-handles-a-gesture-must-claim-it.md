---
title: A surface that handles a gesture must claim it even when it does nothing
date: 2026-08-21
last_updated: 2026-08-23
component: scribe-client (search.rs find overlay, beads_board.rs Flow strip and A2 lanes, main.rs grid pointer path)
tags: [gpui, stop_propagation, mouse-reporting, pty, sgr, wheel, overlay, pointer, hit-test]
problem_type: bug
---

## Problem

Three separate defects in this codebase are the same mistake. An element sits
over the terminal grid, handles a pointer gesture, and calls
`stop_propagation()` — but only on the half of the gesture it cared about, or
only when its own response changed something. The other half bubbles to the
workspace container, resolves to a grid cell, and is encoded to the PTY. A
mouse-tracking application (vim, htop, tmux, less) then receives an event that
never happened.

The three:

1. The find overlay stopped the left mouse DOWN and not the matching UP. The
   press never reached the PTY, so the application saw a button-up for a button
   it was never told went down.
2. The Flow strip's wheel handler called `stop_propagation()` only when the
   clamped scroll offset actually moved. Wheeling at either end of a graph
   changes nothing, so the wheel fell through and scrolled the pane behind the
   board.
3. The link path had the same hazard and was already guarded, which is what
   makes the pattern legible — see `main.rs:7665`, where a release belonging to
   a completed link gesture is dropped rather than forwarded.
4. (Found 2026-08-23.) The A2 lanes never registered a wheel handler at all,
   so every wheel over an open Beads board fell through to the pane behind it.
   Filed as `scribe-2c6p`; the lost scroll axis behind it is its own learning,
   `docs/solutions/runtime-errors/ui-rewrite-drops-a-scroll-axis-that-lived-in-the-element-type.md`.

## Root cause

`stop_propagation()` answers "is this gesture mine?", not "did I do something
with it?". Tying the call to a state change conflates the two. A no-op response
is still a response: the pointer was over your surface, so the event was yours,
and declining it hands a real event to whatever is painted underneath.

Both fixes are one line, and both look like nothing:

- `search.rs:654` and `search.rs:753` — an `on_mouse_up` stop sits next to each
  existing `on_mouse_down` stop, so the pairing is visible at the call site.
- `beads_board.rs:2327` (`2218` when this was written) —
  `app.stop_propagation()` moved out of the `if ... scroll_flow(...)`
  condition. `window.refresh()` stays inside it, because a repaint genuinely is
  conditional on something having changed. The comment above it states the rule.

## The rule

An element that swallows a press swallows the matching release. An element that
handles a wheel claims every wheel it handles. Claiming and responding are
separate decisions:

```rust
// claim unconditionally — the gesture was over this surface
app.stop_propagation();
// repaint conditionally — only if something actually changed
if boards.scroll_flow(workspace_id, travel, rect) {
    window.refresh();
}
```

## What not to do

Do not fix this downstream by teaching the grid's release path to recognise
overlays. That was considered and rejected: it creates a second place that has
to know about every overlay, which is exactly the bookkeeping that produced the
bug. The rule stays local to the element that ate the first half.

## Why a non-occluding overlay leaks wheels but not clicks

GPUI hit-tests scroll and every other mouse event differently, and the asymmetry
is easy to miss because the click half looks correct.

`Window::hit_test` (`crates/gpui/src/window.rs:938`) walks painted hitboxes
front to back, pushes every one containing the pointer into `hit_test.ids`, and
only `break`s on a hitbox with `HitboxBehavior::BlockMouse` —
`InteractiveElement::occlude()`. Then:

- `HitboxId::is_hovered` (`window.rs:629`) reads only the first
  `hover_hitbox_count` ids, so an overlay in front hides the elements behind it
  from hover styling, clicks, and moves.
- `HitboxId::should_handle_scroll` (`window.rs:666`) is a bare
  `ids.contains(&self)` — **every** hitbox under the pointer, occluders aside.
  This is deliberate upstream: a scroll should find the nearest scrollable
  ancestor even through non-interactive overlays.

So an overlay that does not `.occlude()` will correctly swallow a press it
handles and still hand the same pointer's wheel to whatever is painted
underneath. Over the terminal grid that means `scroll_pane` (`main.rs:4508`)
moves scrollback or encodes an SGR wheel report for a cell the user was never
pointing at. `stop_propagation()` in the overlay's own wheel handler is the fix;
`.occlude()` also works but changes hover semantics for everything behind, which
is why `lane_drawer` (`beads_board.rs:3197`) uses it and `board_shell`
(`beads_board.rs:2179`) does not.

## Why it stays invisible

None of the three reproduce in a unit test, and none are visible on screen. The
leaked event only matters when the pane's application has mouse tracking on
(CSI ?1000h / ?1002h / ?1003h / ?1006h), so the symptom is a TUI behaving oddly
under a UI surface that looks correct. All three were caught by asserting on
`send_pty_bytes` output with tracking enabled, not by looking at pixels. When
adding any pointer-handling chrome over the grid, assert zero mouse-report
frames for the gestures it swallows.
