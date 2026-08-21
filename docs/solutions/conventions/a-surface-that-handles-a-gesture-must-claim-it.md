---
title: A surface that handles a gesture must claim it even when it does nothing
date: 2026-08-21
component: scribe-client (search.rs find overlay, beads_board.rs Flow strip, main.rs grid pointer path)
tags: [gpui, stop_propagation, mouse-reporting, pty, sgr, wheel, overlay, pointer]
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

## Root cause

`stop_propagation()` answers "is this gesture mine?", not "did I do something
with it?". Tying the call to a state change conflates the two. A no-op response
is still a response: the pointer was over your surface, so the event was yours,
and declining it hands a real event to whatever is painted underneath.

Both fixes are one line, and both look like nothing:

- `search.rs:654` and `search.rs:753` — an `on_mouse_up` stop sits next to each
  existing `on_mouse_down` stop, so the pairing is visible at the call site.
- `beads_board.rs:2218` — `app.stop_propagation()` moved out of the
  `if ... scroll_flow(...)` condition. `window.refresh()` stays inside it,
  because a repaint genuinely is conditional on something having changed. The
  comment at `beads_board.rs:2210` states the rule.

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

## Why it stays invisible

None of the three reproduce in a unit test, and none are visible on screen. The
leaked event only matters when the pane's application has mouse tracking on
(CSI ?1000h / ?1002h / ?1003h / ?1006h), so the symptom is a TUI behaving oddly
under a UI surface that looks correct. All three were caught by asserting on
`send_pty_bytes` output with tracking enabled, not by looking at pixels. When
adding any pointer-handling chrome over the grid, assert zero mouse-report
frames for the gestures it swallows.
