#!/usr/bin/env python3
"""Regenerate the embedded Symbols Nerd Font with a U+006D cmap entry.

GPUI's `CosmicTextSystem::load_family` (gpui rev f96212f,
crates/gpui_wgpu/src/cosmic_text_system.rs) evicts any font face whose
cmap does not map 'm' to a nonzero glyph, so a stock symbols-only Nerd
Font can never enter the user font-fallback chain: the face is removed
from the cosmic-text database outright. This script adds a U+006D
mapping (aliased to the font's U+E0B0 powerline glyph, or the first
mapped glyph as a fallback) so the face passes that check while leaving
every symbol glyph untouched. 'm' itself is never shaped from this font
in practice because the fallback chain is only consulted for codepoints
the primary terminal font does not cover.

Usage:
    uv run --with fonttools tools/patch-nerd-symbols-font.py \
        <input>.ttf <output>.ttf

The committed asset at
crates/scribe-client/assets/fonts/SymbolsNerdFontMono-Regular-scribe.ttf
was produced from the upstream SymbolsNerdFontMono-Regular.ttf
(https://github.com/ryanoasis/nerd-fonts, MIT licensed).
"""

import sys

from fontTools.ttLib import TTFont


def main() -> None:
    if len(sys.argv) != 3:
        raise SystemExit(f"usage: {sys.argv[0]} <input>.ttf <output>.ttf")
    src, dst = sys.argv[1], sys.argv[2]

    font = TTFont(src)
    best = font.getBestCmap()
    if 0x6D in best:
        print(f"{src}: already maps U+006D -> {best[0x6D]}; copying as-is")
    else:
        target = best.get(0xE0B0) or next(iter(best.values()))
        patched = 0
        for table in font["cmap"].tables:
            if table.isUnicode():
                table.cmap[0x6D] = target
                patched += 1
        if patched == 0:
            raise SystemExit(f"{src}: no unicode cmap subtable found")
        print(f"{src}: mapped U+006D -> {target} in {patched} subtable(s)")

    # Mark the binary as locally patched without renaming the family, so
    # the fallback chain still resolves it by its upstream family name.
    name = font["name"]
    for name_id in (3, 5):
        for record in name.names:
            if record.nameID == name_id:
                text = record.toUnicode()
                if "scribe" not in text:
                    record.string = f"{text}; scribe cmap patch"

    font.save(dst)
    print(f"wrote {dst}")


if __name__ == "__main__":
    main()
