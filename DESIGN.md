---
name: Scribe Settings — Console
description: Swiss-minimal dark instrument for the GPUI settings window only.
colors:
  ground: "#0c0d0f"           # the one surface — no cards, no tiles, no panels
  raised: "#17181c"           # anchored menus only; the sole lifted surface
  frame-edge: "#ffffff14"     # 8% white — window edge
  hairline: "#ffffff0f"       # 6% white — the only rule in the system
  wash: "#ffffff06"           # row and nav hover
  wash-active: "#ffffff0a"    # selected nav row
  text: "#edeef1"             # primary text AND every "on" state
  dim: "#9aa0a8"              # data values — 7.1:1
  quiet: "#7d838d"            # every secondary text role — 4.6:1, clears AA
  glyph: "#666c75"            # non-text marks only — 3.7:1, clears 1.4.11
  live: "#6e8bff"             # focus ring, capture state, selected-option mark
  error: "#ff7a70"            # validation failure and the close-button hover
  knob-off: "#8d94a2"         # OFF toggle knob
typography:
  title:
    fontFamily: "IBM Plex Sans, system-ui, sans-serif"
    fontSize: "17px"
    fontWeight: 500
    lineHeight: 1.3
    letterSpacing: "-0.015em"
  label:
    fontFamily: "IBM Plex Sans, system-ui, sans-serif"
    fontSize: "13px"
    fontWeight: 500
    lineHeight: 1.35
    letterSpacing: "-0.005em"
  body:
    fontFamily: "IBM Plex Sans, system-ui, sans-serif"
    fontSize: "12px"
    fontWeight: 400
    lineHeight: 1.5
  section:
    fontFamily: "IBM Plex Sans, system-ui, sans-serif"
    fontSize: "10px"
    fontWeight: 500
    lineHeight: 1
    letterSpacing: "0.14em"
  data:
    fontFamily: "JetBrains Mono, ui-monospace, monospace"
    fontSize: "13px"
    fontWeight: 400
    lineHeight: 1
  data-micro:
    fontFamily: "JetBrains Mono, ui-monospace, monospace"
    fontSize: "11px"
    fontWeight: 400
    lineHeight: 1
rounded:
  xs: "3px"
  sm: "4px"
  md: "5px"
  lg: "6px"
  window: "8px"
spacing:
  row: "52px"
  gutter: "34px"
  measure: "660px"
  sidebar: "212px"
  section-lead: "42px"
components:
  row:
    height: "{spacing.row}"
    padding: "11px 12px"
    rounded: "{rounded.sm}"
    typography: "{typography.label}"
  row-hover:
    backgroundColor: "{colors.wash}"
  section-label:
    textColor: "{colors.quiet}"
    typography: "{typography.section}"
    padding: "42px 0 4px"
  field:
    backgroundColor: "transparent"
    textColor: "{colors.text}"
    typography: "{typography.data}"
    padding: "6px 0"
  field-focus:
    textColor: "{colors.text}"
    rounded: "{rounded.md}"
  toggle:
    backgroundColor: "#ffffff17"
    size: "32x18"
    rounded: "9px"
  toggle-on:
    backgroundColor: "{colors.text}"
    size: "32x18"
    rounded: "9px"
  option:
    textColor: "{colors.quiet}"
    typography: "{typography.label}"
    padding: "2px 0"
  option-selected:
    textColor: "{colors.text}"
  action:
    textColor: "{colors.text}"
    typography: "{typography.label}"
    padding: "0 0 2px"
  nav-row:
    textColor: "{colors.quiet}"
    height: "28px"
    padding: "0 6px"
    rounded: "{rounded.sm}"
  nav-row-active:
    backgroundColor: "{colors.wash-active}"
    textColor: "{colors.text}"
  menu:
    backgroundColor: "{colors.raised}"
    rounded: "{rounded.lg}"
    padding: "5px"
  acknowledgment:
    textColor: "{colors.quiet}"
    typography: "{typography.body}"
---

# Design System: Scribe Settings — "Console"

## Overview

**Creative North Star: "Console."** The settings window is an instrument, not a
document and not a dashboard. One ground, one hairline, one accent. Hierarchy
comes from type scale, spacing, and restraint — never from containers. Nothing
is drawn that isn't information: no cards, no tiles, no boxed controls, no
standing status bars, no labels that restate an invariant.

**Scope boundary:** these tokens govern the SETTINGS WINDOW only. This is
native GPUI (Rust) — no CSS; the Rust symbols in
`crates/scribe-client/src/settings/window.rs` are the canonical tokens, and
`SettingsColors::resolve` ignores its config argument so the surface stays a
stable instrument while the user edits themes. The terminal
window derives its chrome from the active terminal theme; never apply this
palette to the terminal surface. The palette stays fixed and independent of the
active terminal theme so settings remains a stable instrument while the user
edits themes.

**Mode: Operate.** Scanability, keyboard traversal, and task speed outrank
expression. Brand lives in precision — the alignment of a number column, the
weight of one hairline, the fact that the value type is the same face the user
is configuring.

## Colors

Near-monochrome on a single deep-neutral ground. The interface is grayscale;
color means something happened.

**The One-Accent Rule.** `#6e8bff` appears only as live state: the keyboard
focus ring, an in-progress shortcut capture, and the mark on a selected menu
option. It is never spent on idle chrome, never on a resting button, never as
decoration.

