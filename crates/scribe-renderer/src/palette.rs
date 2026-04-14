use alacritty_terminal::vte::ansi::{Color, NamedColor};

/// xterm-256 RGBA colour palette.
///
/// Entries 0-7 are standard ANSI, 8-15 are bright ANSI, 16-231 form the
/// 6×6×6 colour cube, and 232-255 are a 24-step greyscale ramp.
pub struct ColorPalette {
    entries: [[f32; 4]; 256],
}

/// Convert an 8-bit sRGB component to a linear f32 in \[0, 1\].
#[inline]
fn u8_to_linear(v: u8) -> f32 {
    let s = f32::from(v) / 255.0;
    if s <= 0.04045 { s / 12.92 } else { (s + 0.055).mul_add(1.0 / 1.055, 0.0).powf(2.4) }
}

/// Build an opaque RGBA entry from three sRGB u8 components (kept in sRGB
/// space — the palette constructor linearises all entries after population).
#[inline]
const fn rgba(r: u8, g: u8, b: u8) -> [f32; 4] {
    [r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0, 1.0]
}

/// Convert a single sRGB channel to linear space.
#[inline]
fn srgb_to_linear(s: f32) -> f32 {
    if s <= 0.04045 { s / 12.92 } else { (s + 0.055).mul_add(1.0 / 1.055, 0.0).powf(2.4) }
}

/// Linearise an RGBA colour from sRGB (alpha unchanged).
fn linearise(c: &mut [f32; 4]) {
    if let Some(r) = c.get_mut(0) {
        *r = srgb_to_linear(*r);
    }
    if let Some(g) = c.get_mut(1) {
        *g = srgb_to_linear(*g);
    }
    if let Some(b) = c.get_mut(2) {
        *b = srgb_to_linear(*b);
    }
}

/// Standard ANSI colours (indices 0-15).
const ANSI_COLORS: [[f32; 4]; 16] = [
    // 0-7: standard
    rgba(0x00, 0x00, 0x00), // 0 black
    rgba(0xaa, 0x00, 0x00), // 1 red
    rgba(0x00, 0xaa, 0x00), // 2 green
    rgba(0xaa, 0x55, 0x00), // 3 yellow
    rgba(0x00, 0x00, 0xaa), // 4 blue
    rgba(0xaa, 0x00, 0xaa), // 5 magenta
    rgba(0x00, 0xaa, 0xaa), // 6 cyan
    rgba(0xaa, 0xaa, 0xaa), // 7 white
    // 8-15: bright
    rgba(0x55, 0x55, 0x55), // 8  bright black
    rgba(0xff, 0x55, 0x55), // 9  bright red
    rgba(0x55, 0xff, 0x55), // 10 bright green
    rgba(0xff, 0xff, 0x55), // 11 bright yellow
    rgba(0x55, 0x55, 0xff), // 12 bright blue
    rgba(0xff, 0x55, 0xff), // 13 bright magenta
    rgba(0x55, 0xff, 0xff), // 14 bright cyan
    rgba(0xff, 0xff, 0xff), // 15 bright white
];

/// Component intensities used in the 6×6×6 colour cube (indices 16-231).
const CUBE_INTENSITIES: [u8; 6] = [0, 95, 135, 175, 215, 255];

/// Build the colour cube entry for the given r, g, b cube coordinates (0-5).
fn cube_entry(r: usize, g: usize, b: usize) -> [f32; 4] {
    let rv = CUBE_INTENSITIES.get(r).copied().unwrap_or(0);
    let gv = CUBE_INTENSITIES.get(g).copied().unwrap_or(0);
    let bv = CUBE_INTENSITIES.get(b).copied().unwrap_or(0);
    rgba(rv, gv, bv)
}

/// Populate the 6×6×6 colour-cube region (entries 16-231) of `table`.
fn fill_cube(table: &mut [[f32; 4]; 256]) {
    let mut idx: usize = 16;
    for r in 0_usize..6 {
        for g in 0_usize..6 {
            fill_cube_row(table, &mut idx, r, g);
        }
    }
}

/// Populate one row of the colour cube (one r,g pair, all 6 b values).
fn fill_cube_row(table: &mut [[f32; 4]; 256], idx: &mut usize, r: usize, g: usize) {
    for b in 0_usize..6 {
        if let Some(slot) = table.get_mut(*idx) {
            *slot = cube_entry(r, g, b);
        }
        *idx += 1;
    }
}

