---
name: Scribe Settings — Typeset Ink
description: Fixed dark documentation-grammar system for the GPUI settings window only.
colors:
  ink-ground: "#141518"        # SettingsColors.page_bg — the one ground everywhere
  frame-matte: "#0b0c0e"       # frame_bg — resize-gutter window frame
  control-raised: "#1d1f24"    # control_bg — choices, steppers, buttons
  control-hover: "#262931"
  control-pressed: "#191b1f"
  input-engraved: "#101114"    # input_bg — search, paths, hex fields
  menu-elevated: "#22242a"     # menu_bg — anchored choice menu
  status-ground: "#1a1b1f"     # status_bg — pinned status line
  text-warm-white: "#e9e8e4"
  text-dim: "#a6a5a0"          # secondary: summaries, read-only values
  text-quiet: "#83827b"        # captions, group labels, placeholders, glyphs
  amber-live: "#f5b83a"        # accent — live state ONLY
  amber-hover: "#ffc85c"       # ON-toggle hover (inline literal)
  amber-pressed: "#dfa42f"     # ON-toggle pressed (inline literal)
  error-ink: "#e0584c"         # validation failure ink, never amber
  close-hover-red: "#c83030"   # close-button hover fill (inline literal)
  hairline: "#ffffff14"        # 8% white — seams and control outlines
  hairline-strong: "#ffffff1f" # 12% white — emphasis outlines, window edge
  wash-nav-active: "#ffffff0f"
  wash-nav-hover: "#ffffff08"
  wash-row-hover: "#ffffff06"
typography:
  title: { fontSize: "20px", fontWeight: 700 }
  body: { fontSize: "14px", fontWeight: 400 }
  caption: { fontSize: "12px", fontWeight: 600 }
  data: { fontFamily: "monospace", fontSize: "14px" }
---

# Design System: Scribe Settings — "Typeset Ink"

## Overview

**Creative North Star: "Typeset Ink."** Settings reads like impeccably typeset
technical documentation — the man-page tradition — refusing boxed line-soup
settings chrome. One unified deep-ink ground; hierarchy from type scale and
spacing, not containers; hairline seams only at structural boundaries; amber
spent only on live state.

**Scope boundary:** these tokens govern the SETTINGS WINDOW only. The terminal
window derives its chrome from the active terminal theme; never apply this
palette to the terminal surface. The palette is deliberately fixed
(`SettingsColors::resolve` in `crates/scribe-client/src/settings/window.rs`
ignores its config argument) so settings stays a stable instrument while the
user edits themes. This is native GPUI (Rust) — no CSS; the Rust symbols are
the canonical tokens.

## Colors

Warm off-white ink on deep neutral ground; all hovers are white-alpha washes
so they read identically over every surface.

**The Amber-Is-Live Rule.** `#f5b83a` appears only as live state: the selected
page's glyph, focus rings, the ON toggle fill, the input caret, the selected
menu option, and the status-line dot. Idle actions stay neutral.

**The Error-Ink Rule.** Validation failures use `#e0584c` — a separate channel
from amber, so a rejected edit never scans like live state. Field errors render
in it at 12px; the status line stays neutral text and speaks product language
("Saved Ligatures."), never dotted config keys.

## Typography

GPUI scale: 12px (`text_xs`) captions, section heads, mono notes; 14px
(`text_sm`) everything else; 20px (`text_xl`) bold page title. Weights 400/500
(row labels, buttons, selected nav)/600 (section heads, stepper glyphs)/700
(title). Icon glyphs come from Symbols Nerd Font Mono at 16px (`text_base`).

**The Monospace-Is-Data Rule.** Monospace only for real data: paths, hex
values, keybinding combos, stepper numbers, the `config.toml · live apply`
corner note, the Ctrl+K hint, and the `Scribe v<version>` footer. Never
decorative.

## Layout

Titlebar 38px (`SETTINGS_TITLEBAR_HEIGHT`), title centered between a 120px
traffic-light reservation and three 40px window controls. Sidebar 232px: 30px
search field, 32px nav rows (8px margin, 12px pad, 5px radius), 28px group
labels, 36px mono footer. Content: 44px gutters, every row capped at an 840px
measure, left-aligned. Rows are 46px, 62px with a description, separated by
rhythm alone. The value column is a fixed right-aligned 300px
(`VALUE_COLUMN_WIDTH`). Section heads carry 34px air above, 10px below —
more above than below. Window minimum 1040×720; 6px resize gutter
(`RESIZE_GUTTER`) painted in frame-matte.

## Elevation & Depth

Flat. Depth is tonal (engraved inputs below ground, raised controls above,
elevated menu). The single shadow in the system is `shadow_lg` on the anchored
choice menu; nothing else casts one.

## Shapes

Radii: 4px (swatch, dismiss), 5px (controls, inputs, nav rows), 6px (hover
wash pills, menus, list rows), fully rounded (toggle, knob, status dot).
**The No-Row-Rules Rule.** Rows carry no rules or boxes; the only hairlines
are the titlebar seam, the sidebar seam, the status-line seam, and control
outlines. Hover washes a row in a faint pill bleeding 12px past the text
margin (`mx(-12)`/`px(12)`).

## Components

- **Nav row** 32px: dim text, quiet glyph; selected = neutral wash + amber glyph + medium weight; focus = amber hairline outline.
- **Section head**: 12px semibold UPPERCASE in quiet ink (`heading_label`).
- **Control row** 46/62px: medium 14px label (12px dim description below) left, control right-aligned in the 300px column; gated rows dim to 0.42 opacity (`GATED_OPACITY`) and show a read-only value.
- **Toggle** 38×22 fully rounded, 16px white knob; ON = amber fill (hover `#ffc85c`, pressed `#dfa42f`); OFF = raised fill, strong hairline. Focus ring is WHITE (text ink), not amber — the ON track is already amber.
- **Choice** 240×30 (`CHOICE_WIDTH`) raised button with chevron; anchored menu same width, 30px rows (`CHOICE_OPTION_HEIGHT`), max 360px then scrolls, opens with the live value in view; selected option = amber text + amber check.
- **Stepper** 152×30: 36px −/+ ends around a centered mono value; a button at its bound loses its click handler and tab stop, dims to 0.55, and its accessible label names the limit.
- **Text input / color editor** 30px tall on the engraved inset, mono, 5px radius; focus = amber border + 2×14 amber caret; placeholder in quiet ink yields to the caret when focused-empty. Color editor adds a 16px swatch (4px radius); errors in error ink above the field.
- **Action button** 30px, 12px pad: raised neutral outline, medium text — no accent at rest.
- **Read-only value**: plain right-aligned dim monospace; the missing control outline is the read-only mark, AccessKit says "Read-only value".
- **Status line**: pinned below the scroller on status-ground with a top hairline, 6px amber dot, neutral text, 22px dismiss.
- **Window controls** 40px wide, quiet glyphs; close hovers `#c83030`, others control-hover.

## Do's and Don'ts

- **Do** keep every edit live-apply; the `config.toml · live apply` corner note is the contract (omitted only on Releases).
- **Do** clear keyboard-focus styling on pointer use; focus rings mark keyboard traversal only.
- **Don't** derive any settings color from the active terminal theme.
- **Don't** add rules, boxes, or cards between rows — rhythm and washes only.
- **Don't** spend amber on idle chrome, or error red on anything but validation failure and the close hover.