**The White-Is-On Rule.** Active state is `text` white, not accent — the ON
toggle track, the selected inline option, the current nav row. This keeps the
accent rare enough to mean something and keeps state legible for anyone who
cannot separate the accent hue from the ground.

**The Quiet/Glyph Split.** `quiet` (#7d838d, 4.6:1) carries every secondary
*text* role: section labels, nav group labels, unselected options, units,
captions, placeholders, hex labels. `glyph` (#666c75, 3.7:1) is for non-text
marks only — chevrons, arrows, the search magnifier, window controls. Never
put text in `glyph`; never put a mark in a text tone just to brighten it.

**The Error Channel.** `#ff7a70` marks validation failure and the close-button
hover. It never coexists with the accent on the same control.

**Gating mutes with color, never with opacity.** A gated row drops to `quiet`
and appends `· off`; it never dims below AA, because the description is the
only thing explaining why the row is unavailable.

## Typography

Two faces, six roles. **IBM Plex Sans** for interface, **JetBrains Mono** for
data — the same face the user is configuring in the Font family field, which is
the point.

Roles: 17px/500 page title · 13px/500 row label · 12px/400 description ·
10px/500 uppercase 0.14em section label · 13px mono value · 11px mono micro-data
(units, dates, hex, version). Nothing smaller than 10px, and nothing below 11px
carries information a user must read to complete a task.

**The Monospace-Is-Data Rule.** Monospace only for real data: values, hex,
keybinding chords, dates, versions, paths. Never as a costume for "technical."

## Layout

Titlebar 44px: window title left, window controls right, nothing between them.
Sidebar 212px: 30px search field, 28px nav rows, 10px group labels with 26px of
air above, mono version footer. Content: 34px gutters, a 660px measure centered
in the pane, page title with 28px above it.

Rows are 52px minimum, growing for descriptions and for error states, separated
by a single hairline. The value column right-aligns on a common edge that every
control type shares — that shared edge is the spine of the page. Section labels
carry 42px above and 4px below: far more above than below, so a section reads as
belonging to what follows it.

**Steppers hang in the gutter.** The `−`/`+` glyphs are positioned outside the
value column so the number column never shifts when they appear on hover. They
consume gutter space and are verified against the 1040px window minimum.

Window minimum 1040×720.

## Elevation & Depth

Flat. There is exactly one lifted surface in the system — the anchored choice
menu, which uses `raised` plus a hairline and a soft shadow. Everything else is
the ground. Hover is a white-alpha wash, not a raised plane. No card ever
appears; nested surfaces are always wrong here.

## Shapes

Radii: 3px (swatches, chips), 4px (rows, nav rows), 5px (search, controls),
6px (menus), 8px (window), fully rounded (toggle and knob only).

**The No-Container Rule.** Groups are made by whitespace and one hairline
between rows. No boxes around sections, no outlines around fields at rest, no
tracks around segmented options. A control gets an outline only while it is
being touched.

## Components

- **Nav row** 28px: quiet text; active = `wash-active` + white text. No icons — the label is the affordance.
- **Section label**: 10px uppercase in quiet, 42px of air above.
- **Control row** 52px: 13px label (12px quiet description below) left, control right-aligned on the shared spine. Long labels wrap; they never push the control off the edge.
- **Toggle** 32×18: OFF = 9% white track with a `knob-off` knob; ON = white track, ground-colored knob. Hit area extends to 40×28 without changing the visual.
- **Inline options**: text separated by 14px, unselected in quiet, selected in white with a 1px underline. `role="radiogroup"` with a group name — they are exclusive, and assistive tech has to know that.
- **Stepper**: mono value holding the column, `−`/`+` at 50% opacity in the gutter, full on row hover, invisible at a bound. The number is also the entry point: activating it opens exact entry in the same inline field every other typed value uses, seeded with the current number selected. Enter or moving focus away commits a finite in-range value at the shown precision and closes the field; Escape closes it on the saved number. A rejected value stays in the open field with the reason on the row's second line, and the setting does not change.
- **Field / hex**: transparent at rest, hairline underline on hover, accent underline plus a 3px accent glow on focus. Hex adds a 14px swatch immediately left of the value. The visible row label IS the field's `<label for>`.
- **Choice**: value text plus a chevron, no box; anchored menu on `raised`, 28px rows, selected option marked with a 5px accent dot.
- **Action**: text with a hairline under it; the rule brightens on hover. No filled buttons except a genuine primary.
- **Acknowledgment**: the word `Saved` appears beside the control that changed and fades after 1.9s. A timer owns removal, so reduced motion loses the fade and never the disappearance.
- **Error**: red hairline under the field plus the message on its own line under the value, `role="alert"` and bound with `aria-describedby`. It persists until fixed.
- **Empty search**: names the query, says the match may live on another page, suggests a shorter word.

## Do's and Don'ts

- **Do** put success where the cause was, transiently; put failure inline, persistently.
- **Do** keep one shared right edge for every control type on a page.
- **Do** extend hit areas past the visual bounds to clear 24×24 rather than inflating the visuals.
- **Don't** add a standing status bar, a config path, or a "live apply" label. Live apply is the contract, not a caption; the path belongs in a command, not in chrome.
- **Don't** wrap groups in cards, tiles, or outlines — whitespace and one hairline.
- **Don't** spend the accent on anything that isn't live state, and don't use white for anything that isn't "on."
- **Don't** convey a gated or disabled state with opacity that drops text below 4.5:1.
- **Don't** let a reduced-motion branch remove an outcome; it removes motion only.
