//! Self-contained terminal fonts registered before GPUI resolves any family.
//!
//! The primary `JetBrains Mono` faces ship in the binary, not in an installer
//! dependency or the developer's font directory. A missing configured family
//! is resolved to this known terminal face before GPUI can substitute a UI font.
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

/// The primary family shipped with every client, including raw binaries.
pub const TERMINAL_FONT_FAMILY: &str = "JetBrains Mono";

const TERMINAL_FONTS: [&[u8]; 4] = [
    include_bytes!("../assets/fonts/jetbrains-mono/JetBrainsMono-Regular.ttf"),
    include_bytes!("../assets/fonts/jetbrains-mono/JetBrainsMono-Bold.ttf"),
    include_bytes!("../assets/fonts/jetbrains-mono/JetBrainsMono-Italic.ttf"),
    include_bytes!("../assets/fonts/jetbrains-mono/JetBrainsMono-BoldItalic.ttf"),
];

/// `Symbols Nerd Font Mono` (MIT, <https://github.com/ryanoasis/nerd-fonts>)
/// with the `U+006D` cmap alias applied by `tools/patch-nerd-symbols-font.py`.
/// Its family name is unchanged, so the [`GridFont`] fallback chain resolves
/// this embedded copy by the same name users know from the legacy client.
///
/// [`GridFont`]: crate::terminal_element::GridFont
pub const SYMBOLS_NERD_FONT_MONO: &[u8] =
    include_bytes!("../assets/fonts/SymbolsNerdFontMono-Regular-scribe.ttf");

/// Register the primary and symbol faces before the first family is resolved.
///
/// GPUI caches family misses. Runtime installation after the first frame is
/// too late, so both the terminal and settings startup paths call this first.
pub fn register_embedded_fonts(cx: &App) {
    let fonts = TERMINAL_FONTS
        .into_iter()
        .chain(std::iter::once(SYMBOLS_NERD_FONT_MONO))
        .map(Cow::Borrowed)
        .collect();
    if let Err(error) = cx.text_system().add_fonts(fonts) {
        tracing::error!("failed to register bundled terminal fonts: {error:#}");
    }
}

/// Resolve a configured family without letting a missing name become UI text.
///
/// Called only at window creation and font/zoom reload, never per row or frame.
/// The saved configuration is untouched, so an unavailable user font can still
/// be selected after installation and a client restart. Preserve the platform's
/// canonical spelling when the user's name differs only in ASCII case.
pub fn terminal_font_family(requested: &str, cx: &App) -> String {
    family_or_default(requested, cx.text_system().all_font_names())
}

fn family_or_default(requested: &str, available: Vec<String>) -> String {
    if let Some(family) =
        available.into_iter().find(|family| family.eq_ignore_ascii_case(requested))
    {
        return family;
    }
    tracing::warn!(requested, fallback = TERMINAL_FONT_FAMILY, "terminal font is unavailable");
    TERMINAL_FONT_FAMILY.to_owned()
}

#[cfg(test)]
mod tests {
    use ttf_parser::Face;

    use super::{SYMBOLS_NERD_FONT_MONO, TERMINAL_FONT_FAMILY, TERMINAL_FONTS, family_or_default};

    // @lat: [[test#GPUI Client Headless Suites#Cell-accurate paint path#Bundled primary terminal font]]
    #[test]
    fn bundled_primary_faces_cover_default_text_and_styles() {
        assert_eq!(scribe_common::config::AppearanceConfig::default().font, TERMINAL_FONT_FAMILY);
        for (bytes, (weight, italic)) in
            TERMINAL_FONTS.into_iter().zip([(400, false), (700, false), (400, true), (700, true)])
        {
            let face = Face::parse(bytes, 0).expect("bundled primary face parses");
            let family = face
                .names()
                .into_iter()
                .filter(|name| name.name_id == ttf_parser::name_id::FAMILY)
                .find_map(|name| name.to_string())
                .expect("family name");
            assert_eq!(family, TERMINAL_FONT_FAMILY);
            assert_eq!(face.weight().to_number(), weight);
            assert_eq!(face.is_italic(), italic);
            assert!(face.is_monospaced());
            let advance =
                face.glyph_hor_advance(face.glyph_index('m').expect("GPUI admission glyph"));
            for ch in ' '..='~' {
                let glyph = face.glyph_index(ch).expect("printable ASCII coverage");
                assert_eq!(face.glyph_hor_advance(glyph), advance, "fixed cell width for {ch:?}");
            }
        }
    }

    // @lat: [[test#GPUI Client Headless Suites#Cell-accurate paint path#Bundled primary terminal font]]
    #[test]
    fn missing_primary_uses_bundled_font_not_a_proportional_ui_fallback() {
        let available = vec!["Noto Sans".to_owned(), TERMINAL_FONT_FAMILY.to_owned()];
        assert_eq!(
            family_or_default("uninstalled custom font", available.clone()),
            TERMINAL_FONT_FAMILY
        );
        assert_eq!(family_or_default("jetbrains mono", available.clone()), TERMINAL_FONT_FAMILY);
        assert_eq!(
            family_or_default("Noto Sans", available),
            "Noto Sans",
            "do not override an explicitly available user choice"
        );
        assert_eq!(family_or_default("missing", Vec::new()), TERMINAL_FONT_FAMILY);
    }

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
