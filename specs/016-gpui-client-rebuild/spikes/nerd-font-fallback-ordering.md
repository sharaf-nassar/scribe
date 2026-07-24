# GPUI Nerd-Font fallback ordering spike

GPUI at Scribe's pinned Zed revision can honor an explicit, ordered fallback
chain for each text run, preserving the terminal's Nerd-Font-first behavior.

## Demo

Run the standalone demo from this directory:

```bash
cargo run --manifest-path tools/gpui-font-fallback-spike/Cargo.toml
```

It creates the U+E0B0 text run with a primary monospace font and an ordered
fallback chain: `Symbols Nerd Font Mono`, then `Unifont Sample`. The demo exits
successfully only when the public GPUI `TextStyle::to_run` API preserves that
exact order on the run sent to the platform text system.

The demo uses the exact `v1.12.0` pin, commit
`f96212f2c50f54d93712fa130d6226b1ce7d76b5`.

## Decision

Use `gpui::FontFallbacks::from_fonts` for every terminal glyph run. Construct
the list in Scribe's existing order:

1. `Symbols Nerd Font Mono`
2. `Symbols Nerd Font`
3. `Nerd Font Symbols Mono`
4. `Nerd Font Symbols`
5. Existing generic sans, mono, symbol, and emoji fallbacks

Do not include `Unifont Sample`: Scribe already excludes it because its
private-use mappings can make unavailable terminal icons look like unrelated
sample symbols. GPUI's Linux `CosmicTextSystem` loads the supplied names in
order and chooses the first fallback covering each grapheme before handing the
span to cosmic-text. Its own source includes a regression test named
`falls_through_chain_in_order` for that selection rule.

## US3 impact

US3's requirement that Nerd Font glyphs resolve before generic symbol fonts
remains achievable and does not need a regression exception. The Phase B
glyph-run painter must carry the ordered `FontFallbacks` on its `TextStyle` or
`Font` for every shaped terminal run; omitting it falls back to GPUI's platform
font selection and would not preserve Scribe's ordering.
