---
title: Find overlay opens at the window corner instead of the focused pane
date: 2026-08-18
last_updated: 2026-08-21
component: scribe-client (search.rs, main.rs), GPUI layout
tags:
  [
    gpui,
    overlay,
    absolute,
    positioning-ancestor,
    panes,
    find,
    mount-point,
    flexbox,
    min-width,
    overflow,
  ]
problem_type: bug
---

## Problem

`ctrl+shift+f` opens the find box at the top-right corner of the whole
window, not of the pane being searched. In a vertical split with the LEFT
pane focused, the box paints entirely inside the unfocused RIGHT pane. It
also overlaps the tab-bar chrome band, and on a shared window the share
roster paints over the query field.

## Root cause

The overlay's style says nothing wrong. `FindOverlayView::render` uses
`.absolute().inset_0()` with `justify_end()` / `items_start()`
(`crates/scribe-client/src/search.rs:571-580`) — a perfectly ordinary
top-right anchor. What decides *which* top-right corner is where the entity
is mounted in the child tree: GPUI resolves `absolute` against the nearest
positioned ancestor, and the overlay is a child of the window root div
(`crates/scribe-client/src/main.rs:9991`), which is `.relative().size_full()`
(`main.rs:9979-9980`). The focused pane's rect is not an input to the layout
at all.

The rest follows from the same fact. `inset_0` on the root spans the
titlebar and tab strip, hence the chrome overlap; and the share roster
(`main.rs:9994`) is a later sibling of the same parent, hence the occlusion.
One mount point, three symptoms.

The mismatch is with what find actually searches:
`TerminalView::send_search_request` targets `shared.active_session`
(`main.rs:7610`) and highlights are computed only for `placement.focused`
(`main.rs:6555-6564`). The surface is pane-scoped everywhere except in its
own geometry.

The crate already had the correct idiom in place: the jump-to-bottom button
is mounted in the pane's own `.relative()` `grid_slot`
(`main.rs:6629-6640`), which is also the one container that excludes the
prompt strip in both `PromptBarPosition` settings.

## What didn't work

- Reading `lat.md/client.md` "GPUI Find Overlay" first. It describes the
  surface as "a top-right box", which is true of the winit original and of
  the port, and reads as intended behaviour rather than a defect. The
  documentation named the symptom as design intent, so it could not be used
  as the oracle here.
- The shipped `tests/e2e/visual/find-overlay.sh` contract. It is single-pane
  end to end, and in a single-pane window the window's top-right corner and
  the focused pane's top-right corner are within a chrome band of each
  other. Every phase passes with the bug present.

## Fix

Mount the overlay in the focused pane instead of the window: drop
`.children(self.find_overlay.clone())` from the root and attach it to the
focused pane's `grid_slot` in `TerminalView::compose_pane_content`, exactly
where `jump_button` already goes; then scope the overlay's backdrop to the
pane and clamp the fixed `w(px(360.0))` (`search.rs:588`) so a narrow split
shrinks the box instead of clipping it under `grid_slot`'s
`overflow_hidden`. Filed as `scribe-pty2`; the pointer controls and restyle are
`scribe-1mpq`, blocked on it.

Landed in `48f81e3` for `scribe-pty2`, as described. The regression phase is a
proper negative control: with the split's LEFT pane focused it measured
left +0 / right +342 before the fix and left +550 / right +0 after, so the
assertion fails on the old mount point rather than merely passing on the new
one. The 360px box is clamped with `max_w` plus a 200px `min_w` floor; a pane
narrower than that floor still clips against `grid_slot`'s `overflow_hidden`,
tracked as ponytail debt rather than solved.

The reproduction that made it measurable: split the pane, focus LEFT, open
find, and count lit pixels per window half with the `half_ink` helper from
`tests/e2e/visual/pane-workspace-layout.sh:120`. Opening find added +725
pixels to the RIGHT half and +42 (cursor-blink noise) to the LEFT. That
inversion is the regression assertion.

## Prevention

When an overlay lands in the wrong place, read its mount point before its
style. In GPUI a positioned element is only as scoped as its nearest
`relative()` ancestor, so an entity held in a `TerminalView` field and
splatted into the root's child list is window-scoped no matter what its own
`render` says. Before adding a `.children(...)` for a new overlay, ask which
rect it is conceptually attached to and mount it under that rect's container
— the pane's `grid_slot`, the region's rect, or the root — rather than
positioning it after the fact.

A surface that resolves its *data* per pane (`active_session`,
`placement.focused`) and its *geometry* per window is the smell. Those two
should agree.

## Follow-up: the narrow-pane floor left behind here was the wrong knob

The `min_w(200)` floor above was filed as ponytail debt (`scribe-cu7f`) with
the upgrade note "widen the floor if a narrower split turns out to matter".
Triaging that debt on 2026-08-21 measured the geometry with
`VisualTestContext::debug_bounds` and found the note points the wrong way.

The box is `w(360) max_w(360) min_w(200)` under a `justify_end` parent
(`crates/scribe-client/src/search.rs:629-638`). `min_w` is what blocks
flex-shrink, so below a 214px pane (200 + the 14px right margin) the box's
left edge goes negative:

```
pane 120 -> box left  -94.0  width 200.0
pane 200 -> box left  -14.0  width 200.0
pane 214 -> box left    0.0  width 200.0
pane 374 -> box left    0.0  width 360.0
```

That is one gesture away, not a corner: `layout.rs:175` sets
`MIN_RATIO = 0.1` with no pixel floor, so a divider dragged to the clamp on a
1200px window leaves a 120px pane.

Both obvious moves on the floor make things worse. Widening it pushes the left
edge further negative. Setting it to `0` lets the box shrink correctly — but
the row's `flex_none` children (the 50px counter plus three 22px controls plus
gaps, roughly 170px that cannot shrink) then spill out the box's *right* side,
so the controls get clipped instead of the query text. The floor was never the
knob; the overflow *direction* is. What holds is `min_w(px(0.0))` plus
`.overflow_hidden()` plus `.justify_end()` on the box, with `.min_w(px(0.0))`
on the flexible query child — overflow then leaves from the start edge and the
controls are the last thing to go. Measured good down to an 80px pane. Filed as
`scribe-kx46`; unlanded as of this writing.

Two things generalise. First, in flexbox a fixed `min-width` on a
right-anchored box does not "floor" it gracefully — it converts a shrink into
an overflow, and under a clipping ancestor an overflow is invisible rather
than ugly, so it never shows up in the single-pane fixture. Second, a
`ponytail:` note that names an upgrade path is a hypothesis, not a finding: it
is written at land time by someone who did not measure the failure mode. Treat
the trigger as real and the prescription as unverified, and measure before
following it.

For the geometry half, `debug_bounds` is the tool: tag the element with
`.debug_selector(|| "…".to_owned())` (gpui compiles it to a no-op in release,
`gpui/src/elements/div.rs:810-822`) and read the rect back in a headless
`gpui::test`. A hit-test proxy is not a substitute — clicking a point near the
pane's left edge cannot tell "box overflows past 0" from "box fits exactly at
0", and that ambiguity sent this triage down a wrong path for one iteration
before the bounds read settled it.

For the fixture half of this, see
`docs/solutions/conventions/viewport-edge-fixtures-hide-anchor-bugs.md`: a
single-pane find fixture is the same class of degenerate site as an
edge-pinned tooltip probe — it collapses "window corner" and "pane corner"
onto one coordinate, so the contract cannot observe the property it looks
like it is asserting. An overlay whose anchor is pane-relative needs at
least one multi-pane phase, focused on a pane that is not the one touching
the window corner.
