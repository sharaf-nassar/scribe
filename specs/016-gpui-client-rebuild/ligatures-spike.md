# Terminal Ligatures Capability Spike

GPUI v1.12.0 preserves multi-column terminal ligatures when a terminal run is
shaped with its grid cell width forced through `shape_line`.

## Decision

Keep the `appearance.ligatures` configuration key. The GPUI terminal port will
pass `Some(dimensions.cell_width)` to `Window::text_system().shape_line` for
each same-style terminal run and select `FontFeatures::disable_ligatures()`
only when that setting is false.

The forced width fixes cell origins after shaping; it does not replace
OpenType shaping. A ligature glyph retains its multi-cell outline while its
run stays anchored to the terminal grid, so its following cell begins at the
next grid column instead of drifting with the font's natural advances.

## Demo

The pinned Zed source contains the executable terminal-rendering shape used
for the Scribe scaffold. `BatchedTextRun` groups adjacent equal-style terminal
cells, then paints a ligature-bearing run with the terminal cell width forced:

```rust
window
    .text_system()
    .shape_line(
        self.text.clone().into(),
        self.font_size.to_pixels(window.rem_size()),
        std::slice::from_ref(&self.style),
        Some(dimensions.cell_width),
    )
    .paint(
        pos,
        dimensions.line_height,
        gpui::TextAlign::Left,
        None,
        window,
        cx,
    )
    .log_err();
```

Use a JetBrains Mono terminal row such as `// => !=` beside a background grid
of `dimensions.cell_width` columns. With ligatures enabled, the combined
glyphs span their source cells; the next glyph's origin remains the expected
cell boundary. Disabling `calt` removes the combined glyphs without changing
those origins.

## Evidence

The probe was checked against Zed tag `v1.12.0`
(`f96212f2c50f54d93712fa130d6226b1ce7d76b5`), the GPUI pin named by this
rebuild. `crates/terminal_view/src/terminal_element.rs` uses the demo above
in its production `BatchedTextRun::paint` path. GPUI's
`apply_force_width_to_layout` preserves a zero-advance glyph's offset from its
base glyph and assigns each advancing glyph to the next forced-width cell.
That is the required behavior for a contextual multi-cell ligature: its
outline remains intact while later terminal cells remain aligned.

GPUI also exposes `FontFeatures::disable_ligatures()`, which turns off the
`calt` OpenType feature. This provides the direct implementation of the
existing boolean setting rather than requiring a renderer-side workaround.

## Verification

The source probe verifies the exact GPUI API and terminal call site at the
pinned revision. It is intentionally documented rather than added as a Scribe
crate because the GPUI scaffold and its pinned dependencies are owned by the
separate scaffold spike; this task must not introduce a second client or move
the dependency pin.
