//! Embedded fallback fonts registered with GPUI's text system at startup.
//!
//! The terminal grid's [`FONT_FALLBACKS`] chain names `Symbols Nerd Font
//! Mono` first, but GPUI's cosmic-text backend (rev `f96212f`,
//! `CosmicTextSystem::load_family`) evicts any face whose cmap does not map
//! `'m'` to a nonzero glyph — and every stock symbols-only font fails that
//! check, so the face is removed from the font database and the chain entry
//! silently drops out. Worse, cosmic-text's automatic platform fallback then
//! ranks `Unifont Sample` above the Nerd Fonts (fontconfig charset order),
//! turning every private-use icon into an unrelated hex-box sample glyph —
//! exactly the substitution the legacy renderer banned via
//! `SCRIBE_FORBIDDEN_FALLBACKS`.
//!
//! [`SYMBOLS_NERD_FONT_MONO`] is the upstream `SymbolsNerdFontMono-Regular`
//! binary with one addition: a `U+006D` cmap alias (see
//! `tools/patch-nerd-symbols-font.py`) so the face survives the eviction
//! check and the fallback chain resolves it. The alias is inert in practice:
//! the chain is only consulted for codepoints the primary terminal font does
//! not cover, and every terminal font covers `m`. Embedding the font also
//! makes Nerd Font glyphs render on hosts with no Nerd Fonts installed.
//!
//! [`FONT_FALLBACKS`]: crate::terminal_element::GridFont::fallbacks

use std::borrow::Cow;

use gpui::App;

/// `Symbols Nerd Font Mono` (MIT, <https://github.com/ryanoasis/nerd-fonts>)
/// with the `U+006D` cmap alias applied by `tools/patch-nerd-symbols-font.py`.
/// Its family name is unchanged, so the [`GridFont`] fallback chain resolves
/// this embedded copy by the same name users know from the legacy client.
///
/// [`GridFont`]: crate::terminal_element::GridFont
pub const SYMBOLS_NERD_FONT_MONO: &[u8] =
    include_bytes!("../assets/fonts/SymbolsNerdFontMono-Regular-scribe.ttf");

/// Register the embedded fallback fonts with the app's text system.
///
/// Must run before the first frame is shaped: `load_family` caches per-family
/// resolutions, so a font added after the terminal has painted once would
/// never displace the cached miss. Registration failure is logged, not fatal —
/// the grid still paints, only private-use icons degrade to tofu.
pub fn register_embedded_fonts(cx: &App) {
    if let Err(error) = cx.text_system().add_fonts(vec![Cow::Borrowed(SYMBOLS_NERD_FONT_MONO)]) {
        tracing::warn!("failed to register embedded Symbols Nerd Font Mono: {error:#}");
    }
}

#[cfg(test)]
mod tests {
    use ttf_parser::Face;

    use super::SYMBOLS_NERD_FONT_MONO;

    /// The embedded symbols font must keep the exact family name the
    /// `FONT_FALLBACKS` chain resolves, map `'m'` to a nonzero glyph so
    /// GPUI's `load_family` eviction check keeps the face, and cover the
    /// Nerd Font ranges the legacy client rendered (powerline, Font
    /// Awesome).
    // @lat: [[test#GPUI Client Headless Suites#Cell-accurate paint path#Embedded Nerd Font survives GPUI face eviction]]
    #[test]
    fn embedded_symbols_font_passes_gpui_eviction_and_covers_icons() {
        let face = Face::parse(SYMBOLS_NERD_FONT_MONO, 0).expect("embedded font parses");

        let family = face
            .names()
            .into_iter()
            .filter(|name| name.name_id == ttf_parser::name_id::FAMILY)
            .find_map(|name| name.to_string())
            .expect("family name present");
        assert_eq!(family, "Symbols Nerd Font Mono");

        // GPUI keeps a face only if `charmap().map('m') != 0`.
        assert!(face.glyph_index('m').is_some(), "U+006D must map or GPUI evicts the face");

        // Powerline and Font Awesome codepoints the legacy client shipped.
        for ch in ['\u{e0a0}', '\u{e0b0}', '\u{e0b2}', '\u{f09b}', '\u{f121}'] {
            assert!(face.glyph_index(ch).is_some(), "missing coverage for U+{:04X}", ch as u32);
        }
    }
}
