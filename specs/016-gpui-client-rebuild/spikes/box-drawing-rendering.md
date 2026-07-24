# GPUI box-drawing rendering spike

GPUI's pinned text API has no public custom glyph-provider hook, but its
public `Window::paint_quad` API supports a procedural terminal-cell overlay.

## Demo

Run the standalone demo from the repository root:

```bash
cargo run --manifest-path tools/gpui-box-drawing-spike/Cargo.toml
```

It opens a GPUI window containing representative U+2500–U+259F glyphs. Every
visible shape is emitted as paint quads from the alpha mask produced by the
existing procedural rasterizer; the demo does not render these glyphs through
the text system. The demo uses the planned GPUI pin,
`f96212f2c50f54d93712fa130d6226b1ce7d76b5`.

## Decision

Use a paint-quad overlay in `TerminalElement`, keyed by
`is_box_drawing(cell.c)`. The terminal painter must skip text shaping for
those cells, call the procedural rasterizer with the physical cell size, and
paint the resulting alpha-mask runs after backgrounds and before text glyphs.

The overlay is selected over a custom glyph provider because the pinned GPUI
text-system API exposes shaping and painting, but does not expose a public
hook to supply an application-owned glyph raster. The overlay keeps Scribe's
existing geometry, including edge-to-edge blocks and shade alpha, independent
of installed font coverage.

## US3 impact

US3 retains procedural coverage for U+2500–U+259F. Phase B must preserve the
existing rasterizer's output and route these codepoints to the overlay; it
must not rely on a terminal font for them. Normal text remains on GPUI's
`shape_line` path, so this decision does not constrain the separate ligature
or fallback-ordering spikes.