/// Populate the greyscale ramp region (entries 232-255) of `table`.
fn fill_greyscale(table: &mut [[f32; 4]; 256]) {
    for i in 0_usize..24 {
        let step = u8::try_from(i).unwrap_or(u8::MAX);
        let v = 8_u8.saturating_add(step.saturating_mul(10));
        if let Some(slot) = table.get_mut(232 + i) {
            *slot = rgba(v, v, v);
        }
    }
}

impl ColorPalette {
    /// Build the default xterm-256 palette.
    pub fn new() -> Self {
        let mut entries = [[0.0_f32; 4]; 256];

        // Entries 0-15: standard + bright ANSI
        for (i, color) in ANSI_COLORS.iter().enumerate() {
            if let Some(slot) = entries.get_mut(i) {
                *slot = *color;
            }
        }

        // Entries 16-231: 6×6×6 colour cube
        fill_cube(&mut entries);

        // Entries 232-255: greyscale ramp
        fill_greyscale(&mut entries);

        // Convert all entries from sRGB to linear for the GPU pipeline.
        // The sRGB framebuffer applies the inverse transform on output.
        for entry in &mut entries {
            linearise(entry);
        }

        Self { entries }
    }

    /// Resolve an alacritty `Color` to RGBA floats `[r, g, b, a]`.
    ///
    /// Named colours that map outside the 256-entry table (e.g. `Foreground`,
    /// `Background`) fall back to opaque magenta so they remain visible.
    pub fn resolve(&self, color: Color) -> [f32; 4] {
        match color {
            Color::Named(named) => self.resolve_named(named),
            Color::Indexed(idx) => self.entry(usize::from(idx)),
            Color::Spec(rgb) => {
                [u8_to_linear(rgb.r), u8_to_linear(rgb.g), u8_to_linear(rgb.b), 1.0]
            }
        }
    }

    /// Resolve a `NamedColor` to the corresponding palette entry.
    fn resolve_named(&self, named: NamedColor) -> [f32; 4] {
        // Dim variants share the same palette index as their non-dim counterparts;
        // callers may reduce brightness separately.
        let idx: Option<usize> = match named {
            NamedColor::Black | NamedColor::DimBlack => Some(0),
            NamedColor::Red | NamedColor::DimRed => Some(1),
            NamedColor::Green | NamedColor::DimGreen => Some(2),
            NamedColor::Yellow | NamedColor::DimYellow => Some(3),
            NamedColor::Blue | NamedColor::DimBlue => Some(4),
            NamedColor::Magenta | NamedColor::DimMagenta => Some(5),
            NamedColor::Cyan | NamedColor::DimCyan => Some(6),
            NamedColor::White | NamedColor::DimWhite => Some(7),
            NamedColor::BrightBlack => Some(8),
            NamedColor::BrightRed => Some(9),
            NamedColor::BrightGreen => Some(10),
            NamedColor::BrightYellow => Some(11),
            NamedColor::BrightBlue => Some(12),
            NamedColor::BrightMagenta => Some(13),
            NamedColor::BrightCyan => Some(14),
            NamedColor::BrightWhite => Some(15),
            // These live outside the 256-entry indexed palette.
            NamedColor::Foreground
            | NamedColor::Background
            | NamedColor::Cursor
            | NamedColor::BrightForeground
            | NamedColor::DimForeground => None,
        };

        idx.map_or(Self::fallback(), |i| self.entry(i))
    }

    /// Look up a palette entry by index (0-255).
    fn entry(&self, idx: usize) -> [f32; 4] {
        self.entries.get(idx).copied().unwrap_or_else(Self::fallback)
    }

    /// Override ANSI colors 0-15 with theme values.
    pub fn override_ansi(&mut self, colors: &[[f32; 4]; 16]) {
        if let Some(entries) = self.entries.get_mut(..16) {
            entries.copy_from_slice(colors);
        }
    }

    /// Opaque magenta — used as an unmistakeable "missing colour" sentinel.
    fn fallback() -> [f32; 4] {
        [1.0, 0.0, 1.0, 1.0]
    }
}

impl Default for ColorPalette {
    fn default() -> Self {
        Self::new()
    }
}
