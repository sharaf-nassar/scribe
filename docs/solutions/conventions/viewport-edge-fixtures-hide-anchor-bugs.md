---
title: Viewport-edge visual fixtures hide overlay anchor bugs
date: 2026-08-18
last_updated: 2026-08-18
component: tests/e2e/visual, scribe-client overlays
tags: [e2e, visual-tests, gpui, anchored, tooltip, test-oracle, degenerate-fixture]
problem_type: convention
---

## Problem

The Beads board card tooltip renders left-aligned to its card instead of
centred on it, so a 480px reveal over a 178px card runs across two neighbouring
lanes. The shipped visual contract at
`tests/e2e/visual/beads-board.sh:428-455` hovers a card and measures the
tooltip's exact bounds — and passed with the bug present, in the same run that
also asserted the box is "bounded, wrapped and viewport-safe".

## Root cause

The assertion is sited on the lane-4 (Done) card, which the fixture places
against the right viewport edge on purpose, to prove the clamp. GPUI's
`anchored()` with `snap_to_window_with_margin` shifts an overflowing box inward
by whatever it overflows
(`crates/gpui/src/elements/anchored.rs`, the `desired.right() > limits.right()`
branch in `prepaint`). For that card the tooltip overflows under either
placement, so the snap drives it to the same `x` regardless of the anchor
corner. The measurement is real; the *property* is unobservable at that site.

The surviving assertions only check horizontal overlap with the card
(`TOOLTIP_X < card_right && TOOLTIP_X + TOOLTIP_W > card_left`), which a
left-anchored box satisfies trivially.

Measured on clean main at `2c6936a`, with the long-title fixture moved to the
lane-2 card and the probe retargeted to lane 2:

    PROBE win=1008 card_left=412 card_w=178 card_center=501
          tip_x=414 tip_w=480 tip_center=654

Tooltip left tracks the card's left; the centre is 153px off. The same probe on
the shipped lane-4 site reports `480x48+524+19` — the clamped value, identical
for both placements.

## What didn't work

- Running the shipped `SCRIBE_E2E_TOOLTIP_ONLY=1` path. It reproduces the
  screenshot geometry faithfully and still cannot distinguish the two
  placements; the numbers look correct because the clamp normalised them.
- Retargeting the probe to a middle lane without also lengthening that card's
  title. At the fixture's natural ~170px tooltip width the two placements
  differ by about 4px, inside the trim/border noise of an ImageMagick
  difference-and-trim measurement.

## Fix

Anchor the popup's bottom-centre to the card's horizontal centre
(`Anchor::BottomCenter` at `self.anchor.center().x`) in
`crates/scribe-client/src/beads_board.rs:1008-1011`; `from_anchor_and_size`
handles `BottomCenter` as `origin.x - size.width / 2`, and the existing
`snap_to_window_with_margin(px(4.0))` still supplies the edge behaviour. Filed
as `scribe-6yoe`, unlanded as of this writing.

The durable part is the test siting: the regression assertion has to hover a
card that is *not* against a viewport edge and *does* carry a max-width title,
and the existing edge probe stays as the separate clamp proof. Two sites, two
properties.

## Prevention

When a visual assertion measures an overlay's position, check whether the
fixture site lets the clamp, the snap, or a max-width cap decide the number
being asserted. An edge-pinned anchor, a box wider than the viewport, or a
saturated max-width all collapse distinct layout rules onto one coordinate, and
the assertion then pins the clamp rather than the rule it was written for.

Alignment and clamping are separate properties and need separate fixture sites:
a centred site where the box fits with room on both sides, and an edge site
where it cannot. A single site can only ever prove one of them.

To measure without dirtying the repo, copy `tests/e2e/` to scratch, patch the
fixture and the probe there, and mount the copy — the visual harness only needs
`-v <scratch>/e2e:/tests:ro -v <scratch>/out:/output` against the prebuilt
`scribe-test-visual` image.

## Second instance: single-pane fixtures and pane-relative anchors

The same collapse hit `tests/e2e/visual/find-overlay.sh`, which is single-pane
end to end. The find box is supposed to anchor to the focused pane and in fact
anchors to the window, but in a one-pane window those two corners differ only
by a chrome band, so every shipped phase passed with the bug present. The
property became observable only after splitting the pane and focusing the half
*away* from the window corner. See
`docs/solutions/runtime-errors/gpui-overlay-mount-point-decides-anchor.md`.

Generalised: a fixture with one of something cannot distinguish "anchored to
the container" from "anchored to that one instance". Pane-, region-, and
tab-relative anchors all need a fixture with at least two.
